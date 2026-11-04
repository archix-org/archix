//! The in-place apply that switch and rollback share: packages, accounts, files, services and hooks, on the running root.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};

use super::apply::{self, Accounts, SystemChange};
use super::credentials::{self, Passwords};
use crate::packages::{Packages, RunsPacman};
use crate::paths::Paths;
use crate::repos::Repo;

/// The root a switch applies to: `/`, or a directory a test points ARCHIX_TEST_ROOT at, where commands run chrooted.
pub struct Root {
    path: PathBuf,
}

impl Root {
    pub fn new(path: &Path) -> Root {
        Root {
            path: path.to_path_buf(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn is_slash(&self) -> bool {
        self.path == Path::new("/")
    }

    /// Whether this is the booted system with systemd running, whose services a switch stops, starts and restarts.
    pub fn running(&self) -> bool {
        self.is_slash() && Path::new("/run/systemd/system").is_dir()
    }

    /// `program` run in this root.
    pub fn command<S: AsRef<OsStr>>(&self, program: S) -> Command {
        if self.is_slash() {
            return Command::new(program);
        }
        let mut command = Command::new("chroot");
        command.arg(&self.path).arg(program);
        command
    }

    pub fn output<S: AsRef<OsStr>>(&self, argv: &[S]) -> Result<Output> {
        let (program, args) = argv.split_first().context("no command to run")?;
        self.command(program)
            .args(args)
            .output()
            .with_context(|| format!("running {}", describe(argv)))
    }

    /// Runs `argv` and fails with its error output unless it exits 0.
    pub fn run<S: AsRef<OsStr>>(&self, argv: &[S]) -> Result<()> {
        let output = self.output(argv)?;
        if !output.status.success() {
            bail!(
                "{} failed: {}",
                describe(argv),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }
}

impl super::boot::BootRoot for Root {
    fn local_db(&self) -> PathBuf {
        // A root laid out for rollbacks keeps the database under /var/rollback, bound over /var/lib/pacman only where it boots, so not in a keep's clone.
        let rolled = self.path.join(crate::snapshot::layout::ON_ROOT).join("pacman/local");
        match rolled.is_dir() {
            true => rolled,
            false => self.path.join("var/lib/pacman/local"),
        }
    }

    fn read_path(&self, path: &Path) -> Result<PathBuf> {
        Ok(self.path.join(path.strip_prefix("/").unwrap_or(path)))
    }

    fn output(&self, argv: &[&str]) -> Result<Output> {
        Root::output(self, argv)
    }

    fn signing_output(&self, argv: &[&str]) -> Result<Output> {
        Root::output(self, argv)
    }
}

impl RunsPacman for Root {
    fn pacman(&self) -> Result<Command> {
        let mut pacman = Command::new("pacman");
        // Its own process group, so a Ctrl-C at the prompt that started the switch can't stop it halfway through a package.
        pacman.process_group(0);
        if !self.is_slash() {
            let at = |path: &str| self.path.join(path);
            pacman
                .arg("--root")
                .arg(&self.path)
                .arg("--dbpath")
                .arg(at("var/lib/pacman"))
                .arg("--config")
                .arg(at("etc/pacman.conf"))
                .arg("--gpgdir")
                .arg(at("etc/pacman.d/gnupg"));
            // pacman refuses a --hookdir that doesn't exist.
            if at("etc/pacman.d/hooks").is_dir() {
                pacman.arg("--hookdir").arg(at("etc/pacman.d/hooks"));
            }
        }
        Ok(pacman)
    }

    fn place(&self) -> String {
        self.path.display().to_string()
    }
}

fn describe<S: AsRef<OsStr>>(argv: &[S]) -> String {
    argv.iter()
        .map(|arg| arg.as_ref().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

pub struct Credentials<'a> {
    pub passwords: &'a Passwords,
    pub allow_no_password_login: bool,
    /// Users whose password a checked crypt_hash secret sets.
    pub logins: &'a BTreeSet<String>,
}

/// Everything one in-place apply needs, prepared and checked before it starts.
pub struct Plan<'a> {
    pub paths: &'a Paths,
    pub root: &'a Root,
    pub packages: &'a Packages,
    pub system: &'a SystemChange,
    pub repos: &'a [Repo],
    /// boot.esp, which fstab keeps a line for.
    pub esp: Option<&'a str>,
    /// A switch's passwords; a rollback sets none.
    pub credentials: Option<Credentials<'a>>,
    /// What packages left the files whose capabilities the current generation declares.
    pub package_capabilities: &'a BTreeMap<String, String>,
    /// Managed files that differ on disk, whose on_change scripts run.
    pub rewritten: &'a BTreeSet<String>,
    /// When the switch began, so services that restarted since aren't restarted again.
    pub started: u64,
}

/// What an apply did besides failing.
#[derive(Debug, Default)]
pub struct Applied {
    pub package_capabilities: BTreeMap<String, String>,
    /// What went wrong without stopping it: units, hooks, secrets.
    pub failures: Vec<String>,
}

/// Refuses, before anything changes, a sudoers drop-in visudo rejects or tmpfiles lines systemd-tmpfiles can't parse, checked on copies of what the switch would write.
pub fn check_files(system: &SystemChange, content: &dyn Fn(&str) -> Option<Vec<u8>>) -> Result<()> {
    let dir = Scratch::new()?;
    if system.to.files.contains_key(credentials::SUDOERS)
        && let Some(bytes) = content(credentials::SUDOERS)
    {
        let copy = dir.path().join("sudoers");
        fs::write(&copy, bytes).with_context(|| format!("writing {}", copy.display()))?;
        let output = Command::new("visudo")
            .args(["-c", "-f"])
            .arg(&copy)
            .output()
            .context("running visudo")?;
        if !output.status.success() {
            let said = String::from_utf8_lossy(&output.stderr)
                .lines()
                .next()
                .unwrap_or_default()
                .replace(&copy.display().to_string(), credentials::SUDOERS);
            bail!("visudo refuses the sudoers lines from sudo ({said}); nothing was changed");
        }
    }
    if system.to.files.contains_key(credentials::TMPFILES)
        && let Some(bytes) = content(credentials::TMPFILES)
    {
        let copy = dir.path().join("archix.conf");
        fs::write(&copy, bytes).with_context(|| format!("writing {}", copy.display()))?;
        let output = Command::new("systemd-tmpfiles")
            .args(["--dry-run", "--create"])
            .arg(&copy)
            .output()
            .context("running systemd-tmpfiles")?;
        let shown = copy.display().to_string();
        let errors = String::from_utf8_lossy(&output.stderr);
        // Users and groups the switch creates don't exist yet, so only theirs may fail to resolve.
        let created = |line: &str| {
            line.split('\'')
                .nth(1)
                .is_some_and(|name| system.to.users.contains_key(name) || system.to.groups.contains_key(name))
        };
        let unparsed: Vec<String> = errors
            .lines()
            .filter(|line| line.contains(&shown) && !(line.contains("Failed to resolve") && created(line)))
            .map(|line| line.replace(&shown, credentials::TMPFILES))
            .collect();
        if !unparsed.is_empty() {
            bail!(
                "systemd-tmpfiles refuses tmpfiles lines: {}; nothing was changed",
                unparsed.join("; ")
            );
        }
    }
    Ok(())
}

/// A directory of root's own, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Scratch> {
        let dir = std::env::temp_dir().join(format!("archix-check-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Scratch(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Refuses files the config writes into /usr, an AppImage's command or menu entry, that a package in `local_db` ships.
pub fn refuse_packaged_usr_files(system: &SystemChange, local_db: &Path) -> Result<()> {
    let ours: Vec<&str> = system
        .to
        .files
        .keys()
        .filter(|path| path.starts_with("/usr/") && !path.starts_with("/usr/local/"))
        .map(|path| &path[1..])
        .collect();
    if ours.is_empty() {
        return Ok(());
    }
    let taken: Vec<String> = crate::conflicts::owners(local_db)
        .into_iter()
        .flat_map(|(package, files)| {
            files
                .into_iter()
                .filter(|file| ours.contains(&file.as_str()))
                .map(move |file| format!("/{file} is {package}'s"))
        })
        .collect();
    if taken.is_empty() {
        return Ok(());
    }
    bail!(
        "the config's AppImages would replace files packages ship: {}; give the AppImage another name in appimages",
        taken.join(", ")
    )
}

/// The context of an apply's error when it failed before changing anything, which a switch tells apart from one part way.
#[derive(Debug)]
pub struct NothingApplied(pub &'static str);

impl std::fmt::Display for NothingApplied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} failed before anything was changed", self.0)
    }
}

/// Whether a package transaction that failed got far enough to change the system: the local database changed, or pacman's log says a transaction started after `log_before` bytes.
pub fn transaction_began(
    before: &BTreeMap<String, crate::pacman::LocalPkg>,
    after: &BTreeMap<String, crate::pacman::LocalPkg>,
    log: &[u8],
    log_before: u64,
) -> bool {
    let changed = before.len() != after.len()
        || before
            .iter()
            .any(|(name, pkg)| after.get(name).is_none_or(|now| now.version != pkg.version));
    let since = log.get(log_before as usize..).unwrap_or_default();
    changed || String::from_utf8_lossy(since).contains("[ALPM] transaction started")
}

/// The steps of an apply that finished, which an error partway names.
struct Progress(Vec<&'static str>);

impl Progress {
    fn step<T>(&mut self, name: &'static str, result: Result<T>) -> Result<T> {
        match result {
            Ok(value) => {
                self.0.push(name);
                Ok(value)
            }
            Err(e) if self.0.is_empty() => Err(e.context(NothingApplied(name))),
            Err(e) => Err(e.context(format!(
                "{name} failed after these were applied: {}; the system is part way between the two states",
                self.0.join(", ")
            ))),
        }
    }
}

/// Applies `plan` to its root in place: packages, users and passwords, files, capabilities, units and archix's own files, secrets, then on a running system the services, and last the hooks. Everything that can refuse must have refused before this is called; a failure here leaves what was applied so far.
pub fn apply(plan: &Plan) -> Result<Applied> {
    let mut progress = Progress(Vec::new());
    let paths = plan.paths;
    let root = plan.root;
    let db = paths.local_db();
    let before = crate::pacman::read_local_db(&db)?;
    let kernel = super::running::Running::new(root.path());
    crate::testhooks::fail_at("switch-apply").context(NothingApplied("the apply"))?;
    if !plan.packages.install.is_empty() || !plan.packages.remove.is_empty() {
        kernel.keep(paths, &before, plan.packages);
        let log = root.path().join("var/log/pacman.log");
        let log_before = fs::metadata(&log).map_or(0, |meta| meta.len());
        let transacted = crate::packages::transact(root, plan.packages, &db);
        if transacted.is_err()
            && transaction_began(
                &before,
                &crate::pacman::read_local_db(&db).unwrap_or_default(),
                &fs::read(&log).unwrap_or_default(),
                log_before,
            )
        {
            progress.0.push("part of the package transaction");
        }
        if root.running() {
            for failed in super::dkms::reconcile(Path::new(super::boot::MODULES)) {
                eprintln!("warning: {failed} failed, so DKMS may not remove that module's files later");
            }
        }
        kernel.restore(paths);
        progress.step("the package transaction", transacted)?;
        crate::testhooks::mark("switch-packages");
        progress.step(
            "checking the config's /usr files",
            refuse_packaged_usr_files(plan.system, &db),
        )?;
    }
    let after = crate::pacman::read_local_db(&db)?;
    let etc = root.path().join("etc");
    let existed = Accounts::read(&etc);
    let run = |argv: &[String]| root.run(argv);
    let users = progress.step("users and groups", apply::users(plan.system, &etc, &run, &paths.state))?;
    if let Some(credentials) = &plan.credentials {
        let existed = |name: &str| existed.uid(name).is_some();
        let kept = progress.step(
            "passwords",
            credentials::apply(&etc, &etc, credentials.passwords, &existed),
        )?;
        for kept in kept {
            eprintln!("warning: {kept}");
        }
        if let Some(warning) = credentials::lockout(&etc, &etc, credentials.allow_no_password_login, credentials.logins)
            .unwrap_or_else(|e| Some(format!("warning: {e:#}")))
        {
            eprintln!("{warning}");
        }
    }
    let accounts = Accounts::read(&etc);
    let with_password = match &plan.credentials {
        Some(_) => credentials::with_password(&plan.system.to.users),
        None => BTreeSet::new(),
    };
    progress.step(
        "home directories",
        apply::create_homes(&users, &accounts, &etc.join("skel"), root.path(), &with_password),
    )?;
    let mut failures = apply::disable_services(plan.system, &run);
    let on_root = |path: &str| Some(root.path().join(path.trim_start_matches('/')));
    progress.step("files", apply::files(paths, plan.system, &on_root, &accounts))?;
    crate::testhooks::mark("switch-files");
    let package_capabilities = progress.step(
        "file capabilities",
        set_capabilities(root.path(), &db, plan.system, plan.package_capabilities),
    )?;
    failures.extend(apply::enable_services(plan.system, &run));
    let archix = crate::integration::own_path()?;
    progress.step(
        "archix's own files",
        crate::integration::write_all(paths, &archix, &|path: &Path| {
            Ok(root.path().join(path.strip_prefix("/").unwrap_or(path)))
        }),
    )?;
    progress.step(
        "pacman.conf's repos",
        crate::integration::sync_pacman_conf(&etc.join("pacman.conf"), plan.repos),
    )?;
    if let Some(esp) = plan.esp {
        progress.step("fstab", super::fstab::write_in(&etc, esp, &plan.system.to.fstab))?;
    }
    progress.step(
        "the hibernation guard",
        super::fstab::hibernation_in(&etc, &plan.system.to.fstab),
    )?;
    if plan.system.to.secrets.is_some() {
        failures.extend(activate_secrets(root));
    }
    if root.running() {
        failures.extend(services(plan.system));
        let changed: BTreeSet<String> = before
            .iter()
            .filter(|(name, pkg)| after.get(*name).is_none_or(|now| now.version != pkg.version))
            .map(|(name, _)| name.clone())
            .chain(after.keys().filter(|name| !before.contains_key(*name)).cloned())
            .collect();
        if !changed.is_empty() {
            let restarts = super::restart::plan(&db, &changed, &plan.system.to.restart_if_changed, plan.started);
            failures.extend(super::restart::run(&restarts));
        }
    }
    if kernel.replaced(paths) {
        println!(
            "the running kernel was replaced: reboot to run the new one (the running one's modules stay loadable until then)"
        );
    }
    let due = super::hooks::due(plan.system, plan.rewritten);
    if !due.is_empty() || !plan.system.to.hooks.is_empty() {
        eprintln!(":: running hooks");
    }
    failures.extend(super::hooks::run(root.path(), &due, &plan.system.to.hooks));
    Ok(Applied {
        package_capabilities,
        failures,
    })
}

/// Decrypts the secrets the config declares into the ramfs now, in a child archix as the boot does; what it couldn't is a soft failure.
fn activate_secrets(root: &Root) -> Vec<String> {
    let archix = match crate::integration::own_path() {
        Ok(archix) => archix,
        Err(e) => return vec![format!("decrypting secrets: {e:#}")],
    };
    let output = Command::new(archix)
        .args(["secrets", "activate", "--root"])
        .arg(root.path())
        .env_remove("ARCHIX_TEST_ROOT")
        .output();
    match output {
        Ok(output) if output.status.success() => Vec::new(),
        Ok(output) => vec![format!(
            "decrypting secrets: {}{}",
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        )],
        Err(e) => vec![format!("decrypting secrets: {e}")],
    }
}

/// Sets the config's file capabilities under `root` after everything that writes or chowns its files, given what `base_packages` says packages left the current generation's; returns the new record.
fn set_capabilities(
    root: &Path,
    db: &Path,
    system: &SystemChange,
    base_packages: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    if system.to.capabilities.is_empty() && system.from.capabilities.is_empty() {
        return Ok(BTreeMap::new());
    }
    let owned = crate::conflicts::owned_files(db);
    let declared = system
        .to
        .files
        .keys()
        .filter(|path| path.starts_with("/usr/") || path.starts_with("/opt/"))
        .cloned()
        .collect();
    super::capabilities::apply(
        root,
        &super::capabilities::Base {
            declared: &system.from.capabilities,
            packages: base_packages,
        },
        &system.to.capabilities,
        &owned,
        &declared,
    )
}

fn systemctl(args: &[&str], failures: &mut Vec<String>) {
    let output = Command::new("systemctl").args(args).output();
    let what = format!("systemctl {}", args.join(" "));
    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => failures.push(format!("{what}: {}", String::from_utf8_lossy(&output.stderr).trim())),
        Err(e) => failures.push(format!("{what}: {e}")),
    }
}

fn active(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", unit])
        .status()
        .is_ok_and(|status| status.success())
}

/// On the running system: reloads systemd, stops the units the config turned off or dropped, starts the ones it newly enables and restarts those whose unit files changed; failures are returned.
fn services(change: &SystemChange) -> Vec<String> {
    let mut failures = Vec::new();
    systemctl(&["daemon-reload"], &mut failures);
    for (verb, unit) in unit_actions(change, &active) {
        systemctl(&[verb, &unit], &mut failures);
    }
    failures
}

/// What a switch does to units on the running system, given which are `active`: stops the disabled or dropped, starts the newly enabled, try-restarts those whose unit file it rewrote and reloads those whose RELOADS file changed.
pub fn unit_actions(change: &SystemChange, active: &dyn Fn(&str) -> bool) -> Vec<(&'static str, String)> {
    let mut actions = Vec::new();
    let off = change
        .to
        .services
        .iter()
        .filter(|(_, enabled)| !**enabled)
        .map(|(unit, _)| unit)
        .chain(
            change
                .from
                .services
                .iter()
                .filter(|(unit, enabled)| **enabled && !change.to.services.contains_key(*unit))
                .map(|(unit, _)| unit),
        );
    for unit in off {
        if active(unit) {
            actions.push(("stop", unit.clone()));
        }
    }
    let changed = change.changed_paths();
    for (unit, enabled) in &change.to.services {
        if !*enabled {
            continue;
        }
        let rewritten = changed.iter().any(|path| {
            *path == format!("/etc/systemd/system/{unit}")
                || path.starts_with(&format!("/etc/systemd/system/{unit}.d/"))
        });
        let newly = change.from.services.get(unit) != Some(&true);
        let never = change.to.restart_if_changed.get(unit) == Some(&false);
        let reread = RELOADS
            .iter()
            .any(|(reloads, path)| *reloads == unit && changed.contains(path));
        if newly && !active(unit) {
            actions.push(("start", unit.clone()));
        } else if rewritten && !never {
            actions.push(("try-restart", unit.clone()));
        } else if reread {
            actions.push(("try-reload-or-restart", unit.clone()));
        }
    }
    actions
}

/// Units archix writes whose running service reads a file the switch may change on its own: reloaded when only that file changed.
const RELOADS: &[(&str, &str)] = &[(super::firewall::UNIT, super::firewall::RULES)];

#[cfg(test)]
mod unit_tests {
    use super::*;
    use crate::system::{ManagedFile, SystemState, firewall};

    fn file(sha256: &str) -> ManagedFile {
        ManagedFile {
            sha256: sha256.to_string(),
            link: None,
            mode: 0o644,
            owner: "root".to_string(),
            group: "root".to_string(),
            merge: None,
        }
    }

    fn with_rules(sha256: &str) -> SystemState {
        let mut state = SystemState::default();
        state.files.insert(firewall::RULES.to_string(), file(sha256));
        state
            .files
            .insert(format!("/etc/systemd/system/{}", firewall::UNIT), file("unit"));
        state.services.insert(firewall::UNIT.to_string(), true);
        state
    }

    #[test]
    fn the_firewall_reloads_when_only_its_rules_change_and_not_when_nothing_does() {
        let change = SystemChange {
            from: with_rules("a"),
            to: with_rules("b"),
        };
        assert_eq!(
            unit_actions(&change, &|_| true),
            vec![("try-reload-or-restart", firewall::UNIT.to_string())]
        );
        let same = SystemChange {
            from: with_rules("a"),
            to: with_rules("a"),
        };
        assert!(unit_actions(&same, &|_| true).is_empty());
    }
}

#[cfg(test)]
mod transaction_tests {
    use super::*;

    fn db(packages: &[(&str, &str)]) -> BTreeMap<String, crate::pacman::LocalPkg> {
        packages
            .iter()
            .map(|(name, version)| {
                let pkg = crate::pacman::LocalPkg {
                    name: name.to_string(),
                    version: version.to_string(),
                    arch: "x86_64".to_string(),
                    explicit: true,
                    installed_at: None,
                };
                (name.to_string(), pkg)
            })
            .collect()
    }

    #[test]
    fn a_failed_transaction_counts_as_begun_once_the_database_or_pacmans_log_says_so() {
        let before = db(&[("a", "1"), ("b", "1")]);
        let old_log = b"[2026-11-04] [ALPM] transaction started\n[2026-11-04] [ALPM] transaction completed\n";
        let at = old_log.len() as u64;
        assert!(
            !transaction_began(&before, &before, old_log, at),
            "an earlier transaction in the log isn't this one"
        );
        let mut log = old_log.to_vec();
        log.extend_from_slice(b"[2026-11-04] [PACMAN] Running 'pacman -U'\n");
        assert!(
            !transaction_began(&before, &before, &log, at),
            "refused before its commit"
        );
        log.extend_from_slice(b"[2026-11-04] [ALPM] transaction started\n");
        assert!(
            transaction_began(&before, &before, &log, at),
            "its commit began, even with nothing in the database yet"
        );
        assert!(
            transaction_began(&before, &db(&[("a", "2"), ("b", "1")]), old_log, at),
            "a package changed"
        );
        assert!(
            transaction_began(&before, &db(&[("a", "1")]), old_log, at),
            "a package went"
        );
    }
}

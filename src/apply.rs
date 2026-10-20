use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::generations::Store;
use crate::journal::{Journal, Step, Transaction};
use crate::pacman::{LocalPkg, read_local_db};
use crate::paths::Paths;
use crate::system::running::Running;

/// Set for every pacman run archix starts, so archix's own pacman hook can tell them apart from manual ones.
pub const TRANSACTION_ENV: &str = "ARCHIX_TRANSACTION";
/// pacman's `--ask` bit that answers "remove the conflicting package?" with yes (ALPM_QUESTION_CONFLICT_PKG).
const ASK_REMOVE_CONFLICTS: &str = "--ask=4";
/// archix may have restored an old kernel's modules itself, so reinstalling that kernel may overwrite them.
const KERNEL_MODULES: &str = "/usr/lib/modules/*";

#[derive(Debug, PartialEq)]
pub enum PacmanLock {
    Free,
    Running,
    Stale,
}

pub fn pacman_lock(paths: &Paths) -> PacmanLock {
    let lock = paths.pacman_lock();
    if !lock.exists() {
        PacmanLock::Free
    } else if held_open(&lock) {
        PacmanLock::Running
    } else {
        PacmanLock::Stale
    }
}

/// Whether any process has `path` open: libalpm keeps db.lck open while it holds the lock, whichever front end (pacman, pamac, PackageKit) took it.
fn held_open(path: &Path) -> bool {
    let Ok(processes) = fs::read_dir("/proc") else {
        return true;
    };
    processes.flatten().any(|process| {
        fs::read_dir(process.path().join("fd"))
            .into_iter()
            .flatten()
            .flatten()
            .any(|fd| fs::read_link(fd.path()).is_ok_and(|target| target == path))
    })
}

/// Runs the journal's remaining steps, saving progress after each; `reinstall` redoes installs after a crash.
pub fn run(paths: &Paths, journal: &mut Journal, reinstall: bool) -> Result<Vec<String>> {
    // A closed terminal or ssh session, or a Ctrl-C, would otherwise kill pacman halfway through a transaction.
    let _held = HoldInterrupts::new(paths);
    let path = paths.journal();
    // Downloads, copies and builds the switch prepared must be on disk before anything changes, or a power cut leaves recovery with empty package files.
    let written = crate::durable::system_paths(&paths.root());
    crate::durable::sync_filesystems(&written);
    let mut soft_failures = Vec::new();
    while let Some(step) = journal.next_step() {
        eprintln!(":: {}", step.describe());
        soft_failures.extend(run_step(paths, journal, step, reinstall).with_context(|| {
            format!(
                "{} failed; the switch is unfinished, run `archix recover`",
                step.describe()
            )
        })?);
        // pacman doesn't fsync, so a step is only recorded as done once what it wrote is on disk.
        crate::durable::sync_filesystems(&written);
        journal.done.push(step);
        journal.save(&path)?;
        crate::testhooks::crash_at(&format!("after-{}", format!("{step:?}").to_lowercase()));
    }
    Journal::delete(&path)?;
    Ok(soft_failures)
}

fn run_step(paths: &Paths, journal: &Journal, step: Step, reinstall: bool) -> Result<Vec<String>> {
    let (tx, generation) = (&journal.transaction, journal.generation);
    let system = tx.system.as_ref();
    match step {
        Step::Restart if tx.at_boot => Ok(Vec::new()),
        Step::Restart => {
            let changed: BTreeSet<String> = journal
                .before
                .keys()
                .chain(tx.versions.keys())
                .filter(|name| journal.before.get(*name).map(|pkg| &pkg.version) != tx.versions.get(*name))
                .cloned()
                .collect();
            let overrides = system
                .map(|change| change.to.restart_if_changed.clone())
                .unwrap_or_default();
            Ok(crate::system::restart::run(&crate::system::restart::plan(
                &paths.local_db(),
                &changed,
                &overrides,
                journal.started,
            )))
        }
        Step::Users => system
            .map_or(Ok(()), |change| crate::system::apply::users(change, &paths.state))
            .map(|()| Vec::new()),
        Step::Files => system
            .map_or(Ok(()), |change| crate::system::apply::files(paths, change))
            .map(|()| Vec::new()),
        Step::Reload => Ok(system.map_or_else(Vec::new, crate::system::apply::reload)),
        Step::StopServices => Ok(system.map_or_else(Vec::new, |change| {
            let mut failures = crate::system::apply::stop_services(change, tx.at_boot);
            failures.extend(crate::system::apply::remove_users(change));
            failures
        })),
        Step::Services => Ok(system.map_or_else(Vec::new, |change| {
            crate::system::apply::start_services(change, tx.at_boot)
        })),
        Step::Boot => match &tx.boot {
            Some(plan) => {
                // The rescue hook has to be in place before this generation's initramfs is built, also on the first switch.
                crate::integration::ensure(paths, true)?;
                crate::system::boot::write_entry(paths, plan).map(|_| Vec::new())
            }
            None => Ok(Vec::new()),
        },
        step => run_package_step(paths, tx, generation, step, reinstall).map(|()| Vec::new()),
    }
}

fn run_package_step(
    paths: &Paths,
    tx: &Transaction,
    generation: Option<u32>,
    step: Step,
    reinstall: bool,
) -> Result<()> {
    match step {
        Step::Users | Step::StopServices | Step::Files | Step::Reload | Step::Services | Step::Restart | Step::Boot => {
            Ok(())
        }
        Step::Activate => match generation {
            Some(number) => {
                let store = Store::new(paths);
                let mut activated = store.get(number)?;
                activated.rescue = crate::system::rescue::closure(&paths.local_db());
                store.update(&activated)?;
                store.activate(number)?;
                crate::system::boot::set_default_entry(paths, number)?;
                crate::journal::write_config(&tx.config)
            }
            None => Ok(()),
        },
        Step::Install if tx.install.is_empty() => restore_running_modules(paths),
        Step::Install => {
            let installed = read_local_db(&paths.local_db())?;
            Running::new().keep(paths, tx, &installed)?;
            let taken_over = removals_taken_over(paths, tx, &installed)?;
            if !taken_over.is_empty() {
                let mut args = vec!["-Rdd", "--noconfirm"];
                args.extend(taken_over.iter().map(String::as_str));
                pacman(&args)?;
            }
            let keyrings = incoming_keyrings(&tx.install);
            if !keyrings.is_empty() {
                let mut args = vec![
                    OsStr::new("-U"),
                    OsStr::new("--noconfirm"),
                    OsStr::new("--nodeps"),
                    OsStr::new("--nodeps"),
                ];
                if !reinstall {
                    args.push(OsStr::new("--needed"));
                }
                args.extend(keyrings.iter().map(|path| path.as_os_str()));
                pacman(&args)?;
                // Their install scripts rewrote pacman's gpg keyring; torn by a power cut, it would make pacman reject every package.
                crate::durable::sync_filesystems(&crate::durable::system_paths(&paths.root()));
            }
            let rest: Vec<&PathBuf> = tx.install.iter().filter(|path| !keyrings.contains(path)).collect();
            if rest.is_empty() {
                return restore_running_modules(paths);
            }
            // -dd: the lock is a complete set, and what still depends on old versions is removed by the Remove step that follows.
            let mut args = vec![
                OsStr::new("-U"),
                OsStr::new("--noconfirm"),
                OsStr::new("--nodeps"),
                OsStr::new("--nodeps"),
                OsStr::new(ASK_REMOVE_CONFLICTS),
                OsStr::new("--overwrite"),
                OsStr::new(KERNEL_MODULES),
            ];
            if !reinstall && !tx.reinstall {
                args.push(OsStr::new("--needed"));
            }
            args.extend(rest.iter().map(|path| path.as_os_str()));
            pacman(&args)?;
            for failed in crate::system::dkms::reconcile(Path::new(crate::system::boot::MODULES)) {
                eprintln!("warning: {failed} failed, so DKMS may not remove that module's files later");
            }
            restore_running_modules(paths)
        }
        Step::Remove => {
            let installed = read_local_db(&paths.local_db())?;
            let remove: Vec<&str> = tx
                .remove
                .iter()
                .filter(|name| installed.contains_key(*name))
                .map(String::as_str)
                .collect();
            if remove.is_empty() {
                return Ok(());
            }
            Running::new().keep(paths, tx, &installed)?;
            // -dd: whatever still depends on these is removed in the same run.
            let mut args = vec!["-Rdd", "--noconfirm"];
            args.extend(remove);
            pacman(&args)
        }
        Step::Reasons => {
            let installed = read_local_db(&paths.local_db())?;
            for (flag, explicit) in [("--asexplicit", true), ("--asdeps", false)] {
                let names: Vec<&str> = tx
                    .reasons
                    .iter()
                    .filter(|(name, want)| {
                        **want == explicit && installed.get(*name).is_some_and(|p| p.explicit != explicit)
                    })
                    .map(|(name, _)| name.as_str())
                    .collect();
                if !names.is_empty() {
                    let mut args = vec!["-D", flag];
                    args.extend(names);
                    pacman(&args)?;
                }
            }
            Ok(())
        }
        Step::Verify => {
            let problems = mismatches(paths, tx)?;
            if problems.is_empty() {
                Ok(())
            } else {
                bail!("the system does not match the target:\n  {}", problems.join("\n  "))
            }
        }
    }
}

/// Incoming packages that ship a pacman keyring; they're installed on their own first, so pacman checks the rest against the keys they add.
fn incoming_keyrings(files: &[PathBuf]) -> Vec<PathBuf> {
    files
        .iter()
        .filter(|file| {
            file.file_name()
                .is_some_and(|name| name.to_string_lossy().contains("-keyring-"))
        })
        .filter(|file| {
            crate::conflicts::contents(file).is_ok_and(|contents| {
                contents
                    .files
                    .iter()
                    .any(|path| path.starts_with(crate::pacman::KEYRINGS) && path.ends_with(".gpg"))
            })
        })
        .cloned()
        .collect()
}

/// A copy of pacman's keyring that also trusts what incoming keyring packages add, removed with any gpg-agent it started when dropped.
struct TrialKeyring(PathBuf);

impl TrialKeyring {
    fn new(keyrings: &[PathBuf]) -> Result<TrialKeyring> {
        let trial = TrialKeyring(std::env::temp_dir().join(format!("archix-keyring-{}", std::process::id())));
        let gnupg = trial.gnupg();
        let import = trial.0.join("keyrings");
        copy_regular(Path::new("/etc/pacman.d/gnupg"), &gnupg)?;
        fs::set_permissions(&gnupg, fs::Permissions::from_mode(0o700))?;
        fs::create_dir_all(&import)?;
        let mut names = Vec::new();
        for keyring in keyrings {
            let status = Command::new("bsdtar")
                .args(["-xf"])
                .arg(keyring)
                .arg("-C")
                .arg(&trial.0)
                .arg(crate::pacman::KEYRINGS)
                .status()
                .context("running bsdtar")?;
            if !status.success() {
                bail!("couldn't read the keyring in {}", keyring.display());
            }
            for file in fs::read_dir(trial.0.join(crate::pacman::KEYRINGS))?.flatten() {
                let name = file.file_name().to_string_lossy().into_owned();
                if let Some(stem) = name.strip_suffix(".gpg") {
                    names.push(stem.to_string());
                }
                fs::rename(file.path(), import.join(&name))?;
            }
        }
        let output = Command::new("pacman-key")
            .arg("--gpgdir")
            .arg(&gnupg)
            .arg("--populate-from")
            .arg(&import)
            .arg("--populate")
            .args(&names)
            .output()
            .context("running pacman-key")?;
        if !output.status.success() {
            bail!(
                "pacman-key couldn't load the incoming keyrings: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(trial)
    }

    fn gnupg(&self) -> PathBuf {
        self.0.join("gnupg")
    }
}

impl Drop for TrialKeyring {
    fn drop(&mut self) {
        let _ = Command::new("gpgconf")
            .arg("--homedir")
            .arg(self.gnupg())
            .args(["--kill", "all"])
            .output();
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Copies a directory's files and subdirectories, leaving out sockets such as a running gpg-agent's.
pub fn copy_regular(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in fs::read_dir(from)
        .with_context(|| format!("reading {}", from.display()))?
        .flatten()
    {
        let kind = entry.file_type()?;
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_regular(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &target).with_context(|| format!("copying {}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// Has pacman load every incoming package without installing it, so a bad signature or unknown key fails before the Remove step runs.
pub fn check_packages(tx: &Transaction) -> Result<()> {
    if tx.install.is_empty() {
        return Ok(());
    }
    let keyrings = incoming_keyrings(&tx.install);
    let trial = match keyrings.is_empty() {
        true => None,
        false => TrialKeyring::new(&keyrings)
            .inspect_err(|e| eprintln!("warning: {e:#}; checking signatures against the installed keyring instead"))
            .ok(),
    };
    let mut pacman = Command::new("pacman");
    pacman.args([
        "-U",
        "--print",
        "--noconfirm",
        ASK_REMOVE_CONFLICTS,
        "--nodeps",
        "--nodeps",
    ]);
    if let Some(trial) = &trial {
        pacman.arg("--gpgdir").arg(trial.gnupg());
    }
    let output = pacman
        .args(&tx.install)
        .env(TRANSACTION_ENV, "1")
        .output()
        .context("running pacman")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let errors: Vec<&str> = stderr.lines().filter(|line| line.starts_with("error:")).collect();
        bail!(
            "pacman refuses these packages, so nothing was changed:\n  {}",
            if errors.is_empty() {
                stderr.trim().to_string()
            } else {
                errors.join("\n  ")
            }
        );
    }
    Ok(())
}

/// Hard links to package files without the .sig beside them, for a rollback: each was verified by pacman when first installed and is checked against its pinned sha256, while today's keyring may have revoked its packager since.
pub fn without_signatures(paths: &Paths, files: &[std::path::PathBuf]) -> Result<Vec<std::path::PathBuf>> {
    let dir = paths.state.join("verified");
    fs::create_dir_all(&dir)?;
    let mut out = Vec::new();
    for file in files {
        let has_sig = file
            .with_file_name(format!(
                "{}.sig",
                file.file_name().unwrap_or_default().to_string_lossy()
            ))
            .exists();
        if !has_sig {
            out.push(file.clone());
            continue;
        }
        let link = dir.join(file.file_name().unwrap_or_default());
        if !link.exists() && fs::hard_link(file, &link).is_err() {
            fs::copy(file, &link).with_context(|| format!("staging {}", file.display()))?;
        }
        out.push(link);
    }
    Ok(out)
}

/// Packages the transaction removes that own a file an incoming package ships; pacman would refuse the install while they still hold it, so they go first.
fn removals_taken_over(paths: &Paths, tx: &Transaction, installed: &BTreeMap<String, LocalPkg>) -> Result<Vec<String>> {
    let removing: Vec<&String> = tx.remove.iter().filter(|name| installed.contains_key(*name)).collect();
    if removing.is_empty() {
        return Ok(Vec::new());
    }
    let mut incoming = std::collections::HashSet::new();
    for path in &tx.install {
        incoming.extend(crate::conflicts::contents(path)?.files);
    }
    let owners = crate::conflicts::owners(&paths.local_db());
    Ok(removing
        .into_iter()
        .filter(|name| {
            owners
                .get(name.as_str())
                .is_some_and(|files| files.iter().any(|file| incoming.contains(file)))
        })
        .cloned()
        .collect())
}

fn restore_running_modules(paths: &Paths) -> Result<()> {
    if Running::new().restore(paths)? {
        eprintln!(":: kept the running kernel's modules, so they still load until you reboot");
    }
    Ok(())
}

/// Every way the installed packages differ from the transaction's end state.
pub fn mismatches(paths: &Paths, tx: &Transaction) -> Result<Vec<String>> {
    let installed = read_local_db(&paths.local_db())?;
    let keyrings = crate::pacman::keyring_packages(&paths.local_db());
    let mut problems = Vec::new();
    for (name, version) in &tx.versions {
        match installed.get(name) {
            None => problems.push(format!("{name} {version} is not installed")),
            Some(pkg)
                if keyrings.contains(name)
                    && crate::pacman::vercmp(&pkg.version, version) == std::cmp::Ordering::Greater => {}
            Some(pkg) if pkg.version != *version => {
                problems.push(format!("{name} is {} instead of {version}", pkg.version))
            }
            Some(_) => {}
        }
    }
    for name in &tx.remove {
        if installed.contains_key(name) {
            problems.push(format!("{name} is still installed"));
        }
    }
    for (name, explicit) in &tx.reasons {
        if installed.get(name).is_some_and(|pkg| pkg.explicit != *explicit) {
            let reason = if *explicit {
                "explicitly installed"
            } else {
                "a dependency"
            };
            problems.push(format!("{name} should be marked as {reason}"));
        }
    }
    Ok(problems)
}

static HANGUP_LOG: std::sync::OnceLock<std::ffi::CString> = std::sync::OnceLock::new();

/// Once the terminal hangs up, as a dropped ssh connection does, writing to it fails, and Rust's print macros would stop archix halfway; output goes to a log instead.
extern "C" fn after_hangup(_: libc::c_int) {
    if let Some(path) = HANGUP_LOG.get() {
        unsafe {
            let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND | libc::O_CLOEXEC;
            let fd = libc::open(path.as_ptr(), flags, 0o600 as libc::c_uint);
            if fd >= 0 {
                libc::dup2(fd, 1);
                libc::dup2(fd, 2);
                libc::close(fd);
            }
        }
    }
}

/// Holds off SIGINT and a hang-up while the journal runs, restoring the previous handlers afterwards.
struct HoldInterrupts {
    previous: [libc::sighandler_t; 2],
}

impl HoldInterrupts {
    fn new(paths: &Paths) -> Self {
        let log = paths.state.join("after-hangup.log");
        let _ = HANGUP_LOG.set(std::ffi::CString::new(log.as_os_str().as_encoded_bytes()).unwrap_or_default());
        let previous = unsafe {
            [
                libc::signal(libc::SIGHUP, after_hangup as *const () as libc::sighandler_t),
                libc::signal(libc::SIGINT, libc::SIG_IGN),
            ]
        };
        eprintln!(":: interrupts wait until the switch is done; if it is cut off anyway, `archix recover` finishes it");
        HoldInterrupts { previous }
    }
}

impl Drop for HoldInterrupts {
    fn drop(&mut self) {
        unsafe {
            libc::signal(libc::SIGHUP, self.previous[0]);
            libc::signal(libc::SIGINT, self.previous[1]);
        }
    }
}

fn pacman<S: AsRef<OsStr>>(args: &[S]) -> Result<()> {
    let mut pacman = Command::new("pacman");
    pacman.args(args).process_group(0).env(TRANSACTION_ENV, "1");
    // archix's own hang-up handler doesn't carry over into pacman, so it ignores the signal outright.
    unsafe {
        pacman.pre_exec(|| {
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            Ok(())
        });
    }
    let status = pacman.status().context("running pacman")?;
    if !status.success() {
        bail!("pacman exited with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lock_counts_as_held_only_while_a_process_has_it_open() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("db.lck");
        let file = fs::File::create(&lock).unwrap();
        assert!(held_open(&lock));
        drop(file);
        // Another test forking can hold every fd of this process until its child execs.
        let stale = (0..100).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(10));
            !held_open(&lock)
        });
        assert!(stale, "a lock left by a killed pacman is stale");
    }
}

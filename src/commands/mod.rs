mod helper;
mod init;
pub mod keep;
mod lock;
pub mod snapshot;

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use archix::archive::Archive;
use archix::aur;
use archix::aur::git::AurGit;
use archix::aur::index::Index;
use archix::date::Date;
use archix::flatpak;
use archix::lock::{HostLock, Locked};
use archix::packages::{self, PacmanLock};
use archix::paths::Paths;
use archix::recipes::Recipes;
use archix::repos::{Custom, Repo, is_official, ordered};
use archix::resolve::Aur;
use archix::value::Value;

pub use helper::{prefetch, search};
pub use init::init;
pub use keep::limine_after_good_boot;
pub use lock::lock;

/// `archix secure-boot setup`.
pub fn secure_boot_revoke(dry_run: bool, noconfirm: bool) -> Result<()> {
    if !dry_run {
        require_root("secure-boot revoke")?;
    }
    archix::system::revoke::revoke(&Paths::system(), dry_run, false, &|question| {
        if noconfirm { Ok(true) } else { confirm(question) }
    })
}

pub fn secure_boot_setup(microsoft: bool) -> Result<()> {
    require_root("secure-boot setup")?;
    if !microsoft {
        eprintln!(
            "warning: without Microsoft's keys, GPU option ROMs and Windows may fail to start, and some boards won't boot at all"
        );
    }
    archix::system::secureboot::setup(microsoft)
}

/// `archix secure-boot check`; true when everything checked is met.
pub fn secure_boot_check(anticheat: bool) -> bool {
    let lines = archix::system::secureboot::Machine::new().check(anticheat);
    for (ok, line) in &lines {
        println!("{} {line}", if *ok { "  ok " } else { "  no " });
    }
    let missing = lines.iter().filter(|(ok, _)| !ok).count();
    if anticheat && missing > 0 {
        println!(
            "Easy Anti-Cheat Linux v2 would refuse this boot: {}",
            count(missing, "requirement isn't met", "requirements aren't met")
        );
    }
    if Paths::system().state.join("boot").join("esp").exists()
        && let Some(why) = archix::system::boot::counting_off()
    {
        println!("note: {why}");
    }
    missing == 0
}

const FLATHUB: &str = "https://dl.flathub.org/repo/flathub.flatpakrepo";

pub struct Target<'a> {
    pub config_dir: &'a Path,
    pub host: &'a str,
    pub config: &'a Value,
    /// Where the evaluated config's files are read from: root's own copy of the captured tree, or the config directory itself when not root.
    pub tree: &'a Path,
    /// The tree a root command captured, which a switch records as applied.
    pub captured: Option<&'a archix::captured::Captured>,
}

/// A config evaluated for a command: as root, captured as its owner into a root-owned copy and evaluated under a transient uid; otherwise in this process.
pub struct Evaluated {
    pub config: Value,
    pub captured: Option<archix::captured::Captured>,
    _work: Option<archix::captured::Work>,
}

pub fn evaluate_config(
    config_dir: &Path,
    host: &str,
    overlay: std::collections::BTreeMap<std::path::PathBuf, String>,
) -> Result<std::result::Result<Evaluated, String>> {
    if unsafe { libc::geteuid() } != 0 {
        return Ok(archix::eval::evaluate(&archix::eval::Input {
            config_dir: config_dir.to_path_buf(),
            hostname: host.to_string(),
            overlay,
        })
        .map(|evaluation| Evaluated {
            config: evaluation.config,
            captured: None,
            _work: None,
        })
        .map_err(|errors| errors.to_string()));
    }
    let work = archix::captured::Work::new(Path::new(archix::captured::CAPTURES))?;
    let captured = archix::captured::capture(config_dir, &work)?;
    archix::testhooks::after_capture();
    Ok(reevaluate(&captured, host, &overlay)
        .map_err(|error| captured.explain(error))
        .map(|config| Evaluated {
            config,
            captured: Some(captured),
            _work: Some(work),
        }))
}

/// Evaluates an already captured tree again, with another overlay, in the isolated child.
pub fn reevaluate(
    captured: &archix::captured::Captured,
    host: &str,
    overlay: &std::collections::BTreeMap<std::path::PathBuf, String>,
) -> std::result::Result<Value, String> {
    archix::eval::isolated::evaluate(&captured.dir, host, overlay, &Default::default()).map_err(|e| format!("{e:#}"))
}

/// Which archive snapshot `archix lock` resolves against.
pub enum Snapshot {
    Locked,
    Latest,
    On(Date),
}

/// The package-related settings from the evaluated config.
struct Declared {
    packages: Vec<String>,
    prefer: Vec<String>,
    repos: Vec<Repo>,
    aur_url: String,
    aur_review: bool,
    archive_url: String,
    archive_mirrors: Vec<String>,
    package_caches: Vec<String>,
    boot: Option<BootSettings>,
    /// (remote, app id) pairs.
    flatpak_apps: Vec<(String, String)>,
    flatpak_remotes: BTreeMap<String, flatpak::Remote>,
    /// Official packages to rebuild with these patch files, relative to the config directory.
    patches: BTreeMap<String, Vec<String>>,
    holds: archix::holds::Holds,
}

/// The boot.* settings, present when archix manages booting.
struct BootSettings {
    loader: archix::system::boot::Loader,
    esp: String,
    kernel: String,
    microcode: Vec<String>,
    secure_boot: Option<bool>,
    cmdline: String,
    protected_entry: bool,
    tries: u32,
    revoke_unused: bool,
    health_check: Option<archix::system::health::HealthCheck>,
    rescue_entry: bool,
    unblessed_timeout: u32,
}

impl Declared {
    fn hold_specs(&self) -> BTreeMap<String, String> {
        archix::holds::specs(&self.holds)
    }

    fn repo_names(&self) -> Vec<String> {
        self.repos.iter().map(|repo| repo.name.clone()).collect()
    }

}

/// `boot.health_check`, the default checks when it's not set.
fn health_check(value: Option<&Value>) -> Result<Option<archix::system::health::HealthCheck>> {
    let Some(Value::Map(check)) = value else {
        return Ok(Some(archix::system::health::HealthCheck::default()));
    };
    let strings = |key: &str| -> Vec<String> {
        match check.get(key) {
            Some(Value::List(items)) => items
                .iter()
                .filter_map(|item| match item {
                    Value::Str(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    };
    let listening = match check.get("listening") {
        Some(Value::List(items)) => items
            .iter()
            .filter_map(|item| match item {
                Value::Int(port) => Some(
                    u16::try_from(*port)
                        .ok()
                        .filter(|port| *port > 0)
                        .with_context(|| format!("boot.health_check.listening: {port} isn't a TCP port")),
                ),
                _ => None,
            })
            .collect::<Result<Vec<u16>>>()?,
        _ => Vec::new(),
    };
    let parsed = archix::system::health::HealthCheck {
        default_target: check.get("default_target") != Some(&Value::Bool(false)),
        failed_units: check.get("failed_units") != Some(&Value::Bool(false)),
        display_manager: check.get("display_manager") != Some(&Value::Bool(false)),
        units: strings("units"),
        listening,
        reachable: strings("reachable"),
        login: check.get("login") == Some(&Value::Bool(true)),
        timeout: match check.get("timeout") {
            Some(Value::Int(n)) => u32::try_from(*n).unwrap_or(0),
            _ => 120,
        },
        reboot_on_failure: check.get("reboot_on_failure") == Some(&Value::Bool(true)),
    };
    Ok(Some(parsed))
}

fn declared(config: &Value) -> Result<Declared> {
    let Value::Map(map) = config else {
        bail!("the evaluated config is not a table")
    };
    let strings = |key: &str| -> Vec<String> {
        match map.get(key) {
            Some(Value::List(items)) => items
                .iter()
                .filter_map(|item| match item {
                    Value::Str(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    };
    let packages: Vec<String> = strings("packages")
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut official = Vec::new();
    for repo in strings("repos") {
        if !official.contains(&repo) {
            official.push(repo);
        }
    }
    if official.is_empty() {
        bail!("`repos` is empty; packages need at least one repository");
    }
    let mut custom = Vec::new();
    if let Some(Value::List(entries)) = map.get("custom_repos") {
        for entry in entries {
            let Value::Map(entry) = entry else { continue };
            let text = |key: &str| match entry.get(key) {
                Some(Value::Str(s)) => s.clone(),
                _ => String::new(),
            };
            let list = |key: &str| match entry.get(key) {
                Some(Value::List(items)) => items
                    .iter()
                    .filter_map(|item| match item {
                        Value::Str(s) => Some(s.clone()),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            let name = text("name");
            if is_official(&name) || custom.iter().any(|(existing, _)| *existing == name) {
                bail!("custom repo `{name}` clashes with an official or another custom repo");
            }
            let repo = Custom {
                servers: list("servers"),
                keys: list("keys"),
                keyserver: text("keyserver"),
                sig_level: text("sig_level"),
                before_official: entry.get("before_official") == Some(&Value::Bool(true)),
            };
            custom.push((name, repo));
        }
    }
    let repos = ordered(&official, custom);
    let mut declared = Declared {
        packages,
        prefer: strings("prefer"),
        repos,
        aur_url: match map.get("aur") {
            Some(Value::Map(aur)) => match aur.get("url") {
                Some(Value::Str(url)) => url.clone(),
                _ => aur::DEFAULT_URL.to_string(),
            },
            _ => aur::DEFAULT_URL.to_string(),
        },
        package_caches: strings("package_caches"),
        archive_mirrors: strings("archive_mirrors"),
        archive_url: match map.get("archive_url") {
            Some(Value::Str(url)) if !url.is_empty() => url.trim_end_matches('/').to_string(),
            _ => archix::archive::ARCHIVE_URL.to_string(),
        },
        aur_review: !matches!(map.get("aur"), Some(Value::Map(aur)) if aur.get("review") == Some(&Value::Bool(false))),
        boot: match map.get("boot") {
            Some(Value::Map(boot)) if matches!(boot.get("loader"), Some(Value::Str(_))) => {
                let loader = match boot.get("loader") {
                    Some(Value::Str(name)) => archix::system::boot::Loader::parse(name)
                        .with_context(|| format!("boot.loader is {name:?}, which archix doesn't write entries for"))?,
                    _ => archix::system::boot::Loader::SystemdBoot,
                };
                let text = |key: &str, default: &str| match boot.get(key) {
                    Some(Value::Str(s)) => s.clone(),
                    _ => default.to_string(),
                };
                let params = strings("kernel_params");
                if !params.iter().any(|p| p.starts_with("root=")) {
                    bail!(
                        "boot.loader is set, but kernel_params has no root=; archix needs the full kernel command line"
                    );
                }
                Some(BootSettings {
                    loader,
                    esp: text("esp", "/efi"),
                    kernel: text("kernel", "linux"),
                    microcode: match boot.get("microcode") {
                        Some(Value::List(items)) => items
                            .iter()
                            .filter_map(|i| match i {
                                Value::Str(s) => Some(s.clone()),
                                _ => None,
                            })
                            .collect(),
                        _ => Vec::new(),
                    },
                    secure_boot: match boot.get("secure_boot") {
                        Some(Value::Bool(b)) => Some(*b),
                        _ => None,
                    },
                    cmdline: params.join(" "),
                    protected_entry: boot.get("protected_entry") == Some(&Value::Bool(true)),
                    tries: match boot.get("tries") {
                        Some(Value::Int(n)) => u32::try_from(*n).unwrap_or(0),
                        _ => 2,
                    },
                    unblessed_timeout: match boot.get("unblessed_timeout") {
                        Some(Value::Int(n)) => u32::try_from(*n).unwrap_or(0),
                        _ => 10,
                    },
                    revoke_unused: boot.get("revoke_unused") == Some(&Value::Bool(true)),
                    health_check: health_check(boot.get("health_check"))?,
                    rescue_entry: boot.get("rescue_entry") != Some(&Value::Bool(false)),
                })
            }
            _ => None,
        },
        flatpak_apps: Vec::new(),
        flatpak_remotes: BTreeMap::new(),
        patches: match map.get("patches") {
            Some(Value::Map(patches)) => patches
                .iter()
                .map(|(name, files)| {
                    let files = match files {
                        Value::List(items) => items
                            .iter()
                            .filter_map(|item| match item {
                                Value::Str(s) => Some(s.clone()),
                                _ => None,
                            })
                            .collect(),
                        _ => Vec::new(),
                    };
                    (name.clone(), files)
                })
                .collect(),
            _ => BTreeMap::new(),
        },
        holds: archix::holds::from_config(map.get("holds"))?,
    };
    if let Some(Value::Map(fp)) = map.get("flatpak") {
        let default_remote = match fp.get("remote") {
            Some(Value::Str(remote)) => remote.clone(),
            _ => "flathub".to_string(),
        };
        if let Some(Value::Map(remotes)) = fp.get("remotes") {
            for (name, remote) in remotes {
                let Value::Map(remote) = remote else { continue };
                let Some(Value::Str(url)) = remote.get("url") else {
                    continue;
                };
                declared.flatpak_remotes.insert(
                    name.clone(),
                    flatpak::Remote {
                        url: url.clone(),
                        gpg_verify: remote.get("gpg_verify") != Some(&Value::Bool(false)),
                    },
                );
            }
        }
        if let Some(Value::List(apps)) = fp.get("apps") {
            for app in apps {
                let Value::Str(app) = app else { continue };
                let (remote, id) = match app.split_once(':') {
                    Some((remote, id)) => (remote.to_string(), id.to_string()),
                    None => (default_remote.clone(), app.clone()),
                };
                if !declared.flatpak_remotes.contains_key(&remote) {
                    if remote != "flathub" {
                        bail!("flatpak.apps: {app} uses remote {remote}, which flatpak.remotes doesn't declare");
                    }
                    declared.flatpak_remotes.insert(
                        remote.clone(),
                        flatpak::Remote {
                            url: FLATHUB.to_string(),
                            gpg_verify: true,
                        },
                    );
                }
                declared.flatpak_apps.push((remote, id));
            }
        }
    }
    if let Some(boot) = &declared.boot {
        let mut needed = vec![
            boot.kernel.clone(),
            "mkinitcpio".to_string(),
            "systemd-ukify".to_string(),
            // Without fsck.vfat no unit checks the ESP at boot, and the first broken cluster chain a power cut left makes the kernel remount it read-only.
            "dosfstools".to_string(),
        ];
        if boot.secure_boot == Some(true) {
            needed.push("sbctl".to_string());
        }
        if boot.revoke_unused {
            needed.push("efitools".to_string());
        }
        // DKMS builds its modules for a new kernel in the staging root, which needs that kernel's headers.
        if declared
            .packages
            .iter()
            .any(|name| name == "dkms" || name.ends_with("-dkms"))
        {
            needed.push(format!("{}-headers", boot.kernel));
        }
        declared
            .prefer
            .extend(needed.into_iter().filter(|name| !declared.packages.contains(name)));
        declared.prefer.sort();
        declared.prefer.dedup();
    }
    Ok(declared)
}

/// A package archix builds from a VCS checkout that archix.lock records no commit for, going by the suffixes AUR helpers' devel modes use.
fn floats(name: &str, pkg: &Locked) -> bool {
    pkg.is_built()
        && pkg.sources.is_empty()
        && ["-git", "-hg", "-svn", "-bzr", "-fossil", "-darcs"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

fn check_pacman_free(paths: &Paths) -> Result<()> {
    match packages::pacman_lock(paths) {
        PacmanLock::Free => Ok(()),
        PacmanLock::Running => bail!("pacman is running; wait for it to finish"),
        PacmanLock::Stale if packages::from_an_earlier_boot(&paths.pacman_lock()) => {
            std::fs::remove_file(paths.pacman_lock())
                .with_context(|| format!("removing {}", paths.pacman_lock().display()))?;
            eprintln!(
                "removed {}, left by a pacman that a reboot or power cut stopped",
                paths.pacman_lock().display()
            );
            Ok(())
        }
        PacmanLock::Stale => bail!(
            "{} exists but no pacman is running; if no package manager is in use, remove it and try again",
            paths.pacman_lock().display()
        ),
    }
}

fn require_root(command: &str) -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("`archix {command}` changes the system; run it as root (sudo archix {command})");
    }
    Ok(())
}

/// "1 package", "3 packages".
fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Like `confirm`, but an empty answer means no.
fn confirm_no(question: &str) -> Result<bool> {
    eprint!("{question} [y/N] ");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    let answer = answer.trim().to_lowercase();
    Ok(answer == "y" || answer == "yes")
}

/// Asks with yes as the empty answer when `default_yes`, else no.
fn confirm_or(question: &str, default_yes: bool) -> Result<bool> {
    if default_yes {
        confirm(question)
    } else {
        confirm_no(question)
    }
}

fn confirm(question: &str) -> Result<bool> {
    eprint!("{question} [Y/n] ");
    io::stderr().flush()?;
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer)? == 0 {
        return Ok(false);
    }
    let answer = answer.trim().to_lowercase();
    Ok(answer.is_empty() || answer == "y" || answer == "yes")
}

/// Root shares `/var/cache/archix`; other users get their own cache, since snapshots are immutable either way.
fn user_cache() -> Result<PathBuf> {
    if unsafe { libc::geteuid() } == 0 {
        return Ok(Paths::system().cache);
    }
    if let Some(dir) = env::var_os("XDG_CACHE_HOME").filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir).join("archix"));
    }
    match env::var_os("HOME") {
        Some(home) => Ok(PathBuf::from(home).join(".cache/archix")),
        None => bail!("neither XDG_CACHE_HOME nor HOME is set"),
    }
}

fn aur_git(declared: &Declared) -> Result<AurGit> {
    Ok(AurGit::new(&declared.aur_url, &user_cache()?))
}

/// The config's local recipes and patched packages, from the capture when root.
fn recipes(target: &Target, declared: &Declared) -> Result<Recipes> {
    Recipes::load(target.tree, &declared.patches, &user_cache()?)
}

/// The AUR and the config's recipes as the resolver sees them; the AUR's package list is downloaded only if a name is in neither the repos nor pkgbuilds/.
fn aur_source<'a>(
    git: &'a AurGit,
    declared: &'a Declared,
    archive: &'a Archive,
    previous: Option<&'a HostLock>,
    refresh: bool,
    recipes: &'a Recipes,
) -> Aur<'a> {
    Aur::new(git, previous, move || {
        Index::load(archive, &declared.aur_url, &user_cache()?, refresh)
    })
    .with_recipes(recipes)
}

/// Drops locked Flatpak apps the config no longer declares and locks new ones (all, with `refresh`) to their remote's commit; returns what it couldn't lock.
fn settle_flatpak(
    target: &Target,
    declared: &Declared,
    lock: &mut HostLock,
    save: bool,
    refresh: bool,
) -> Result<Vec<String>> {
    let wanted: BTreeMap<&str, &str> = declared
        .flatpak_apps
        .iter()
        .map(|(remote, id)| (id.as_str(), remote.as_str()))
        .collect();
    let before = lock.flatpak.clone();
    lock.flatpak
        .retain(|id, app| wanted.get(id.as_str()) == Some(&app.remote.as_str()));
    let mut unresolved = Vec::new();
    let missing: Vec<&(String, String)> = declared
        .flatpak_apps
        .iter()
        .filter(|(_, id)| refresh || !lock.flatpak.contains_key(id))
        .collect();
    if !missing.is_empty() && flatpak::available() {
        for (remote, id) in &missing {
            match flatpak::latest(remote, id) {
                Ok(app) => {
                    lock.flatpak.insert(id.clone(), app);
                }
                Err(_) if lock.flatpak.contains_key(id) => {
                    eprintln!("warning: couldn't check {id} on {remote}; keeping its locked commit")
                }
                Err(e) => unresolved.push(format!("{id}: {e:#}")),
            }
        }
    } else {
        unresolved.extend(
            missing
                .iter()
                .map(|(_, id)| format!("{id}: flatpak isn't installed yet")),
        );
    }
    if save && lock.flatpak != before {
        let mut lockfile = lock::load_lockfile(target)?;
        lockfile.hosts.insert(target.host.to_string(), lock.clone());
        lock::store_lockfile(target, &lockfile)?;
    }
    Ok(unresolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use archix::eval;
    use std::fs;

    fn declared_from(lua: &str) -> Declared {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("init.lua"), lua).unwrap();
        let evaluation = eval::evaluate(&eval::Input {
            config_dir: dir.path().to_path_buf(),
            hostname: "box".to_string(),
            ..Default::default()
        })
        .unwrap_or_else(|errors| panic!("{errors}"));
        declared(&evaluation.config).unwrap()
    }

    #[test]
    fn a_dkms_package_brings_the_boot_kernels_headers() {
        let boot =
            r#"boot = { loader = "systemd-boot", kernel = "linux-cachyos" }, kernel_params = { "root=/dev/vda2" }"#;
        let with_dkms = declared_from(&format!(
            r#"return {{ packages = {{ "base", "nvidia-dkms" }}, {boot} }}"#
        ));
        assert!(
            with_dkms.prefer.contains(&"linux-cachyos-headers".to_string()),
            "{:?}",
            with_dkms.prefer
        );
        let without = declared_from(&format!(r#"return {{ packages = {{ "base" }}, {boot} }}"#));
        assert!(
            !without.prefer.iter().any(|name| name.ends_with("-headers")),
            "{:?}",
            without.prefer
        );
        let unmanaged = declared_from(r#"return { packages = { "base", "nvidia-dkms" } }"#);
        assert!(
            !unmanaged.prefer.iter().any(|name| name.ends_with("-headers")),
            "{:?}",
            unmanaged.prefer
        );
    }

    #[test]
    fn a_managed_boot_brings_fsck_vfat_so_a_boot_after_a_power_cut_repairs_the_esp() {
        let managed = declared_from(
            r#"return { packages = { "base" }, boot = { loader = "systemd-boot" }, kernel_params = { "root=/dev/vda2" } }"#,
        );
        assert!(
            managed.prefer.contains(&"dosfstools".to_string()),
            "{:?}",
            managed.prefer
        );
        let unmanaged = declared_from(r#"return { packages = { "base" } }"#);
        assert!(!unmanaged.prefer.contains(&"dosfstools".to_string()));
    }
}

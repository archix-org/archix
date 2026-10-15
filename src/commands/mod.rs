mod helper;
mod init;
mod lock;
mod recover;
mod switch;

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use archix::apply::{self, PacmanLock};
use archix::archive::Archive;
use archix::aur;
use archix::aur::git::AurGit;
use archix::aur::index::Index;
use archix::date::Date;
use archix::fetch::{Wanted, fetch_all};
use archix::flatpak;
use archix::journal::{Journal, Kind, Transaction};
use archix::lock::{ARCHIVE_REPO, AUR_REPO, HostLock, LOCK_FILE, Locked, Lockfile};
use archix::pacman::LocalPkg;
use archix::paths::Paths;
use archix::plan::{Change, plan};
use archix::repos::{Custom, Repo, is_official, ordered};
use archix::resolve::Aur;
use archix::value::Value;

pub use helper::{edit_installed, install_temporarily, prefetch, search, upgrade};
pub use init::init;
pub use lock::{diff, lock, status};
pub use recover::recover;
pub use switch::{boot_check, gc, generations, hook_record, rescue, rollback, switch};

const FLATHUB: &str = "https://dl.flathub.org/repo/flathub.flatpakrepo";

pub struct Target<'a> {
    pub config_dir: &'a Path,
    pub host: &'a str,
    pub config: &'a Value,
}

/// Which archive snapshot `archix lock` resolves against.
pub enum Snapshot {
    Locked,
    Latest,
    On(Date),
}

#[derive(Clone, Copy)]
pub enum RecoverMode {
    Continue,
    Rollback,
    Abandon,
}

/// The package-related settings from the evaluated config.
struct Declared {
    packages: Vec<String>,
    repos: Vec<Repo>,
    third_party_fallback: ThirdPartyFallback,
    remove_undeclared: bool,
    ignored: BTreeSet<String>,
    keep: u32,
    keep_days: u32,
    aur_url: String,
    aur_review: bool,
    build_on_host: BTreeSet<String>,
    missing_versions: Option<Nearest>,
    boot: Option<BootSettings>,
    snapshots: bool,
    /// (remote, app id) pairs.
    flatpak_apps: Vec<(String, String)>,
    flatpak_remotes: BTreeMap<String, flatpak::Remote>,
}

/// The boot.* settings, present when archix manages booting.
struct BootSettings {
    esp: String,
    kernel: String,
    microcode: Vec<String>,
    entries: u32,
    secure_boot: Option<bool>,
    cmdline: String,
}

#[derive(Clone, Copy, PartialEq)]
enum ThirdPartyFallback {
    Ask,
    Fallback,
    Fail,
}

impl Declared {
    fn repo_names(&self) -> Vec<String> {
        self.repos.iter().map(|repo| repo.name.clone()).collect()
    }

    fn repo(&self, name: &str) -> Option<&Repo> {
        self.repos.iter().find(|repo| repo.name == name)
    }
}

/// Which version to take when a locked AUR version can't be built any more.
#[derive(Clone, Copy, Debug, PartialEq, clap::ValueEnum)]
pub enum Nearest {
    Newer,
    Older,
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
    let generation_limit = |key: &str, default: u32| match map.get("generations") {
        Some(Value::Map(limits)) => match limits.get(key) {
            Some(Value::Int(n)) => u32::try_from(*n).unwrap_or(default),
            _ => default,
        },
        _ => default,
    };
    let mut declared = Declared {
        packages,
        repos,
        third_party_fallback: match map.get("third_party_fallback") {
            Some(Value::Str(policy)) if policy == "fallback" => ThirdPartyFallback::Fallback,
            Some(Value::Str(policy)) if policy == "fail" => ThirdPartyFallback::Fail,
            _ => ThirdPartyFallback::Ask,
        },
        remove_undeclared: map.get("undeclared_packages") != Some(&Value::Str("warn".into())),
        ignored: strings("ignored_packages").into_iter().collect(),
        keep: generation_limit("keep", 10),
        keep_days: generation_limit("keep_days", 30),
        aur_url: match map.get("aur") {
            Some(Value::Map(aur)) => match aur.get("url") {
                Some(Value::Str(url)) => url.clone(),
                _ => aur::DEFAULT_URL.to_string(),
            },
            _ => aur::DEFAULT_URL.to_string(),
        },
        aur_review: !matches!(map.get("aur"), Some(Value::Map(aur)) if aur.get("review") == Some(&Value::Bool(false))),
        build_on_host: match map.get("aur") {
            Some(Value::Map(aur)) => match aur.get("build_on_host") {
                Some(Value::List(items)) => items
                    .iter()
                    .filter_map(|item| match item {
                        Value::Str(s) => Some(s.clone()),
                        _ => None,
                    })
                    .collect(),
                _ => BTreeSet::new(),
            },
            _ => BTreeSet::new(),
        },
        boot: match map.get("boot") {
            Some(Value::Map(boot)) if matches!(boot.get("loader"), Some(Value::Str(_))) => {
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
                    esp: text("esp", "/boot"),
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
                    entries: match boot.get("entries") {
                        Some(Value::Int(n)) => u32::try_from(*n).unwrap_or(5).max(1),
                        _ => 5,
                    },
                    secure_boot: match boot.get("secure_boot") {
                        Some(Value::Bool(b)) => Some(*b),
                        _ => None,
                    },
                    cmdline: params.join(" "),
                })
            }
            _ => None,
        },
        snapshots: !matches!(map.get("boot"), Some(Value::Map(boot)) if boot.get("snapshots") == Some(&Value::Bool(false))),
        missing_versions: match map.get("missing_versions") {
            Some(Value::Str(policy)) if policy == "newer" => Some(Nearest::Newer),
            Some(Value::Str(policy)) if policy == "older" => Some(Nearest::Older),
            _ => None,
        },
        flatpak_apps: Vec::new(),
        flatpak_remotes: BTreeMap::new(),
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
        ];
        if boot.secure_boot == Some(true) {
            needed.push("sbctl".to_string());
        }
        declared.packages.extend(needed);
        declared.packages.sort();
        declared.packages.dedup();
    }
    Ok(declared)
}

/// Changes that turn `installed` into `packages`; removals skip ignored packages and are only warned about in warn mode.
fn select_changes(
    packages: &BTreeMap<String, Locked>,
    installed: &BTreeMap<String, LocalPkg>,
    remove_undeclared: bool,
    ignored: &BTreeSet<String>,
) -> (Vec<Change>, Vec<String>) {
    let mut warned = Vec::new();
    let changes = plan(packages, installed)
        .into_iter()
        .filter(|change| match change {
            Change::Remove { name, .. } if ignored.contains(name) => false,
            Change::Remove { name, .. } if !remove_undeclared => {
                warned.push(name.clone());
                false
            }
            _ => true,
        })
        .collect();
    (changes, warned)
}

fn print_warned(warned: &[String]) {
    if !warned.is_empty() {
        println!(
            "not declared, kept because undeclared_packages = \"warn\": {}",
            warned.join(" ")
        );
    }
}

/// Downloads everything `changes` installs and builds the transaction that ends at exactly `packages`.
fn transaction_for(
    archive: &Archive,
    paths: &Paths,
    date: Date,
    packages: &BTreeMap<String, Locked>,
    changes: &[Change],
    built: &BTreeMap<String, PathBuf>,
    repos: &[Repo],
) -> Result<Transaction> {
    let mut wanted = Vec::new();
    let mut remove = Vec::new();
    let mut install = Vec::new();
    for change in changes {
        match change {
            Change::Install { name, .. } | Change::Upgrade { name, .. } | Change::Downgrade { name, .. } => {
                let pkg = &packages[name];
                if let Some(path) = built.get(name) {
                    install.push(path.clone());
                    continue;
                }
                if pkg.repo == AUR_REPO {
                    let sha256 = (!pkg.sha256.is_empty()).then(|| pkg.sha256.clone());
                    wanted.push(Wanted {
                        name: name.clone(),
                        file: pkg.file.clone(),
                        urls: Vec::new(),
                        sha256,
                    });
                    continue;
                }
                let custom = repos.iter().find(|repo| repo.name == pkg.repo && repo.custom.is_some());
                let urls = match custom {
                    Some(repo) => repo.urls(&pkg.file),
                    None if pkg.repo == ARCHIVE_REPO => vec![archive.package_url(name, &pkg.file)],
                    None => vec![
                        format!("{}/{}", archive.repo_url(date, &pkg.repo), pkg.file),
                        archive.package_url(name, &pkg.file),
                    ],
                };
                wanted.push(Wanted {
                    name: name.clone(),
                    file: pkg.file.clone(),
                    urls,
                    sha256: Some(pkg.sha256.clone()),
                });
            }
            Change::Remove { name, .. } => remove.push(name.clone()),
            Change::Reason { .. } => {}
        }
    }
    install.extend(fetch_all(
        archive,
        &paths.packages(),
        std::slice::from_ref(&paths.pacman_cache),
        &wanted,
    )?);
    Ok(Transaction {
        install,
        remove,
        system: None,
        boot: None,
        versions: packages
            .iter()
            .filter(|(_, pkg)| !pkg.build_only)
            .map(|(name, pkg)| (name.clone(), pkg.version.clone()))
            .collect(),
        reasons: packages
            .iter()
            .filter(|(_, pkg)| !pkg.build_only)
            .map(|(name, pkg)| (name.clone(), pkg.explicit))
            .collect(),
    })
}

fn describe_unfinished(journal: &Journal) -> String {
    let step = journal.next_step().map_or("cleanup", |step| step.describe());
    format!(
        "the {} to {} (started at unix time {}) was interrupted while {step}",
        kind_name(journal.kind),
        journal.target,
        journal.started
    )
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Switch => "switch",
        Kind::Rollback => "rollback",
    }
}

fn refuse_if_unfinished(paths: &Paths) -> Result<()> {
    if let Some(journal) = Journal::load(&paths.journal())? {
        bail!(
            "{}.\nrun `archix recover` to continue it, roll it back or abandon it",
            describe_unfinished(&journal)
        );
    }
    Ok(())
}

fn check_pacman_free(paths: &Paths) -> Result<()> {
    match apply::pacman_lock(paths) {
        PacmanLock::Free => Ok(()),
        PacmanLock::Running => bail!("pacman is running; wait for it to finish"),
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

/// The AUR as the resolver sees it; the package list is downloaded only if a name isn't in the official repos.
fn aur_source<'a>(
    git: &'a AurGit,
    declared: &'a Declared,
    archive: &'a Archive,
    previous: Option<&'a HostLock>,
    refresh: bool,
) -> Aur<'a> {
    Aur::new(git, previous, move || {
        Index::load(archive, &declared.aur_url, &user_cache()?, refresh)
    })
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
        let path = target.config_dir.join(LOCK_FILE);
        let mut lockfile = Lockfile::load(&path)?;
        lockfile.hosts.insert(target.host.to_string(), lock.clone());
        lockfile.save(&path)?;
    }
    Ok(unresolved)
}

/// Adds the remotes declared apps use, when running as root with flatpak installed.
fn ensure_flatpak_remotes(declared: &Declared) -> Result<()> {
    if unsafe { libc::geteuid() } != 0 || !flatpak::available() {
        return Ok(());
    }
    let used: BTreeMap<String, flatpak::Remote> = declared
        .flatpak_remotes
        .iter()
        .filter(|(name, _)| declared.flatpak_apps.iter().any(|(remote, _)| remote == *name))
        .map(|(name, remote)| (name.clone(), remote.clone()))
        .collect();
    flatpak::ensure_remotes(&used)
}

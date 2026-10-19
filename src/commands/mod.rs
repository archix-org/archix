mod eject;
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
use archix::lock::{ARCHIVE_REPO, HostLock, Locked};
use archix::pacman::LocalPkg;
use archix::paths::Paths;
use archix::plan::{Change, plan};
use archix::recipes::Recipes;
use archix::repos::{Custom, Repo, is_official, ordered};
use archix::resolve::Aur;
use archix::value::Value;

pub use helper::{edit_installed, install_temporarily, pick, prefetch, search, upgrade};
pub use init::init;
pub use lock::{defer_config_writes, deferred_overlay, diff, lock, status};
pub use recover::recover;

/// `archix secure-boot setup`.
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
    missing == 0
}
pub use eject::eject;
pub use switch::{boot_check, cache_push, declare_and_switch, gc, generations, hook_record, rescue, rollback, switch};

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
    prefer: Vec<String>,
    repos: Vec<Repo>,
    third_party_fallback: ThirdPartyFallback,
    remove_undeclared: bool,
    ignored: BTreeSet<String>,
    keep: u32,
    keep_days: u32,
    aur_url: String,
    aur_review: bool,
    archive_url: String,
    lock_warn_after_days: u32,
    package_caches: Vec<String>,
    build_on_host: BTreeSet<String>,
    skip_check: BTreeSet<String>,
    missing_versions: Option<Nearest>,
    boot: Option<BootSettings>,
    snapshots: bool,
    /// (remote, app id) pairs.
    flatpak_apps: Vec<(String, String)>,
    flatpak_remotes: BTreeMap<String, flatpak::Remote>,
    /// Official packages to rebuild with these patch files, relative to the config directory.
    patches: BTreeMap<String, Vec<String>>,
}

/// The boot.* settings, present when archix manages booting.
struct BootSettings {
    esp: String,
    kernel: String,
    microcode: Vec<String>,
    entries: u32,
    secure_boot: Option<bool>,
    cmdline: String,
    protected_entry: bool,
    tries: u32,
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
        prefer: strings("prefer"),
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
        package_caches: strings("package_caches"),
        lock_warn_after_days: match map.get("lock_warn_after_days") {
            Some(Value::Int(days)) => u32::try_from(*days).unwrap_or(0),
            _ => 30,
        },
        archive_url: match map.get("archive_url") {
            Some(Value::Str(url)) if !url.is_empty() => url.trim_end_matches('/').to_string(),
            _ => archix::archive::ARCHIVE_URL.to_string(),
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
        skip_check: match map.get("aur") {
            Some(Value::Map(aur)) => match aur.get("skip_check") {
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
                    protected_entry: boot.get("protected_entry") == Some(&Value::Bool(true)),
                    tries: match boot.get("tries") {
                        Some(Value::Int(n)) => u32::try_from(*n).unwrap_or(0),
                        _ => 3,
                    },
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
        // A boot-time rollback reinstalls the kernel, and DKMS can only rebuild its modules for it with that kernel's headers.
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

/// Changes that turn `installed` into `packages`; removals skip ignored packages and are only warned about in warn mode, and keyrings never go back, which the third list names.
fn select_changes(
    packages: &BTreeMap<String, Locked>,
    installed: &BTreeMap<String, LocalPkg>,
    remove_undeclared: bool,
    ignored: &BTreeSet<String>,
) -> (Vec<Change>, Vec<String>, Vec<String>) {
    let keyrings = archix::pacman::keyring_packages(&Paths::system().local_db());
    let mut warned = Vec::new();
    let mut kept = Vec::new();
    let changes = plan(packages, installed)
        .into_iter()
        .filter(|change| match change {
            Change::Remove { name, .. } if ignored.contains(name) => false,
            Change::Remove { name, .. } if !remove_undeclared => {
                warned.push(name.clone());
                false
            }
            Change::Downgrade { name, from, to } if keyrings.contains(name) => {
                kept.push(format!(
                    "keeping {name} {from} rather than going back to {to}: an older keyring trusts keys revoked since"
                ));
                false
            }
            _ => true,
        })
        .collect();
    (changes, warned, kept)
}

fn print_warned(warned: &[String]) {
    if !warned.is_empty() {
        println!(
            "not declared, kept because undeclared_packages = \"warn\": {}",
            warned.join(" ")
        );
    }
}

/// Ignored packages missing here, as on a second machine sharing the config, which archix won't install.
fn print_missing_ignored(ignored: &BTreeSet<String>, installed: &BTreeMap<String, LocalPkg>) {
    let missing: Vec<&str> = ignored
        .iter()
        .filter(|name| !installed.contains_key(*name))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        println!(
            "ignored_packages lists {} installed here, and archix never installs ignored ones: {}",
            count(missing.len(), "package that isn't", "packages that aren't"),
            missing.join(", ")
        );
    }
}

/// A package archix builds from a VCS checkout, which its locked recipe doesn't pin, going by the suffixes AUR helpers' devel modes use.
fn floats(name: &str, pkg: &Locked) -> bool {
    pkg.is_built()
        && ["-git", "-hg", "-svn", "-bzr", "-fossil", "-darcs"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

/// Where to get a locked package's file: archix's own build cache for built packages, else its repo at the lock's date and the archive's permanent copy.
fn wanted_file(archive: &Archive, date: Date, name: &str, pkg: &Locked, repos: &[Repo]) -> Wanted {
    if pkg.is_built() {
        return Wanted {
            name: name.to_string(),
            file: pkg.file.clone(),
            urls: Vec::new(),
            sha256: (!pkg.sha256.is_empty()).then(|| pkg.sha256.clone()),
        };
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
    Wanted {
        name: name.to_string(),
        file: pkg.file.clone(),
        urls,
        sha256: Some(pkg.sha256.clone()),
    }
}

/// Third-party repos usually keep only their latest version, so a generation keeps a copy of each package it takes from one, also one archix didn't install itself, for rollbacks to install.
fn keep_third_party_copies(
    archive: &Archive,
    paths: &Paths,
    date: Date,
    packages: &BTreeMap<String, Locked>,
    repos: &[Repo],
) {
    for (name, pkg) in packages {
        let third_party = repos.iter().any(|repo| repo.name == pkg.repo && repo.custom.is_some());
        if !third_party || pkg.is_built() || paths.packages().join(&pkg.file).is_file() {
            continue;
        }
        let wanted = wanted_file(archive, date, name, pkg, repos);
        if let Err(e) = fetch_all(
            archive,
            &paths.packages(),
            std::slice::from_ref(&paths.pacman_cache),
            &[wanted],
        ) {
            eprintln!(
                "warning: couldn't keep a copy of {name} {}, so a rollback to this generation needs {} to still have it: {e:#}",
                pkg.version, pkg.repo
            );
        }
    }
}

/// The third-party repos in pacman.conf, which archix keeps in step with the config, for rollbacks that don't evaluate it.
fn third_party_repos(paths: &Paths) -> Vec<Repo> {
    let text = std::fs::read_to_string(&paths.pacman_conf).unwrap_or_default();
    let (_, custom) = archix::capture::parse_pacman_conf(&text, |path| std::fs::read_to_string(path).ok());
    custom
        .into_iter()
        .map(|(name, custom)| Repo {
            name,
            custom: Some(custom),
        })
        .collect()
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
            Change::Install { name, .. }
            | Change::Upgrade { name, .. }
            | Change::Downgrade { name, .. }
            | Change::Rebuild { name, .. } => {
                let pkg = &packages[name];
                if let Some(path) = built.get(name) {
                    install.push(path.clone());
                    continue;
                }
                wanted.push(wanted_file(archive, date, name, pkg, repos));
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
        config: Vec::new(),
        reinstall: changes.iter().any(|change| matches!(change, Change::Rebuild { .. })),
        at_boot: false,
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

/// A warning when the lock's snapshot is older than the config allows: fixes released since aren't installed.
fn lock_age_warning(lock: &HostLock, warn_after_days: u32) -> Option<String> {
    let age = lock.date.days_until(archix::date::Date::today_utc());
    (warn_after_days > 0 && age > i64::from(warn_after_days)).then(|| {
        format!(
            "warning: archix.lock is at the {} snapshot, {age} days old; updates released since aren't installed. `archix update` moves it to the newest snapshot",
            lock.date
        )
    })
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

/// Like `confirm`, but an empty answer means no.
fn confirm_no(question: &str) -> Result<bool> {
    eprint!("{question} [y/N] ");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    let answer = answer.trim().to_lowercase();
    Ok(answer == "y" || answer == "yes")
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

/// The config's local recipes and patched packages.
fn recipes(target: &Target, declared: &Declared) -> Result<Recipes> {
    Recipes::load(target.config_dir, &declared.patches, &user_cache()?)
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

#[cfg(test)]
mod tests {
    use super::*;
    use archix::eval;
    use std::fs;

    #[test]
    fn a_rollback_takes_third_party_servers_from_pacman_conf() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        fs::create_dir_all(paths.pacman_conf.parent().unwrap()).unwrap();
        fs::write(
            &paths.pacman_conf,
            "[options]\n\n[core]\nInclude = /etc/pacman.d/mirrorlist\n\n[archix-self]\nSigLevel = Optional TrustAll\nServer = file:///srv/archix-repo\n",
        )
        .unwrap();
        let locked = Locked {
            version: "1-1".into(),
            repo: "archix-self".into(),
            file: "archix-1-1-x86_64.pkg.tar.zst".into(),
            sha256: "00".into(),
            ..Default::default()
        };
        let archive = Archive::new("https://archive.example");
        let wanted = wanted_file(
            &archive,
            "2026-10-10".parse().unwrap(),
            "archix",
            &locked,
            &third_party_repos(&paths),
        );
        assert_eq!(wanted.urls, ["file:///srv/archix-repo/archix-1-1-x86_64.pkg.tar.zst"]);
    }

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
}

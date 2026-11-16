pub mod diff;
mod helper;
mod init;
mod lock;
pub mod switch;

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use archix::archive::Archive;
use archix::aur;
use archix::aur::git::AurGit;
use archix::aur::index::Index;
use archix::date::Date;
use archix::flatpak;
use archix::lock::{HostLock, Locked};
use archix::paths::Paths;
use archix::recipes::Recipes;
use archix::repos::{Custom, Repo, is_official, ordered};
use archix::resolve::Aur;
use archix::value::Value;

pub use helper::{prefetch, search};
pub use init::init;
pub use lock::lock;

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
    /// (remote, app id) pairs.
    flatpak_apps: Vec<(String, String)>,
    flatpak_remotes: BTreeMap<String, flatpak::Remote>,
    /// Official packages to rebuild with these patch files, relative to the config directory.
    patches: BTreeMap<String, Vec<String>>,
}

impl Declared {
    fn repo_names(&self) -> Vec<String> {
        self.repos.iter().map(|repo| repo.name.clone()).collect()
    }
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
    fn packages_are_sorted_once_and_flatpak_apps_bring_flathub_unless_they_name_a_remote() {
        let declared = declared_from(
            r#"return { packages = { "zsh", "base", "zsh" }, flatpak = { apps = { "org.a.App", "mine:org.b.App" }, remotes = { mine = { url = "https://example.org/repo", gpg_verify = false } } } }"#,
        );
        assert_eq!(
            declared.packages,
            ["base", "flatpak", "zsh"],
            "flatpak apps bring flatpak"
        );
        assert_eq!(
            declared.flatpak_apps,
            [
                ("flathub".to_string(), "org.a.App".to_string()),
                ("mine".to_string(), "org.b.App".to_string())
            ]
        );
        assert_eq!(declared.flatpak_remotes["flathub"].url, FLATHUB);
        assert!(!declared.flatpak_remotes["mine"].gpg_verify);
    }
}

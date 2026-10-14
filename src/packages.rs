use std::collections::BTreeSet;
use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use archix::archive::{ARCHIVE_URL, Archive};
use archix::lock::{HostLock, LOCK_FILE, Lockfile};
use archix::pacman::read_local_db;
use archix::plan::plan;
use archix::resolve::resolve;
use archix::value::Value;

const LOCAL_DB: &str = "/var/lib/pacman/local";

pub struct Target<'a> {
    pub config_dir: &'a Path,
    pub host: &'a str,
    pub config: &'a Value,
}

struct Declared {
    packages: Vec<String>,
    repos: Vec<String>,
}

fn declared(config: &Value) -> Result<Declared> {
    let strings = |key: &str| -> Vec<String> {
        let Value::Map(map) = config else { return Vec::new() };
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
    let mut repos = Vec::new();
    for repo in strings("repos") {
        if !repos.contains(&repo) {
            repos.push(repo);
        }
    }
    if repos.is_empty() {
        bail!("`repos` is empty; packages need at least one repository");
    }
    Ok(Declared { packages, repos })
}

/// Resolves the declared packages and saves them to the lock; `update` moves to the newest snapshot.
pub fn lock(target: &Target, update: bool) -> Result<()> {
    let declared = declared(target.config)?;
    let path = target.config_dir.join(LOCK_FILE);
    let mut lockfile = Lockfile::load(&path)?;
    let old = lockfile.hosts.get(target.host);
    let archive = Archive::new(ARCHIVE_URL);
    let date = match (update, old) {
        (false, Some(old)) => old.date,
        _ => archive.latest()?,
    };
    if let Some(old) = old
        && old.date == date
        && old.matches(&declared.packages, &declared.repos)
    {
        println!("{LOCK_FILE} is up to date for {} (snapshot {date})", target.host);
        return Ok(());
    }
    eprintln!(
        "resolving {} declared packages against the {date} snapshot",
        declared.packages.len()
    );
    let new = resolve(&archive, &cache_dir()?, date, &declared.repos, &declared.packages)?;
    for line in summarize(old, &new) {
        println!("{line}");
    }
    lockfile.hosts.insert(target.host.to_string(), new);
    lockfile.save(&path)?;
    println!("wrote {}", path.display());
    Ok(())
}

/// Shows what a switch would change on this machine to match the lock.
pub fn diff(target: &Target) -> Result<()> {
    let declared = declared(target.config)?;
    let lockfile = Lockfile::load(&target.config_dir.join(LOCK_FILE))?;
    let lock = match lockfile.hosts.get(target.host) {
        Some(lock) if lock.matches(&declared.packages, &declared.repos) => lock.clone(),
        existing => {
            let archive = Archive::new(ARCHIVE_URL);
            let date = match existing {
                Some(lock) => lock.date,
                None => archive.latest()?,
            };
            eprintln!(
                "note: {LOCK_FILE} is out of date for {}; showing a fresh resolution (run `archix lock` to save it)",
                target.host
            );
            resolve(&archive, &cache_dir()?, date, &declared.repos, &declared.packages)?
        }
    };
    let installed = read_local_db(Path::new(LOCAL_DB))?;
    let changes = plan(&lock.packages, &installed);
    println!("snapshot {}: {} packages", lock.date, lock.packages.len());
    if changes.is_empty() {
        println!("the system matches the lock");
    }
    for change in changes {
        println!("{change}");
    }
    Ok(())
}

fn summarize(old: Option<&HostLock>, new: &HostLock) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(old) = old
        && old.date != new.date
    {
        lines.push(format!("snapshot {} -> {}", old.date, new.date));
    }
    let empty = Default::default();
    let old_packages = old.map_or(&empty, |old| &old.packages);
    for (name, pkg) in &new.packages {
        match old_packages.get(name) {
            None => lines.push(format!("+ {name} {}", pkg.version)),
            Some(before) if before.version != pkg.version => {
                lines.push(format!("~ {name} {} -> {}", before.version, pkg.version))
            }
            Some(_) => {}
        }
    }
    for (name, pkg) in old_packages {
        if !new.packages.contains_key(name) {
            lines.push(format!("- {name} {}", pkg.version));
        }
    }
    lines.push(format!("{} packages locked", new.packages.len()));
    lines
}

/// Root shares `/var/cache/archix`; other users get their own cache, since snapshots are immutable either way.
fn cache_dir() -> Result<PathBuf> {
    if unsafe { libc::geteuid() } == 0 {
        return Ok(PathBuf::from("/var/cache/archix"));
    }
    if let Some(dir) = env::var_os("XDG_CACHE_HOME").filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir).join("archix"));
    }
    match env::var_os("HOME") {
        Some(home) => Ok(PathBuf::from(home).join(".cache/archix")),
        None => bail!("neither XDG_CACHE_HOME nor HOME is set"),
    }
}

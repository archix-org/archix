use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result};

use archix::archive::Archive;
use archix::aur::review::review;
use archix::lock::{HostLock, LOCK_FILE, Lockfile};
use archix::resolve::resolve;

use super::{
    Declared, Snapshot, Target, aur_git, aur_source, confirm_or, count, declared, floats, recipes, settle_flatpak,
    user_cache,
};

/// One config file's text as this command wrote it.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ConfigWrite {
    path: std::path::PathBuf,
    text: String,
}

/// The host's lock, re-resolved at the locked date (or `snapshot`) if the config changed; saved when `save`.
pub(super) fn current_lock(
    target: &Target,
    declared: &Declared,
    snapshot: Snapshot,
    save: bool,
    noconfirm: bool,
    for_this_cpu: bool,
) -> Result<(HostLock, bool)> {
    let mut lockfile = load_lockfile(target)?;
    let old = lockfile.hosts.get(target.host).cloned();
    let archive = Archive::new(&declared.archive_url)
        .with_mirrors(&declared.archive_mirrors)
        .with_caches(&declared.package_caches);
    let updating = !matches!(snapshot, Snapshot::Locked);
    let date = match (snapshot, &old) {
        (Snapshot::On(date), _) => date,
        (Snapshot::Locked, Some(old)) => old.date,
        (Snapshot::Locked | Snapshot::Latest, _) => archive.latest()?,
    };
    let recipes = recipes(target, declared)?;
    if let Some(old) = &old
        && !updating
        && old.date == date
        && old.matches(
            &declared.packages,
            &declared.prefer,
            &declared.repo_names(),
            &recipes.fingerprints(&old.local_recipes())?,
        )
        // Only `archix lock` re-resolves a lock made for a newer CPU; switch refuses it instead of rewriting the lock it applies.
        && (!for_this_cpu
            || archix::archive::beyond_cpu(
                old.packages
                    .iter()
                    .map(|(name, pkg)| (name.as_str(), pkg.file.as_str())),
                &archix::archive::architectures(),
            )
            .is_empty())
    {
        return Ok((old.clone(), false));
    }
    eprintln!(
        "resolving {} declared packages against the {date} snapshot",
        declared.packages.len()
    );
    let git = aur_git(declared)?;
    let keep_commits = if updating { None } else { old.as_ref() };
    let aur = aur_source(&git, declared, &archive, keep_commits, updating, &recipes);
    let resolve_with = |prefer: &[String]| {
        resolve(
            &archive,
            &user_cache()?,
            date,
            &declared.repos,
            &declared.packages,
            prefer,
            Some(&aur),
        )
    };
    let mut new = resolve_with(&declared.prefer)?;
    // DKMS builds its modules for every installed kernel that has headers, and only the resolution shows a DKMS module that came in as a dependency.
    if new.packages.contains_key("dkms") {
        let (_, db) = archix::resolve::snapshot_conf(&archive, &user_cache()?, date, &declared.repos)?;
        let in_repos: BTreeSet<String> = declared
            .repos
            .iter()
            .filter_map(|repo| archix::pacman::read_sync_db(&db.join("sync").join(format!("{}.db", repo.name))).ok())
            .flat_map(|packages| packages.into_keys())
            .collect();
        let headers: Vec<String> = kernel_headers(new.packages.keys(), &in_repos)
            .into_iter()
            .filter(|headers| !new.packages.contains_key(headers))
            .collect();
        if !headers.is_empty() {
            let mut prefer = declared.prefer.clone();
            prefer.extend(headers);
            new = resolve_with(&prefer)?;
            new.prefer = declared.prefer.clone();
        }
    }
    if let Some(old) = &old {
        new.keep_pins(old);
        new.flatpak = old.flatpak.clone();
    }
    if !save {
        eprintln!(
            "note: {LOCK_FILE} is out of date for {}; showing a fresh resolution",
            target.host
        );
        return Ok((new, true));
    }
    review(&git, old.as_ref(), &new, declared.aur_review && !noconfirm, &confirm_or)?;
    save_lock(target, &mut lockfile, old.as_ref(), &new)?;
    Ok((new, true))
}

/// The headers DKMS needs: those of every kernel among `packages` (a linux* package with a -headers package in the repos).
fn kernel_headers<'a>(packages: impl Iterator<Item = &'a String>, in_repos: &BTreeSet<String>) -> BTreeSet<String> {
    packages
        .filter(|name| name.starts_with("linux") && !name.ends_with("-headers"))
        .map(|name| format!("{name}-headers"))
        .filter(|headers| in_repos.contains(headers))
        .collect()
}

pub(super) fn save_lock(
    target: &Target,
    lockfile: &mut Lockfile,
    old: Option<&HostLock>,
    new: &HostLock,
) -> Result<()> {
    for line in summarize(old, new) {
        println!("{line}");
    }
    let path = target.config_dir.join(LOCK_FILE);
    lockfile.hosts.insert(target.host.to_string(), new.clone());
    write_config(&path, lockfile.render().as_bytes())?;
    println!("wrote {}", path.display());
    archix::testhooks::crash_at("lock-resolved");
    Ok(())
}

thread_local! {
    /// What this command already wrote to config files, which it reads back instead of the live directory.
    static WRITTEN: std::cell::RefCell<Vec<ConfigWrite>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// archix.lock as this command sees it: what it deferred or wrote, else the capture's when root, else what's on disk; a root command never reads the live directory, which can change after the diff.
pub(super) fn lock_text(target: &Target) -> Result<Option<String>> {
    let path = target.config_dir.join(LOCK_FILE);
    if let Some(text) = written_text(&path) {
        return Ok(Some(text));
    }
    let source = target.tree.join(LOCK_FILE);
    match std::fs::read_to_string(&source) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if target.captured.is_some() && path.symlink_metadata().is_ok() {
                anyhow::bail!(
                    "{} exists but isn't in the capture of the config, so it must be in a .gitignore; the lock is part of a generation, so stop ignoring it",
                    path.display()
                );
            }
            Ok(None)
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", source.display())),
    }
}

pub(super) fn load_lockfile(target: &Target) -> Result<Lockfile> {
    let path = target.config_dir.join(LOCK_FILE);
    match lock_text(target)? {
        Some(text) => Lockfile::parse(&text, &path),
        None => Ok(Lockfile::default()),
    }
}

/// Writes archix.lock, or inside a switch defers it to the switch's commit.
pub(super) fn store_lockfile(target: &Target, lockfile: &Lockfile) -> Result<()> {
    let path = target.config_dir.join(LOCK_FILE);
    write_config(&path, lockfile.render().as_bytes())?;
    Ok(())
}

fn written_text(path: &Path) -> Option<String> {
    WRITTEN.with(|written| {
        written
            .borrow()
            .iter()
            .find(|write| write.path == path)
            .map(|write| write.text.clone())
    })
}

/// Writes a config file, as its directory's owner when that isn't root.
pub(super) fn write_config(path: &Path, text: &[u8]) -> Result<()> {
    WRITTEN.with(|written| {
        let mut written = written.borrow_mut();
        written.retain(|write| write.path != path);
        written.push(ConfigWrite {
            path: path.to_path_buf(),
            text: String::from_utf8_lossy(text).into_owned(),
        });
    });
    if owned_by_someone_else(path) {
        archix::captured::write_as_owner(path, text)
    } else {
        archix::durable::write_config(path, text)
    }
}

/// Whether root is about to write into a directory another account owns, the nearest one that exists when the file's is still to be made, which it then does as that account.
fn owned_by_someone_else(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    (unsafe { libc::geteuid() }) == 0
        && path
            .ancestors()
            .skip(1)
            .find_map(|dir| std::fs::metadata(dir).ok())
            .is_some_and(|meta| meta.uid() != 0)
}

pub fn lock(target: &Target, snapshot: Snapshot, noconfirm: bool) -> Result<()> {
    let declared = declared(target.config)?;
    let refresh = !matches!(snapshot, Snapshot::Locked);
    let before = load_lockfile(target)?
        .hosts
        .get(target.host)
        .map(|lock| lock.flatpak.clone())
        .unwrap_or_default();
    let (mut lock, changed) = current_lock(target, &declared, snapshot, true, noconfirm, true)?;
    for app in settle_flatpak(target, &declared, &mut lock, true, refresh)? {
        println!("note: {app}; the next `archix switch` locks it");
    }
    for (id, app) in &lock.flatpak {
        if before.get(id) != Some(app) {
            println!("flatpak {id} at {}", &app.commit[..app.commit.len().min(12)]);
        }
    }
    if !changed && lock.flatpak == before {
        println!("{LOCK_FILE} is up to date for {} (snapshot {})", target.host, lock.date);
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
        let note = if floats(name, pkg) {
            " (builds from a VCS source; its first build records the commit)"
        } else {
            ""
        };
        match old_packages.get(name) {
            None => lines.push(format!("+ {name} {}{note}", pkg.version)),
            Some(before) if before.version != pkg.version => {
                lines.push(format!("~ {name} {} -> {}{note}", before.version, pkg.version))
            }
            Some(_) => {}
        }
    }
    for (name, pkg) in old_packages {
        if !new.packages.contains_key(name) {
            lines.push(format!("- {name} {}", pkg.version));
        }
    }
    lines.push(format!("{} locked", count(new.packages.len(), "package", "packages")));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dkms_gets_the_headers_of_every_installed_kernel_the_repos_have_them_for() {
        let names = |list: &[&str]| -> Vec<String> { list.iter().map(|s| s.to_string()).collect() };
        let packages = names(&[
            "linux",
            "linux-lts",
            "linux-firmware",
            "linux-api-headers",
            "dkms",
            "glibc",
        ]);
        let in_repos: BTreeSet<String> = names(&["linux-headers", "linux-lts-headers", "linux-zen-headers"])
            .into_iter()
            .collect();
        let headers: Vec<String> = kernel_headers(packages.iter(), &in_repos).into_iter().collect();
        assert_eq!(headers, ["linux-headers", "linux-lts-headers"]);
        let cachyos = names(&["linux-cachyos", "linux-cachyos-lts"]);
        let in_repos: BTreeSet<String> = names(&["linux-cachyos-headers"]).into_iter().collect();
        let headers: Vec<String> = kernel_headers(cachyos.iter(), &in_repos).into_iter().collect();
        assert_eq!(
            headers,
            ["linux-cachyos-headers"],
            "a kernel without headers in the repos gets none"
        );
    }
}

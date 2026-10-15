use std::collections::BTreeSet;

use anyhow::Result;

use archix::adopt::newer_than_lock;
use archix::archive::{ARCHIVE_URL, Archive};
use archix::aur::review::review;
use archix::flatpak;
use archix::generations::Store;
use archix::integration;
use archix::journal::Journal;
use archix::lock::{HostLock, LOCK_FILE, Lockfile};
use archix::pacman::read_local_db;
use archix::paths::Paths;
use archix::plan::rebuilds;
use archix::resolve::resolve;
use archix::system::{self, apply::SystemChange};

use super::{
    Declared, Snapshot, Target, aur_git, aur_source, confirm, count, declared, describe_unfinished, print_warned,
    recipes, refuse_if_unfinished, select_changes, settle_flatpak, user_cache,
};

/// The host's lock, re-resolved at the locked date (or `snapshot`) if the config changed; saved when `save`.
pub(super) fn current_lock(
    target: &Target,
    declared: &Declared,
    snapshot: Snapshot,
    save: bool,
    noconfirm: bool,
) -> Result<(HostLock, bool)> {
    let mut lockfile = Lockfile::load(&target.config_dir.join(LOCK_FILE))?;
    let old = lockfile.hosts.get(target.host).cloned();
    let archive = Archive::new(ARCHIVE_URL);
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
            &recipes.fingerprints()?,
        )
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
    let mut new = resolve(
        &archive,
        &user_cache()?,
        date,
        &declared.repos,
        &declared.packages,
        &declared.prefer,
        Some(&aur),
    )?;
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
    review(&git, old.as_ref(), &new, declared.aur_review && !noconfirm, &confirm)?;
    save_lock(target, &mut lockfile, old.as_ref(), &new)?;
    Ok((new, true))
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
    lockfile.save(&path)?;
    println!("wrote {}", path.display());
    Ok(())
}

pub fn lock(target: &Target, snapshot: Snapshot, noconfirm: bool) -> Result<()> {
    refuse_if_unfinished(&Paths::system())?;
    let declared = declared(target.config)?;
    let refresh = !matches!(snapshot, Snapshot::Locked);
    let before = Lockfile::load(&target.config_dir.join(LOCK_FILE))?
        .hosts
        .get(target.host)
        .map(|lock| lock.flatpak.clone())
        .unwrap_or_default();
    let (mut lock, changed) = current_lock(target, &declared, snapshot, true, noconfirm)?;
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

/// What switching to `lock` would change, one line each, plus what's only kept because of warn mode.
pub(super) fn pending(target: &Target, declared: &Declared, lock: &HostLock) -> Result<(Vec<String>, Vec<String>)> {
    let paths = Paths::system();
    let installed = read_local_db(&paths.local_db())?;
    let (changes, warned) = select_changes(
        &lock.packages,
        &installed,
        declared.remove_undeclared,
        &declared.ignored,
    );
    let store = Store::new(&paths);
    let current = store.current()?.map(|n| store.get(n)).transpose()?;
    let rendered = system::render(target.config, target.config_dir)?;
    let previous_flatpak = current.as_ref().map(|g| g.flatpak.clone()).unwrap_or_default();
    let change = SystemChange {
        from: current.as_ref().map(|g| g.system.clone()).unwrap_or_default(),
        to: rendered.state,
    };
    let mut lines: Vec<String> = changes.iter().map(ToString::to_string).collect();
    let built_from = current.as_ref().map(|g| &g.packages);
    lines.extend(
        rebuilds(&lock.packages, &installed, built_from)
            .iter()
            .map(ToString::to_string),
    );
    lines.extend(system::apply::describe(&change)?);
    let mut lock = lock.clone();
    let unresolved = settle_flatpak(target, declared, &mut lock, false, false)?;
    let (flatpak_lines, unmanaged) = flatpak::describe(&lock.flatpak, &previous_flatpak, &flatpak::installed()?);
    lines.extend(flatpak_lines);
    lines.extend(unresolved.iter().map(|app| format!("flatpak    install {app}")));
    if !unmanaged.is_empty() {
        println!("flatpak apps archix doesn't manage: {}", unmanaged.join(" "));
    }
    Ok((lines, warned))
}

pub fn diff(target: &Target) -> Result<()> {
    let paths = Paths::system();
    let declared = declared(target.config)?;
    let (lock, _) = current_lock(target, &declared, Snapshot::Locked, false, true)?;
    let installed = read_local_db(&paths.local_db())?;
    println!(
        "snapshot {}: {}",
        lock.date,
        count(lock.packages.len(), "package", "packages")
    );
    let newer = newer_than_lock(&lock, &installed).len();
    if newer > 0 {
        println!(
            "{} newer than the lock; `archix switch` will offer to adopt them instead of downgrading",
            count(newer, "package is", "packages are")
        );
    }
    let (lines, warned) = pending(target, &declared, &lock)?;
    if lines.is_empty() {
        println!("the system matches the lock and the config");
    }
    for line in lines {
        println!("{line}");
    }
    print_warned(&warned);
    Ok(())
}

pub fn status(target: &Target) -> Result<()> {
    let paths = Paths::system();
    match Journal::load(&paths.journal())? {
        Some(journal) => println!(
            "{}\nrun `archix recover` to deal with it",
            describe_unfinished(&journal)
        ),
        None => println!("no unfinished switch"),
    }
    let store = Store::new(&paths);
    match store.current()? {
        Some(number) => {
            let generation = store.get(number)?;
            println!("current generation: {number} (snapshot {})", generation.date);
        }
        None => println!("no generation yet; `archix switch` creates the first one"),
    }
    let outside = integration::outside_changes(&paths)?;
    if !outside.is_empty() {
        let packages: BTreeSet<&String> = outside.iter().flat_map(|change| &change.packages).collect();
        println!(
            "{} changed outside archix in {} since the last switch",
            count(packages.len(), "package was", "packages were"),
            count(outside.len(), "transaction", "transactions")
        );
    }
    let lockfile = Lockfile::load(&target.config_dir.join(LOCK_FILE))?;
    let Some(lock) = lockfile.hosts.get(target.host) else {
        println!("{LOCK_FILE} has no entry for {}; run `archix lock`", target.host);
        return Ok(());
    };
    println!(
        "{} is locked to snapshot {} ({})",
        target.host,
        lock.date,
        count(lock.packages.len(), "package", "packages")
    );
    let declared = declared(target.config)?;
    let fingerprints = recipes(target, &declared)?.fingerprints()?;
    if !lock.matches(
        &declared.packages,
        &declared.prefer,
        &declared.repo_names(),
        &fingerprints,
    ) {
        println!("the config changed since the lock was written; `archix lock` or `archix switch` will re-resolve");
    }
    if paths.local_db().is_dir() {
        let installed = read_local_db(&paths.local_db())?;
        let (changes, _) = select_changes(
            &lock.packages,
            &installed,
            declared.remove_undeclared,
            &declared.ignored,
        );
        match changes.len() {
            0 => println!("the system matches the lock"),
            n => println!("{} pending; see `archix diff`", count(n, "change", "changes")),
        }
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
    lines.push(format!("{} locked", count(new.packages.len(), "package", "packages")));
    lines
}

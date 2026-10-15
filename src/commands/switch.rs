use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use archix::adopt::{adopt, newer_than_lock};
use archix::apply;
use archix::archive::{ARCHIVE_URL, Archive};
use archix::aur::build::Builder;
use archix::aur::install::{BuildFailed, Installer, nearest};
use archix::aur::review::review;
use archix::date::Date;
use archix::fetch::sha256;
use archix::gc;
use archix::generations::{Generation, Settings, Store, config_commit, now};
use archix::integration;
use archix::journal::{Journal, Kind};
use archix::lock::{AUR_REPO, HostLock, LOCK_FILE, Locked, Lockfile};
use archix::pacman::read_local_db;
use archix::paths::Paths;
use archix::plan::Change;

use super::lock::{current_lock, save_lock};
use super::{
    Declared, Nearest, Snapshot, Target, aur_git, aur_source, check_pacman_free, confirm, declared, print_warned,
    refuse_if_unfinished, require_root, select_changes, transaction_for, user_cache,
};

pub fn switch(target: &Target, noconfirm: bool, accept_nearest: Option<Nearest>) -> Result<()> {
    require_root("switch")?;
    let paths = Paths::system();
    refuse_if_unfinished(&paths)?;
    check_pacman_free(&paths)?;
    let declared = declared(target.config)?;
    let (mut lock, _) = current_lock(target, &declared, Snapshot::Locked, true, noconfirm)?;
    let installed = read_local_db(&paths.local_db())?;
    let archive = Archive::new(ARCHIVE_URL);

    let newer: Vec<String> = newer_than_lock(&lock, &installed)
        .iter()
        .map(|pkg| pkg.name.clone())
        .collect();
    if !newer.is_empty() {
        let shown: Vec<&str> = newer.iter().take(8).map(String::as_str).collect();
        let more = if newer.len() > shown.len() { ", ..." } else { "" };
        println!(
            "the system is newer than the lock for {} packages ({}{more}), probably from pacman or an AUR helper",
            newer.len(),
            shown.join(", ")
        );
        if noconfirm || confirm("adopt them into the lock? (no reverts them to the lock)")? {
            let git = aur_git(&declared)?;
            let previous = lock.clone();
            let aur = aur_source(&git, &declared, &archive, Some(&previous), false);
            let adopted = adopt(&archive, &user_cache()?, &paths, &previous, &installed, Some(&aur))?;
            let mut lockfile = Lockfile::load(&target.config_dir.join(LOCK_FILE))?;
            save_lock(target, &mut lockfile, Some(&previous), &adopted)?;
            drop(aur);
            lock = adopted;
        }
    }

    let commit = config_commit(target.config_dir);
    if let Some((_, true)) = &commit {
        eprintln!("warning: {} has uncommitted changes", target.config_dir.display());
    }
    let store = Store::new(&paths);
    let current = store.current()?.map(|n| store.get(n)).transpose()?;
    let mut packages = lock.packages.clone();
    carry_aur_hashes(&mut packages, current.as_ref());
    let mut generation = Generation {
        number: 0,
        created: now(),
        host: target.host.to_string(),
        date: lock.date,
        packages,
        settings: Settings {
            remove_undeclared: declared.remove_undeclared,
            ignored: declared.ignored.iter().cloned().collect(),
            keep: declared.keep,
            keep_days: declared.keep_days,
        },
        config_commit: commit.as_ref().map(|(hash, _)| hash.clone()),
        config_dirty: commit.is_some_and(|(_, dirty)| dirty),
    };

    let (changes, warned) = select_changes(
        &lock.packages,
        &installed,
        declared.remove_undeclared,
        &declared.ignored,
    );
    print_warned(&warned);
    if changes.is_empty() {
        if let Some(current) = current.filter(|current| current.same_system(&generation)) {
            finish(&paths)?;
            println!(
                "nothing to do: the system matches generation {} (snapshot {})",
                current.number, lock.date
            );
            return Ok(());
        }
        let created = store.create(generation)?;
        store.activate(created.number)?;
        finish(&paths)?;
        println!(
            "generation {} is current (snapshot {}, no package changes)",
            created.number, lock.date
        );
        return Ok(());
    }
    for change in &changes {
        println!("{change}");
    }
    if !noconfirm && !confirm(&format!("apply {} changes?", changes.len()))? {
        bail!("aborted; nothing was changed");
    }

    let aur_names: Vec<String> = changes
        .iter()
        .filter_map(|change| match change {
            Change::Install { name, .. } | Change::Upgrade { name, .. } | Change::Downgrade { name, .. }
                if lock.packages[name].repo == AUR_REPO =>
            {
                Some(name.clone())
            }
            _ => None,
        })
        .collect();
    let built = if aur_names.is_empty() {
        BTreeMap::new()
    } else {
        build_aur(
            target,
            &declared,
            &paths,
            &archive,
            &mut lock,
            &aur_names,
            noconfirm,
            accept_nearest,
        )?
    };
    for (name, path) in &built {
        if let Some(pkg) = lock.packages.get(name) {
            let mut pkg = pkg.clone();
            pkg.sha256 = sha256(path)?;
            pkg.file = path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or(pkg.file);
            generation.packages.insert(name.clone(), pkg);
        }
    }
    for (name, pkg) in &lock.packages {
        if pkg.repo == AUR_REPO && !built.contains_key(name) {
            let mut pkg = pkg.clone();
            if let Some(previous) = generation.packages.get(name) {
                pkg.sha256 = previous.sha256.clone();
            }
            generation.packages.insert(name.clone(), pkg);
        }
    }
    generation.date = lock.date;

    let transaction = transaction_for(&archive, &paths, lock.date, &lock.packages, &changes, &built)?;
    let created = store.create(generation)?;
    let name = format!("generation {} (snapshot {})", created.number, lock.date);
    let mut journal = Journal::new(Kind::Switch, target.host, name.clone(), installed, transaction);
    journal.generation = Some(created.number);
    journal.save(&paths.journal())?;
    apply::run(&paths, &mut journal, false)?;
    finish(&paths)?;
    println!("switched to {name}");
    Ok(())
}

/// The current generation's built-file hashes for AUR packages still locked at the same commit.
fn carry_aur_hashes(packages: &mut BTreeMap<String, Locked>, current: Option<&Generation>) {
    let Some(current) = current else { return };
    for (name, pkg) in packages
        .iter_mut()
        .filter(|(_, pkg)| pkg.repo == AUR_REPO && pkg.sha256.is_empty())
    {
        if let Some(previous) = current.packages.get(name)
            && previous.commit == pkg.commit
            && previous.version == pkg.version
        {
            pkg.sha256 = previous.sha256.clone();
            pkg.file = previous.file.clone();
        }
    }
}

/// Builds the AUR packages a switch installs; a base that can't be built is moved to the nearest version per policy.
#[allow(clippy::too_many_arguments)]
fn build_aur(
    target: &Target,
    declared: &Declared,
    paths: &Paths,
    archive: &Archive,
    lock: &mut HostLock,
    names: &[String],
    noconfirm: bool,
    accept_nearest: Option<Nearest>,
) -> Result<BTreeMap<String, PathBuf>> {
    let git = aur_git(declared)?;
    let cache = user_cache()?;
    let builder = Builder {
        paths,
        archive,
        git: &git,
        snapshot_cache: &cache,
    };
    let mut moved = BTreeSet::new();
    loop {
        let result =
            Installer::new(&builder, lock, &declared.build_on_host).and_then(|installer| installer.build(names));
        let error = match result {
            Ok(built) => return Ok(built),
            Err(error) => error,
        };
        let Some(failed) = error.downcast_ref::<BuildFailed>() else {
            return Err(error);
        };
        if !moved.insert(failed.base.clone()) {
            return Err(error);
        }
        eprintln!("{failed}");
        let (newer, older) = nearest(&git, &failed.base, &failed.commit, &failed.version)?;
        let policy = accept_nearest.or(declared.missing_versions);
        let pick = match policy {
            Some(Nearest::Newer) => newer,
            Some(Nearest::Older) => older,
            None if noconfirm => newer,
            None => {
                let base = &failed.base;
                if let Some(n) = newer.filter(|(_, info)| {
                    confirm(&format!("use the nearest newer version, {base} {}?", info.version)).unwrap_or(false)
                }) {
                    Some(n)
                } else {
                    older.filter(|(_, info)| {
                        confirm(&format!("use the nearest older version, {base} {}?", info.version)).unwrap_or(false)
                    })
                }
            }
        };
        let Some((commit, info)) = pick else {
            return Err(error.context("no other version was accepted; nothing was changed"));
        };
        let old = lock.clone();
        for (name, pkg) in lock
            .packages
            .iter_mut()
            .filter(|(_, pkg)| pkg.base.as_deref() == Some(failed.base.as_str()))
        {
            let Some(srcpkg) = info.package(name) else {
                bail!("{} {} no longer builds {name}", failed.base, info.version);
            };
            pkg.version = info.version.clone();
            pkg.file = info.file(srcpkg);
            pkg.commit = Some(commit.clone());
        }
        review(&git, Some(&old), lock, declared.aur_review && !noconfirm, &confirm)?;
        let mut lockfile = Lockfile::load(&target.config_dir.join(LOCK_FILE))?;
        save_lock(target, &mut lockfile, Some(&old), lock)?;
    }
}

/// After a successful switch: outside changes are reconciled, integration files are in place, old garbage goes.
fn finish(paths: &Paths) -> Result<()> {
    integration::clear_outside_changes(paths)?;
    integration::ensure(paths)?;
    match gc::collect(paths, &user_cache()?) {
        Ok(report) => print_gc(&report, false),
        Err(e) => eprintln!("warning: garbage collection failed: {e:#}"),
    }
    Ok(())
}

fn print_gc(report: &gc::Report, always: bool) {
    if !report.generations.is_empty() {
        let numbers: Vec<String> = report.generations.iter().map(u32::to_string).collect();
        println!("deleted generations {}", numbers.join(", "));
    }
    if report.files > 0 || report.snapshots > 0 || always {
        println!(
            "freed {:.1} MiB: {} cached package files, {} snapshot databases",
            report.bytes as f64 / 1_048_576.0,
            report.files,
            report.snapshots
        );
    }
}

/// Puts the packages of an earlier generation back; archix.lock stays as it is.
pub fn rollback(to: Option<u32>, noconfirm: bool) -> Result<()> {
    require_root("rollback")?;
    let paths = Paths::system();
    refuse_if_unfinished(&paths)?;
    check_pacman_free(&paths)?;
    let store = Store::new(&paths);
    let current = store
        .current()?
        .context("there are no generations yet; `archix switch` creates the first one")?;
    let number = match to {
        Some(number) => number,
        None => store
            .list()?
            .iter()
            .rev()
            .map(|g| g.number)
            .find(|n| *n < current)
            .context("there is no generation older than the current one")?,
    };
    if number == current {
        println!("generation {number} is already current");
        return Ok(());
    }
    let generation = store.get(number)?;
    let installed = read_local_db(&paths.local_db())?;
    let ignored = generation.settings.ignored.iter().cloned().collect();
    let (changes, warned) = select_changes(
        &generation.packages,
        &installed,
        generation.settings.remove_undeclared,
        &ignored,
    );
    print_warned(&warned);
    let name = format!("generation {number} (snapshot {})", generation.date);
    if changes.is_empty() {
        store.activate(number)?;
        println!("{name} is current; no package changes were needed");
        return Ok(());
    }
    for change in &changes {
        println!("{change}");
    }
    if !noconfirm && !confirm(&format!("roll back to {name} with {} changes?", changes.len()))? {
        bail!("aborted; nothing was changed");
    }
    let archive = Archive::new(ARCHIVE_URL);
    let transaction = transaction_for(
        &archive,
        &paths,
        generation.date,
        &generation.packages,
        &changes,
        &BTreeMap::new(),
    )?;
    let mut journal = Journal::new(Kind::Switch, &generation.host, name.clone(), installed, transaction);
    journal.generation = Some(number);
    journal.save(&paths.journal())?;
    apply::run(&paths, &mut journal, false)?;
    println!("rolled back to {name}; archix.lock is unchanged, so the next `archix switch` returns to the config");
    Ok(())
}

pub fn generations() -> Result<()> {
    let store = Store::new(&Paths::system());
    let current = store.current()?;
    let list = store.list()?;
    if list.is_empty() {
        println!("no generations yet; `archix switch` creates the first one");
    }
    for generation in list {
        let marker = if current == Some(generation.number) { "*" } else { " " };
        let created = Date::from_unix(generation.created);
        let minutes = generation.created % 86_400 / 60;
        let commit = match &generation.config_commit {
            Some(hash) => {
                let dirty = if generation.config_dirty { "+uncommitted" } else { "" };
                format!("  config {}{dirty}", &hash[..hash.len().min(10)])
            }
            None => String::new(),
        };
        println!(
            "{marker} {:>4}  {created} {:02}:{:02} UTC  snapshot {}  {} packages{commit}",
            generation.number,
            minutes / 60,
            minutes % 60,
            generation.date,
            generation.packages.len()
        );
    }
    Ok(())
}

pub fn gc() -> Result<()> {
    require_root("gc")?;
    let report = gc::collect(&Paths::system(), &user_cache()?)?;
    print_gc(&report, true);
    Ok(())
}

/// Run by archix's pacman hook after every transaction; never fails, so it can't break a pacman run.
pub fn hook_record() {
    let paths = Paths::system();
    let packages: Vec<String> = io::stdin()
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .collect();
    match integration::record(&paths, packages) {
        Ok(Some(change)) => println!(
            "archix: recorded {} packages changed outside archix; `archix switch` will offer to adopt them",
            change.packages.len()
        ),
        Ok(None) => {}
        Err(e) => println!("archix: could not record this transaction: {e:#}"),
    }
    if paths.journal().exists() {
        println!("archix: a switch is unfinished; run `sudo archix recover`");
    }
}

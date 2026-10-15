use std::io;

use anyhow::{Context, Result, bail};

use archix::adopt::{adopt, newer_than_lock};
use archix::apply;
use archix::archive::{ARCHIVE_URL, Archive};
use archix::date::Date;
use archix::gc;
use archix::generations::{Generation, Settings, Store, config_commit, now};
use archix::integration;
use archix::journal::{Journal, Kind};
use archix::lock::{LOCK_FILE, Lockfile};
use archix::pacman::read_local_db;
use archix::paths::Paths;

use super::lock::{current_lock, save_lock};
use super::{
    Snapshot, Target, check_pacman_free, confirm, declared, print_warned, refuse_if_unfinished, require_root,
    select_changes, transaction_for, user_cache,
};

pub fn switch(target: &Target, noconfirm: bool) -> Result<()> {
    require_root("switch")?;
    let paths = Paths::system();
    refuse_if_unfinished(&paths)?;
    check_pacman_free(&paths)?;
    let declared = declared(target.config)?;
    let (mut lock, _) = current_lock(target, &declared, Snapshot::Locked, true)?;
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
            let adopted = adopt(&archive, &user_cache()?, &paths, &lock, &installed)?;
            let mut lockfile = Lockfile::load(&target.config_dir.join(LOCK_FILE))?;
            save_lock(target, &mut lockfile, Some(&lock), &adopted)?;
            lock = adopted;
        }
    }

    let commit = config_commit(target.config_dir);
    if let Some((_, true)) = &commit {
        eprintln!("warning: {} has uncommitted changes", target.config_dir.display());
    }
    let generation = Generation {
        number: 0,
        created: now(),
        host: target.host.to_string(),
        date: lock.date,
        packages: lock.packages.clone(),
        settings: Settings {
            remove_undeclared: declared.remove_undeclared,
            ignored: declared.ignored.iter().cloned().collect(),
            keep: declared.keep,
            keep_days: declared.keep_days,
        },
        config_commit: commit.as_ref().map(|(hash, _)| hash.clone()),
        config_dirty: commit.is_some_and(|(_, dirty)| dirty),
    };
    let store = Store::new(&paths);
    let current = store.current()?.map(|n| store.get(n)).transpose()?;

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

    let transaction = transaction_for(&archive, &paths, lock.date, &lock.packages, &changes)?;
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
    let transaction = transaction_for(&archive, &paths, generation.date, &generation.packages, &changes)?;
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

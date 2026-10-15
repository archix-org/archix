use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use archix::adopt::{adopt, newer_than_lock};
use archix::apply::{self, PacmanLock};
use archix::archive::{ARCHIVE_URL, Archive};
use archix::date::Date;
use archix::fetch::{Wanted, fetch_all};
use archix::integration;
use archix::journal::{Journal, Kind, Step, Transaction};
use archix::lock::{ARCHIVE_REPO, HostLock, LOCK_FILE, Lockfile};
use archix::pacman::{LocalPkg, read_local_db};
use archix::paths::Paths;
use archix::plan::{Change, plan};
use archix::resolve::resolve;
use archix::value::Value;

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

struct Declared {
    packages: Vec<String>,
    repos: Vec<String>,
    remove_undeclared: bool,
    ignored: BTreeSet<String>,
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
    let mut repos = Vec::new();
    for repo in strings("repos") {
        if !repos.contains(&repo) {
            repos.push(repo);
        }
    }
    if repos.is_empty() {
        bail!("`repos` is empty; packages need at least one repository");
    }
    let remove_undeclared = map.get("undeclared_packages") != Some(&Value::Str("warn".into()));
    let ignored = strings("ignored_packages").into_iter().collect();
    Ok(Declared {
        packages,
        repos,
        remove_undeclared,
        ignored,
    })
}

/// The host's lock, re-resolved at the locked date (or `snapshot`) if the config changed; saved when `save`.
fn current_lock(target: &Target, declared: &Declared, snapshot: Snapshot, save: bool) -> Result<(HostLock, bool)> {
    let path = target.config_dir.join(LOCK_FILE);
    let mut lockfile = Lockfile::load(&path)?;
    let old = lockfile.hosts.get(target.host);
    let archive = Archive::new(ARCHIVE_URL);
    let date = match (snapshot, old) {
        (Snapshot::On(date), _) => date,
        (Snapshot::Locked, Some(old)) => old.date,
        (Snapshot::Locked | Snapshot::Latest, _) => archive.latest()?,
    };
    if let Some(old) = old
        && old.date == date
        && old.matches(&declared.packages, &declared.repos)
    {
        return Ok((old.clone(), false));
    }
    eprintln!(
        "resolving {} declared packages against the {date} snapshot",
        declared.packages.len()
    );
    let mut new = resolve(&archive, &user_cache()?, date, &declared.repos, &declared.packages)?;
    if let Some(old) = old {
        new.keep_pins(old);
    }
    if !save {
        eprintln!(
            "note: {LOCK_FILE} is out of date for {}; showing a fresh resolution",
            target.host
        );
        return Ok((new, true));
    }
    let old = old.cloned();
    save_lock(target, &mut lockfile, old.as_ref(), &new)?;
    Ok((new, true))
}

fn save_lock(target: &Target, lockfile: &mut Lockfile, old: Option<&HostLock>, new: &HostLock) -> Result<()> {
    for line in summarize(old, new) {
        println!("{line}");
    }
    let path = target.config_dir.join(LOCK_FILE);
    lockfile.hosts.insert(target.host.to_string(), new.clone());
    lockfile.save(&path)?;
    println!("wrote {}", path.display());
    Ok(())
}

pub fn lock(target: &Target, snapshot: Snapshot) -> Result<()> {
    refuse_if_unfinished(&Paths::system())?;
    let declared = declared(target.config)?;
    let (lock, changed) = current_lock(target, &declared, snapshot, true)?;
    if !changed {
        println!("{LOCK_FILE} is up to date for {} (snapshot {})", target.host, lock.date);
    }
    Ok(())
}

/// The lock's changes for this machine, minus ignored packages; undeclared ones only warned about in warn mode.
fn changes_for(
    lock: &HostLock,
    declared: &Declared,
    installed: &BTreeMap<String, LocalPkg>,
) -> (Vec<Change>, Vec<String>) {
    let mut warned = Vec::new();
    let changes = plan(&lock.packages, installed)
        .into_iter()
        .filter(|change| match change {
            Change::Remove { name, .. } if declared.ignored.contains(name) => false,
            Change::Remove { name, .. } if !declared.remove_undeclared => {
                warned.push(name.clone());
                false
            }
            _ => true,
        })
        .collect();
    (changes, warned)
}

pub fn diff(target: &Target) -> Result<()> {
    let paths = Paths::system();
    let declared = declared(target.config)?;
    let (lock, _) = current_lock(target, &declared, Snapshot::Locked, false)?;
    let installed = read_local_db(&paths.local_db())?;
    let (changes, warned) = changes_for(&lock, &declared, &installed);
    println!("snapshot {}: {} packages", lock.date, lock.packages.len());
    let newer = newer_than_lock(&lock, &installed).len();
    if newer > 0 {
        println!(
            "{newer} packages are newer than the lock; `archix switch` will offer to adopt them instead of downgrading"
        );
    }
    if changes.is_empty() {
        println!("the system matches the lock");
    }
    for change in changes {
        println!("{change}");
    }
    print_warned(&warned);
    Ok(())
}

fn print_warned(warned: &[String]) {
    if !warned.is_empty() {
        println!(
            "not declared, kept because undeclared_packages = \"warn\": {}",
            warned.join(" ")
        );
    }
}

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

    let (changes, warned) = changes_for(&lock, &declared, &installed);
    print_warned(&warned);
    if changes.is_empty() {
        finish_switch(&paths)?;
        println!("nothing to do: the system matches snapshot {}", lock.date);
        return Ok(());
    }
    for change in &changes {
        println!("{change}");
    }
    if !noconfirm && !confirm(&format!("apply {} changes?", changes.len()))? {
        bail!("aborted; nothing was changed");
    }

    let mut wanted = Vec::new();
    let mut remove = Vec::new();
    for change in &changes {
        match change {
            Change::Install { name, .. } | Change::Upgrade { name, .. } | Change::Downgrade { name, .. } => {
                let pkg = &lock.packages[name];
                let mut urls = vec![archive.package_url(name, &pkg.file)];
                if pkg.repo != ARCHIVE_REPO {
                    urls.insert(0, format!("{}/{}", archive.repo_url(lock.date, &pkg.repo), pkg.file));
                }
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
    let install = fetch_all(
        &archive,
        &paths.packages(),
        std::slice::from_ref(&paths.pacman_cache),
        &wanted,
    )?;
    let transaction = Transaction {
        install,
        remove,
        versions: lock
            .packages
            .iter()
            .map(|(name, pkg)| (name.clone(), pkg.version.clone()))
            .collect(),
        reasons: lock
            .packages
            .iter()
            .map(|(name, pkg)| (name.clone(), pkg.explicit))
            .collect(),
    };
    let target_name = format!("snapshot {}", lock.date);
    let mut journal = Journal::new(Kind::Switch, target.host, target_name, installed, transaction);
    journal.save(&paths.journal())?;
    apply::run(&paths, &mut journal, false)?;
    finish_switch(&paths)?;
    println!("switched to snapshot {}", lock.date);
    Ok(())
}

/// After a successful switch: outside changes are reconciled, and the hook and login notices are in place.
fn finish_switch(paths: &Paths) -> Result<()> {
    integration::clear_outside_changes(paths)?;
    integration::ensure(paths)
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

pub fn recover(mode: Option<RecoverMode>) -> Result<()> {
    require_root("recover")?;
    let paths = Paths::system();
    let Some(mut journal) = Journal::load(&paths.journal())? else {
        println!("nothing to recover: no switch is unfinished");
        return Ok(());
    };
    println!("{}", describe_unfinished(&journal));
    let mode = match mode {
        Some(mode) => mode,
        None => ask_mode()?,
    };
    match apply::pacman_lock(&paths) {
        PacmanLock::Running => bail!("pacman is running; wait for it to finish, then run `archix recover` again"),
        PacmanLock::Stale => {
            fs::remove_file(paths.pacman_lock()).context("removing pacman's stale lock")?;
            eprintln!("removed pacman's stale lock {}", paths.pacman_lock().display());
        }
        PacmanLock::Free => {}
    }
    let kind = kind_name(journal.kind);
    match mode {
        RecoverMode::Continue => {
            let install_interrupted = !journal.done.contains(&Step::Install);
            apply::run(&paths, &mut journal, install_interrupted)?;
            println!("finished the interrupted {kind} to {}", journal.target);
        }
        RecoverMode::Rollback => {
            let archive = Archive::new(ARCHIVE_URL);
            let installed = read_local_db(&paths.local_db())?;
            let transaction = rollback_transaction(&archive, &paths, &journal, &installed)?;
            let target = format!("the packages from before the {kind} to {}", journal.target);
            let mut rollback = Journal::new(Kind::Rollback, &journal.host, target, installed, transaction);
            rollback.save(&paths.journal())?;
            apply::run(&paths, &mut rollback, true)?;
            println!("rolled back to the packages from before the {kind}");
        }
        RecoverMode::Abandon => {
            let problems = apply::mismatches(&paths, &journal.transaction)?;
            if problems.is_empty() {
                println!("the installed packages already match {}", journal.target);
            } else {
                println!("differences from {}:", journal.target);
                for problem in &problems {
                    println!("  {problem}");
                }
            }
            check_files(&touched(&journal))?;
            Journal::delete(&paths.journal())?;
            println!("closed the journal without changes; `archix diff` shows how the system differs from the lock");
        }
    }
    Ok(())
}

/// Packages the interrupted transaction meant to change, which pacman may have left half-installed.
fn touched(journal: &Journal) -> Vec<String> {
    journal
        .transaction
        .versions
        .iter()
        .filter(|(name, version)| journal.before.get(*name).is_none_or(|pkg| pkg.version != **version))
        .map(|(name, _)| name.clone())
        .chain(journal.transaction.remove.iter().cloned())
        .collect()
}

fn rollback_transaction(
    archive: &Archive,
    paths: &Paths,
    journal: &Journal,
    installed: &BTreeMap<String, LocalPkg>,
) -> Result<Transaction> {
    let touched: BTreeSet<String> = touched(journal).into_iter().collect();
    let mut wanted = Vec::new();
    for (name, pkg) in &journal.before {
        let changed = installed.get(name).is_none_or(|now| now.version != pkg.version);
        if changed || touched.contains(name) {
            let file = format!("{name}-{}-{}.pkg.tar.zst", pkg.version, pkg.arch);
            wanted.push(Wanted {
                name: name.clone(),
                urls: vec![archive.package_url(name, &file)],
                file,
                sha256: None,
            });
        }
    }
    let install = fetch_all(
        archive,
        &paths.packages(),
        std::slice::from_ref(&paths.pacman_cache),
        &wanted,
    )?;
    Ok(Transaction {
        install,
        remove: installed
            .keys()
            .filter(|name| !journal.before.contains_key(*name))
            .cloned()
            .collect(),
        versions: journal
            .before
            .iter()
            .map(|(name, pkg)| (name.clone(), pkg.version.clone()))
            .collect(),
        reasons: journal
            .before
            .iter()
            .map(|(name, pkg)| (name.clone(), pkg.explicit))
            .collect(),
    })
}

fn check_files(names: &[String]) -> Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    println!(
        "checking the files of {} packages the transaction touched:",
        names.len()
    );
    let status = Command::new("pacman")
        .arg("-Qk")
        .args(names)
        .status()
        .context("running pacman -Qk")?;
    if !status.success() {
        println!(
            "some packages are missing files or aren't installed; `archix switch` will reinstall what the lock needs"
        );
    }
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
    let outside = integration::outside_changes(&paths)?;
    if !outside.is_empty() {
        let packages: BTreeSet<&String> = outside.iter().flat_map(|change| &change.packages).collect();
        println!(
            "{} packages were changed outside archix in {} transactions since the last switch",
            packages.len(),
            outside.len()
        );
    }
    let lockfile = Lockfile::load(&target.config_dir.join(LOCK_FILE))?;
    let Some(lock) = lockfile.hosts.get(target.host) else {
        println!("{LOCK_FILE} has no entry for {}; run `archix lock`", target.host);
        return Ok(());
    };
    println!(
        "{} is locked to snapshot {} ({} packages)",
        target.host,
        lock.date,
        lock.packages.len()
    );
    let declared = declared(target.config)?;
    if !lock.matches(&declared.packages, &declared.repos) {
        println!("the config changed since the lock was written; `archix lock` or `archix switch` will re-resolve");
    }
    if paths.local_db().is_dir() {
        let installed = read_local_db(&paths.local_db())?;
        match changes_for(lock, &declared, &installed).0.len() {
            0 => println!("the system matches the lock"),
            n => println!("{n} changes pending; see `archix diff`"),
        }
    }
    Ok(())
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

fn ask_mode() -> Result<RecoverMode> {
    eprint!("[c]ontinue it, [r]oll back, [a]bandon (close the journal without changes), or [q]uit? ");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    match answer.trim().to_lowercase().as_str() {
        "c" | "continue" => Ok(RecoverMode::Continue),
        "r" | "rollback" => Ok(RecoverMode::Rollback),
        "a" | "abandon" => Ok(RecoverMode::Abandon),
        _ => bail!("nothing done"),
    }
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

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Result;

use archix::adopt::newer_than_lock;
use archix::archive::Archive;
use archix::aur::review::review;
use archix::flatpak;
use archix::generations::Store;
use archix::integration;
use archix::journal::{ConfigWrite, Journal};
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
    for_this_cpu: bool,
) -> Result<(HostLock, bool)> {
    let mut lockfile = load_lockfile(target)?;
    let old = lockfile.hosts.get(target.host).cloned();
    let archive = Archive::new(&declared.archive_url).with_caches(&declared.package_caches);
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
    // A DKMS module that came in as a dependency still needs the boot kernel's headers, which only the resolution shows.
    if let Some(boot) = &declared.boot {
        let headers = format!("{}-headers", boot.kernel);
        if new.packages.contains_key("dkms") && !new.packages.contains_key(&headers) {
            let mut prefer = declared.prefer.clone();
            prefer.push(headers);
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
    if !defer_write(&path, &lockfile.render()) {
        lockfile.save(&path)?;
        println!("wrote {}", path.display());
    }
    archix::testhooks::crash_at("lock-resolved");
    Ok(())
}

thread_local! {
    /// While a switch runs, the config files it would write, which wait for it to commit.
    static DEFERRED: std::cell::RefCell<Option<Vec<ConfigWrite>>> = const { std::cell::RefCell::new(None) };
}

/// From here on, config writes wait in memory for the switch's journal, which writes them when the generation becomes current.
pub fn defer_config_writes() {
    DEFERRED.with(|deferred| *deferred.borrow_mut() = Some(Vec::new()));
}

/// Holds `text` for `path` if writes are deferred, replacing an earlier one for it; false if they aren't.
pub(super) fn defer_write(path: &Path, text: &str) -> bool {
    DEFERRED.with(|deferred| match deferred.borrow_mut().as_mut() {
        Some(writes) => {
            writes.retain(|write| write.path != path);
            writes.push(ConfigWrite {
                path: path.to_path_buf(),
                text: text.to_string(),
            });
            true
        }
        None => false,
    })
}

/// archix.lock as this switch sees it: what it deferred, else what's on disk.
pub(super) fn load_lockfile(target: &Target) -> Result<Lockfile> {
    let path = target.config_dir.join(LOCK_FILE);
    match deferred_text(&path) {
        Some(text) => Lockfile::parse(&text, &path),
        None => Lockfile::load(&path),
    }
}

/// Writes archix.lock, or inside a switch defers it to the switch's commit.
pub(super) fn store_lockfile(target: &Target, lockfile: &Lockfile) -> Result<()> {
    let path = target.config_dir.join(LOCK_FILE);
    if !defer_write(&path, &lockfile.render()) {
        lockfile.save(&path)?;
    }
    Ok(())
}

/// The deferred text for `path`, which is what the config holds as far as this switch is concerned.
pub(super) fn deferred_text(path: &Path) -> Option<String> {
    DEFERRED.with(|deferred| {
        deferred
            .borrow()
            .as_ref()?
            .iter()
            .find(|write| write.path == path)
            .map(|write| write.text.clone())
    })
}

/// Deferred .lua files under `config_dir`, as an evaluation overlay, so the config is evaluated as the switch will write it.
pub fn deferred_overlay(config_dir: &Path) -> std::collections::BTreeMap<std::path::PathBuf, String> {
    let root = std::path::absolute(config_dir).unwrap_or_else(|_| config_dir.to_path_buf());
    DEFERRED.with(|deferred| {
        deferred
            .borrow()
            .iter()
            .flatten()
            .filter(|write| write.path.extension().is_some_and(|ext| ext == "lua"))
            .filter_map(|write| {
                let path = std::path::absolute(&write.path).ok()?;
                Some((path.strip_prefix(&root).ok()?.to_path_buf(), write.text.clone()))
            })
            .collect()
    })
}

/// The deferred writes, for the switch's journal; writes are still deferred afterwards.
pub(super) fn take_deferred() -> Vec<ConfigWrite> {
    DEFERRED.with(|deferred| deferred.borrow_mut().as_mut().map(std::mem::take).unwrap_or_default())
}

/// Writes what was deferred, for a switch that commits without a journal, as when no package changes.
pub(super) fn write_deferred() -> Result<()> {
    archix::journal::write_config(&take_deferred())
}

pub fn lock(target: &Target, snapshot: Snapshot, noconfirm: bool) -> Result<()> {
    refuse_if_unfinished(&Paths::system())?;
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
    let (lock, _) = current_lock(target, &declared, Snapshot::Locked, false, true, false)?;
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
    if let Some(state) = archix::system::secureboot::Machine::new().state() {
        println!(
            "Secure Boot: {}",
            match (state.enabled, state.setup_mode) {
                (_, true) => "setup mode (no keys enrolled; `sudo archix secure-boot setup` enrolls them)",
                (true, false) => "on",
                (false, false) => "off",
            }
        );
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
    let lockfile = load_lockfile(target)?;
    let Some(lock) = lockfile.hosts.get(target.host) else {
        println!("{LOCK_FILE} has no entry for {}; run `archix lock`", target.host);
        return Ok(());
    };
    let pins = if lock.pinned.is_empty() {
        String::new()
    } else {
        format!(", {} off the snapshot", count(lock.pinned.len(), "pinned", "pinned"))
    };
    println!(
        "{} is locked to snapshot {} ({}{pins})",
        target.host,
        lock.date,
        count(lock.packages.len(), "package", "packages")
    );
    let declared = declared(target.config)?;
    if let Some(warning) = super::lock_age_warning(lock, declared.lock_warn_after_days) {
        println!("{warning}");
    }
    let fingerprints = recipes(target, &declared)?.fingerprints(&lock.local_recipes())?;
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

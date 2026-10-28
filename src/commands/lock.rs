use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result};

use archix::adopt::newer_than_lock;
use archix::archive::Archive;
use archix::aur::review::review;
use archix::flatpak;
use archix::generations::Store;
use archix::integration;
use archix::lock::{HostLock, LOCK_FILE, Lockfile};
use archix::pacman::read_local_db;
use archix::paths::Paths;
use archix::plan::rebuilds;
use archix::resolve::resolve;
use archix::system::{self, apply::Accounts, apply::SystemChange};

use super::{
    Declared, Snapshot, Target, aur_git, aur_source, confirm, count, declared, floats, print_missing_ignored,
    print_warned, recipes, select_changes, settle_flatpak, user_cache,
};

/// One config file's new text, written when the switch that changed it commits.
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
    // DKMS builds its modules for every kernel in the staging root that has headers, and only the resolution shows a DKMS module that came in as a dependency.
    if let Some(boot) = &declared.boot
        && new.packages.contains_key("dkms")
    {
        let (_, db) = archix::resolve::snapshot_conf(&archive, &user_cache()?, date, &declared.repos)?;
        let in_repos: BTreeSet<String> = declared
            .repos
            .iter()
            .filter_map(|repo| archix::pacman::read_sync_db(&db.join("sync").join(format!("{}.db", repo.name))).ok())
            .flat_map(|packages| packages.into_keys())
            .collect();
        let headers: Vec<String> = kernel_headers(&boot.kernel, new.packages.keys(), &in_repos)
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
    review(&git, old.as_ref(), &new, declared.aur_review && !noconfirm, &confirm)?;
    save_lock(target, &mut lockfile, old.as_ref(), &new)?;
    Ok((new, true))
}

/// The headers DKMS needs: the boot kernel's, and those of every other kernel among `packages` (a linux* package with a -headers package in the repos).
fn kernel_headers<'a>(
    boot_kernel: &str,
    packages: impl Iterator<Item = &'a String>,
    in_repos: &BTreeSet<String>,
) -> BTreeSet<String> {
    let others = packages
        .filter(|name| name.starts_with("linux") && !name.ends_with("-headers") && *name != boot_kernel)
        .map(|name| format!("{name}-headers"))
        .filter(|headers| in_repos.contains(headers));
    std::iter::once(format!("{boot_kernel}-headers"))
        .chain(others)
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
    if !defer_write(&path, &lockfile.render()) {
        write_config(&path, lockfile.render().as_bytes())?;
        println!("wrote {}", path.display());
    }
    archix::testhooks::crash_at("lock-resolved");
    Ok(())
}

thread_local! {
    /// While a switch runs, the config files it would write, which wait for it to commit.
    static DEFERRED: std::cell::RefCell<Option<Vec<ConfigWrite>>> = const { std::cell::RefCell::new(None) };
}

/// From here on, config writes wait in memory until the switch commits its deployment.
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
        write_config(&path, lockfile.render().as_bytes())?;
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

/// Writes what was deferred, keeping each file's owner, once the switch has committed; writes are still deferred afterwards.
pub(super) fn write_deferred() -> Result<()> {
    let writes = DEFERRED.with(|deferred| deferred.borrow_mut().as_mut().map(std::mem::take).unwrap_or_default());
    for write in writes {
        write_config(&write.path, write.text.as_bytes())?;
    }
    Ok(())
}

/// Writes the deferred config files that live in the running /etc into `etc`, a staged deployment's, before it's committed.
pub(super) fn stage_deferred(paths: &Paths, etc: &Path) -> Result<()> {
    let writes = DEFERRED.with(|deferred| deferred.borrow().clone().unwrap_or_default());
    for write in writes {
        if let Some(staged) = in_etc(paths, &write.path, etc) {
            archix::durable::write_config(&staged, write.text.as_bytes())?;
        }
    }
    Ok(())
}

/// Writes a config file, and also in the /etc of a committed deployment that hasn't booted yet, so it's there before the /etc merge carries it.
pub(super) fn write_config(path: &Path, text: &[u8]) -> Result<()> {
    archix::durable::write_config(path, text)?;
    let paths = Paths::system();
    if let Some(etc) = pending_etc(&paths)
        && let Some(copy) = in_etc(&paths, path, &etc)
    {
        archix::durable::write_config(&copy, text).with_context(|| format!("writing {}", copy.display()))?;
    }
    Ok(())
}

fn identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|meta| (meta.dev(), meta.ino()))
}

/// What a deployment waiting for the next boot means for /etc until then, by whether an /etc merge is carrying changes into it.
pub(super) fn pending_note(number: u32, carried: bool) -> String {
    let etc = if carried {
        "changes to /etc made before then are carried over to it"
    } else {
        "no /etc merge is running, so changes to /etc made before then don't reach it"
    };
    format!("deployment {number} is committed and boots at the next restart; {etc}")
}

/// The /etc of the deployment that boots next, when it isn't the one the running /etc belongs to.
pub(super) fn pending_etc(paths: &Paths) -> Option<std::path::PathBuf> {
    if !archix::deploy::layout::laid_out(&paths.root) {
        return None;
    }
    let store = archix::deploy::Store::open(&paths.root).ok()?;
    let etc = store.path(store.current().ok()??).join("etc");
    (identity(&etc) != identity(&paths.root.join("etc"))).then_some(etc)
}

/// Where `path`, a file under the running /etc, is under `etc`; None for a file elsewhere.
fn in_etc(paths: &Paths, path: &Path, etc: &Path) -> Option<std::path::PathBuf> {
    let live = std::fs::canonicalize(paths.root.join("etc")).ok()?;
    let path = std::path::absolute(path).ok()?;
    let parent = path.parent()?;
    let (found, rest) = parent
        .ancestors()
        .find_map(|dir| Some((std::fs::canonicalize(dir).ok()?, parent.strip_prefix(dir).ok()?)))?;
    Some(
        etc.join(found.strip_prefix(&live).ok()?)
            .join(rest)
            .join(path.file_name()?),
    )
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

/// What switching to `lock` would change, one line each, plus what's only kept because of warn mode, and keyrings kept newer.
pub(super) fn pending(
    target: &Target,
    declared: &Declared,
    lock: &HostLock,
) -> Result<(Vec<String>, Vec<String>, Vec<String>)> {
    let paths = Paths::system();
    let db = super::switch::installed_db(&paths);
    let installed = read_local_db(&db)?;
    let (changes, warned, kept) = select_changes(
        &lock.packages,
        &installed,
        &db,
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
    let here = super::switch::current_root(&paths);
    let place = |path: &str| {
        Some(match system::apply::deployment_tree(path) {
            Some(_) => here.join(path.trim_start_matches('/')),
            None => system::usr_local::on_disk(&paths.root, path),
        })
    };
    lines.extend(system::apply::describe(
        &change,
        &place,
        &Accounts::read(&here.join("etc")),
        &here,
    )?);
    let mut lock = lock.clone();
    let unresolved = settle_flatpak(target, declared, &mut lock, false, false)?;
    let (flatpak_lines, unmanaged) = flatpak::describe(&lock.flatpak, &previous_flatpak, &flatpak::installed()?);
    lines.extend(flatpak_lines);
    lines.extend(unresolved.iter().map(|app| format!("flatpak    install {app}")));
    if !unmanaged.is_empty() {
        println!("flatpak apps archix doesn't manage: {}", unmanaged.join(" "));
    }
    Ok((lines, warned, kept))
}

/// What this machine carries into every deployment that the config doesn't declare, as `archix {command}` reports it.
fn carried(target: &Target, ignored: &BTreeSet<String>, all: bool, command: &str) -> Result<Vec<String>> {
    let paths = Paths::system();
    let installed = archix::capture::read_installed(&super::switch::installed_db(&paths))?;
    let exe = integration::own_path()?;
    let mut declared: BTreeSet<String> = system::render(target.config, target.config_dir)?
        .state
        .files
        .into_keys()
        .collect();
    declared.extend(
        integration::files(&paths, &exe)
            .into_iter()
            .chain(integration::boot_files(&exe))
            .chain(integration::boot_wants())
            .map(|(path, _)| path.to_string_lossy().into_owned()),
    );
    let config_dir = std::fs::canonicalize(target.config_dir)
        .ok()
        .and_then(|dir| Some(format!("/{}", dir.strip_prefix(&paths.root).ok()?.display())));
    let found = archix::carried::find(&archix::carried::Scan {
        trees: &super::switch::current_root(&paths),
        root: &paths.root,
        installed: &installed,
        declared: &declared,
        ignored,
        config_dir,
    })?;
    Ok(found.lines(all, command))
}

pub fn diff(target: &Target, all: bool) -> Result<()> {
    let paths = Paths::system();
    let declared = declared(target.config)?;
    let (lock, _) = current_lock(target, &declared, Snapshot::Locked, false, true, false)?;
    let installed = read_local_db(&super::switch::installed_db(&paths))?;
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
    if let Some(boot) = &declared.boot
        && !archix::deploy::layout::laid_out(&paths.root)
        && let Some(why) = archix::deploy::layout::unsupported(&paths.root, Path::new(&boot.esp))
    {
        println!("`archix switch` refuses to lay this system out as deployments until this is fixed: {why}");
    }
    let (lines, warned, kept) = pending(target, &declared, &lock)?;
    if lines.is_empty() {
        println!("the system matches the lock and the config");
    }
    for line in lines {
        println!("{line}");
    }
    print_warned(&warned);
    for line in &kept {
        println!("{line}");
    }
    for line in carried(target, &declared.ignored, all, "diff")? {
        println!("{line}");
    }
    Ok(())
}

pub fn status(target: &Target, all: bool) -> Result<()> {
    let paths = Paths::system();
    let store = Store::new(&paths);
    match store.current()? {
        Some(number) => {
            let generation = store.get(number)?;
            println!("current generation: {number} (snapshot {})", generation.date);
            match archix::system::boot::booted_deployment(&paths.root) {
                Some(booted) if booted != number => {
                    println!("booted generation: {booted}; generation {number} boots at the next restart")
                }
                _ => {}
            }
            if pending_etc(&paths).is_some() {
                println!("{}", pending_note(number, archix::etcmerge::running(&paths)));
            }
            if let Some(waiting) = archix::system::reboot::waiting(&paths.root) {
                println!("{}", archix::system::reboot::line(&waiting));
            }
            archix::deploy::verify::print_status(&paths);
            if let Some(warning) = archix::system::usr_local::boot_files_warning(&paths.root) {
                println!("{warning}");
            }
            for line in archix::system::activate::boot_failures(&paths.root) {
                println!("{line}");
            }
            if let Some(pending) = archix::system::sleep::pending(&paths.root)
                && archix::system::sleep::drop_in(&paths.root).exists()
            {
                println!("{}", archix::system::sleep::note(pending));
            }
            if let Some(fallback) = super::switch::read_fallback(&paths).filter(|f| f.to == number) {
                println!("{}", fallback.note);
            }
            if let Some(note) = archix::system::boot::esp_unwritable_note(&paths) {
                println!("{note}");
            }
            if let Some(entry) = archix::system::boot::default_override() {
                println!(
                    "warning: systemd-boot's LoaderEntryDefault variable (the boot menu's default, or `bootctl set-default`) names {entry} and overrides archix's default; the next switch clears it, or `sudo bootctl set-default ''` now"
                );
            }
            for line in archix::etcmerge::status_lines(&paths) {
                println!("{line}");
            }
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
        for line in carried(target, &declared(target.config)?.ignored, all, "status")? {
            println!("{line}");
        }
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
    if declared.boot.as_ref().is_some_and(|boot| boot.tries > 0)
        && let Some(why) = archix::system::boot::counting_off()
    {
        println!("{why}");
    }
    let floating: Vec<&str> = lock
        .packages
        .iter()
        .filter(|(name, pkg)| floats(name, pkg))
        .map(|(name, _)| name.as_str())
        .collect();
    match floating.len() {
        0 => {}
        1 => println!(
            "1 package builds from a VCS source archix.lock has no commit for yet, so its next build takes upstream's newest code, whose git commit is then recorded: {}",
            floating[0]
        ),
        n => println!(
            "{n} packages build from VCS sources archix.lock has no commits for yet, so their next builds take upstream's newest code, whose git commits are then recorded: {}",
            floating.join(", ")
        ),
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
    let db = super::switch::installed_db(&paths);
    if db.is_dir() {
        let installed = read_local_db(&db)?;
        let (changes, _, _) = select_changes(
            &lock.packages,
            &installed,
            &db,
            declared.remove_undeclared,
            &declared.ignored,
        );
        match changes.len() {
            0 => println!("the system matches the lock"),
            n => println!("{} pending; see `archix diff`", count(n, "change", "changes")),
        }
        print_missing_ignored(&declared.ignored, &installed);
    }
    for line in carried(target, &declared.ignored, all, "status")? {
        println!("{line}");
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
    fn a_config_file_in_the_running_etc_maps_to_the_same_place_in_another_etc() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        std::fs::create_dir_all(dir.path().join("etc/archix")).unwrap();
        std::fs::write(dir.path().join("etc/archix/archix.lock"), "").unwrap();
        let other = dir.path().join("pending/etc");
        let mapped = |path: &str| in_etc(&paths, &dir.path().join(path), &other).map(|p| p.into_os_string());
        assert_eq!(
            mapped("etc/archix/archix.lock"),
            Some(other.join("archix/archix.lock").into_os_string()),
            "a rename to a path with a trailing slash fails"
        );
        assert_eq!(
            mapped("etc/archix/lua/hosts/box/installed.lua"),
            Some(other.join("archix/lua/hosts/box/installed.lua").into_os_string()),
            "a file in directories not made yet"
        );
        assert_eq!(mapped("home/config/init.lua"), None);
    }

    #[test]
    fn dkms_gets_the_headers_of_every_kernel_not_just_the_boot_one() {
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
        let headers: Vec<String> = kernel_headers("linux", packages.iter(), &in_repos)
            .into_iter()
            .collect();
        assert_eq!(headers, ["linux-headers", "linux-lts-headers"]);
        let cachyos = names(&["linux-cachyos", "linux-cachyos-lts"]);
        let in_repos: BTreeSet<String> = names(&["linux-cachyos-headers"]).into_iter().collect();
        let headers: Vec<String> = kernel_headers("linux-cachyos", cachyos.iter(), &in_repos)
            .into_iter()
            .collect();
        assert_eq!(
            headers,
            ["linux-cachyos-headers"],
            "a kernel without headers in the repos gets none"
        );
    }
}

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};

use archix::adopt::{adopt, newer_than_lock};
use archix::apply;
use archix::archive::{ARCHIVE_URL, Archive};
use archix::aur::build::Builder;
use archix::aur::index::Index;
use archix::aur::install::{BuildFailed, Installer, nearest, read_srcinfo};
use archix::aur::review::review;
use archix::date::Date;
use archix::eval;
use archix::fetch::{Wanted, fetch_all, sha256};
use archix::flatpak::{self, LockedApp};
use archix::gc;
use archix::generations::{Generation, Settings, Store, config_commit, now};
use archix::integration;
use archix::journal::{Journal, Kind, Step};
use archix::lock::{AUR_REPO, HostLock, LOCK_FILE, Locked, Lockfile};
use archix::pacman::{read_local_db, read_sync_db};
use archix::paths::Paths;
use archix::plan::Change;
use archix::repos::{Repo, fetch_db};
use archix::resolve::snapshot_conf;
use archix::system::boot::BootPlan;
use archix::system::{self, SystemState, apply::SystemChange, snapshot};

use super::lock::{current_lock, save_lock};
use super::{
    Declared, Nearest, Snapshot, Target, ThirdPartyFallback, aur_git, aur_source, check_pacman_free, confirm, count,
    declared, edit_installed, ensure_flatpak_remotes, print_warned, refuse_if_unfinished, require_root, select_changes,
    settle_flatpak, transaction_for, user_cache,
};

/// Adds out-of-band packages to installed.lua and switches to the result, undoing the edit if that fails.
fn switch_after_declaring(
    target: &Target,
    added: &[String],
    noconfirm: bool,
    accept_nearest: Option<Nearest>,
) -> Result<()> {
    let edit = edit_installed(target.config_dir, target.host, None, added, &[])?;
    let result = eval::evaluate(&eval::Input {
        config_dir: target.config_dir.to_path_buf(),
        hostname: target.host.to_string(),
    })
    .map_err(|errors| anyhow::anyhow!("{errors}"))
    .and_then(|evaluation| {
        let target = Target {
            config: &evaluation.config,
            ..*target
        };
        switch(&target, noconfirm, accept_nearest)
    });
    if result.is_err() {
        edit.restore();
    }
    result.context("installed.lua was restored")
}

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
            "the system is newer than the lock for {} ({}{more}), probably from pacman or an AUR helper",
            count(newer.len(), "package", "packages"),
            shown.join(", ")
        );
        if noconfirm || confirm("adopt them into the lock? (no reverts them to the lock)")? {
            let git = aur_git(&declared)?;
            let previous = lock.clone();
            let aur = aur_source(&git, &declared, &archive, Some(&previous), false);
            let adopted = adopt(
                &archive,
                &user_cache()?,
                &paths,
                &previous,
                &installed,
                &declared.repos,
                Some(&aur),
            )?;
            let mut lockfile = Lockfile::load(&target.config_dir.join(LOCK_FILE))?;
            save_lock(target, &mut lockfile, Some(&previous), &adopted)?;
            drop(aur);
            lock = adopted;
        }
    }

    if declared.remove_undeclared {
        let outside: BTreeSet<String> = integration::outside_changes(&paths)?
            .into_iter()
            .flat_map(|change| change.packages)
            .collect();
        let added: Vec<String> = installed
            .values()
            .filter(|pkg| {
                pkg.explicit
                    && outside.contains(&pkg.name)
                    && !lock.packages.contains_key(&pkg.name)
                    && !declared.ignored.contains(&pkg.name)
            })
            .map(|pkg| pkg.name.clone())
            .collect();
        if !added.is_empty() {
            println!(
                "installed with pacman or an AUR helper but not declared: {}",
                added.join(" ")
            );
            if noconfirm || confirm("declare them in installed.lua? (no removes them)")? {
                return switch_after_declaring(target, &added, noconfirm, accept_nearest);
            }
        }
    }

    let unresolved_flatpak = settle_flatpak(target, &declared, &mut lock, true, false)?;
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
            esp: declared.boot.as_ref().map(|boot| boot.esp.clone()),
            boot: declared.boot.as_ref().map(|boot| {
                format!(
                    "{} {} {:?} {:?} {:?} {}",
                    boot.kernel, boot.cmdline, boot.microcode, boot.secure_boot, boot.entries, boot.esp
                )
            }),
        },
        config_commit: commit.as_ref().map(|(hash, _)| hash.clone()),
        config_dirty: commit.is_some_and(|(_, dirty)| dirty),
        system: SystemState::default(),
        snapshot: None,
        flatpak: lock.flatpak.clone(),
    };
    let rendered = system::render(target.config, target.config_dir)?;
    generation.system = rendered.state.clone();
    let system_change = SystemChange {
        from: current.as_ref().map(|g| g.system.clone()).unwrap_or_default(),
        to: rendered.state.clone(),
    };
    let system_lines = system::apply::describe(&system_change)?;
    system::apply::check(&paths, &system_change)?;
    let previous_flatpak = current.as_ref().map(|g| g.flatpak.clone()).unwrap_or_default();
    let (mut flatpak_lines, _) = flatpak::describe(&lock.flatpak, &previous_flatpak, &flatpak::installed()?);
    flatpak_lines.extend(unresolved_flatpak.iter().map(|app| format!("flatpak    install {app}")));

    let (changes, warned) = select_changes(
        &lock.packages,
        &installed,
        declared.remove_undeclared,
        &declared.ignored,
    );
    print_warned(&warned);
    let unchanged = current.as_ref().is_some_and(|current| current.same_system(&generation));
    if changes.is_empty()
        && system_lines.is_empty()
        && flatpak_lines.is_empty()
        && (unchanged || declared.boot.is_none())
    {
        if let Some(current) = current.filter(|current| current.same_system(&generation)) {
            finish(&paths, declared.boot.is_some(), &declared.repos)?;
            println!(
                "nothing to do: the system matches generation {} (snapshot {})",
                current.number, lock.date
            );
            return report_soft_failures(&run_hooks(&system_change));
        }
        system::blobs::ensure(&paths, &archive, &rendered)?;
        system::apply::keep_originals(&paths, &system_change)?;
        let created = store.create(generation)?;
        store.activate(created.number)?;
        finish(&paths, false, &declared.repos)?;
        println!(
            "switched to generation {} (snapshot {}); no package changes were needed",
            created.number, lock.date
        );
        return report_soft_failures(&run_hooks(&system_change));
    }
    for change in &changes {
        println!("{change}");
    }
    for line in system_lines.iter().chain(&flatpak_lines) {
        println!("{line}");
    }
    if declared.boot.is_some() {
        println!("boot entry for the new generation");
    }
    if let Ok(names) = system::hooks::order(&system_change.to.hooks)
        && !names.is_empty()
    {
        println!("run hooks  {}", names.join(", "));
    }
    let total = changes.len() + system_lines.len() + flatpak_lines.len() + usize::from(declared.boot.is_some());
    if !noconfirm && !confirm(&format!("apply {}?", count(total, "change", "changes")))? {
        bail!("aborted; nothing was changed");
    }
    system::blobs::ensure(&paths, &archive, &rendered)?;

    import_keys(&declared.repos)?;
    let mut changes = changes;
    if third_party_fallback(target, &declared, &paths, &archive, &mut lock, &changes, noconfirm)? {
        changes = select_changes(
            &lock.packages,
            &installed,
            declared.remove_undeclared,
            &declared.ignored,
        )
        .0;
        generation.packages = lock.packages.clone();
        carry_aur_hashes(&mut generation.packages, current.as_ref());
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

    let mut transaction = transaction_for(
        &archive,
        &paths,
        lock.date,
        &lock.packages,
        &changes,
        &built,
        &declared.repos,
    )?;
    transaction.system = Some(system_change);
    let mut created = store.create(generation)?;
    if let Some(boot) = &declared.boot {
        transaction.boot = Some(BootPlan {
            generation: created.number,
            esp: boot.esp.clone(),
            kernel: boot.kernel.clone(),
            microcode: boot.microcode.clone(),
            cmdline: boot.cmdline.clone(),
            keep: boot.entries,
            secure_boot: boot.secure_boot,
        });
    }
    if declared.snapshots && snapshot::root_is_btrfs() {
        created.snapshot = Some(snapshot::take(created.number)?);
        store.update(&created)?;
    }
    let name = format!("generation {} (snapshot {})", created.number, lock.date);
    let mut journal = Journal::new(Kind::Switch, target.host, name.clone(), installed, transaction);
    journal.generation = Some(created.number);
    journal.save(&paths.journal())?;
    let mut soft_failures = apply::run(&paths, &mut journal, false)?;
    finish(&paths, declared.boot.is_some(), &declared.repos)?;
    soft_failures.extend(finish_flatpak(
        target,
        &declared,
        &mut lock,
        &store,
        created.number,
        &previous_flatpak,
    )?);
    println!("switched to {name}");
    report_soft_failures(&soft_failures)
}

/// Hooks for a switch that needed no journal; NixOS runs activation on every switch too.
fn run_hooks(change: &SystemChange) -> Vec<String> {
    if change.to.hooks.is_empty() && change.to.on_change.is_empty() {
        return Vec::new();
    }
    eprintln!(":: {}", Step::Hooks.describe());
    system::hooks::run(change)
}

/// Locks Flatpak apps that couldn't be locked before flatpak was installed, then installs and pins them.
fn finish_flatpak(
    target: &Target,
    declared: &Declared,
    lock: &mut HostLock,
    store: &Store,
    number: u32,
    previous: &BTreeMap<String, LockedApp>,
) -> Result<Vec<String>> {
    if declared.flatpak_apps.is_empty() && previous.is_empty() {
        return Ok(Vec::new());
    }
    ensure_flatpak_remotes(declared)?;
    let mut failures = settle_flatpak(target, declared, lock, true, false)?;
    let mut generation = store.get(number)?;
    if generation.flatpak != lock.flatpak {
        generation.flatpak = lock.flatpak.clone();
        store.update(&generation)?;
    }
    failures.extend(flatpak::sync(&lock.flatpak, previous));
    Ok(failures)
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

/// Lists what failed without undoing the switch, and makes the command fail so scripts notice.
fn report_soft_failures(failures: &[String]) -> Result<()> {
    if failures.is_empty() {
        return Ok(());
    }
    for failure in failures {
        eprintln!("failed: {failure}");
    }
    bail!(
        "the new generation is current, but {} (listed above)",
        count(failures.len(), "step failed", "steps failed")
    )
}

/// Imports and locally signs custom repos' signing keys that pacman's keyring doesn't have yet.
fn import_keys(repos: &[Repo]) -> Result<()> {
    for repo in repos {
        let Some(custom) = &repo.custom else { continue };
        for key in &custom.keys {
            let known = Command::new("pacman-key")
                .args(["--list-keys", key])
                .output()
                .is_ok_and(|o| o.status.success());
            if known {
                continue;
            }
            eprintln!(":: importing {}'s signing key {key}", repo.name);
            for args in [
                vec!["--keyserver", custom.keyserver.as_str(), "--recv-keys", key],
                vec!["--lsign-key", key],
            ] {
                let status = Command::new("pacman-key")
                    .args(&args)
                    .status()
                    .context("running pacman-key")?;
                if !status.success() {
                    bail!("pacman-key {} failed for {}'s key {key}", args.join(" "), repo.name);
                }
            }
        }
    }
    Ok(())
}

/// Replaces locked custom-repo packages that are gone (not cached, not on the servers) per `third_party_fallback`.
fn third_party_fallback(
    target: &Target,
    declared: &Declared,
    paths: &Paths,
    archive: &Archive,
    lock: &mut HostLock,
    changes: &[Change],
    noconfirm: bool,
) -> Result<bool> {
    let old = lock.clone();
    let mut replaced = false;
    for change in changes {
        let (Change::Install { name, .. } | Change::Upgrade { name, .. } | Change::Downgrade { name, .. }) = change
        else {
            continue;
        };
        let pkg = lock.packages[name].clone();
        let Some(repo) = declared.repo(&pkg.repo).filter(|repo| repo.custom.is_some()) else {
            continue;
        };
        let wanted = Wanted {
            name: name.clone(),
            file: pkg.file.clone(),
            urls: repo.urls(&pkg.file),
            sha256: Some(pkg.sha256.clone()),
        };
        if fetch_all(
            archive,
            &paths.packages(),
            std::slice::from_ref(&paths.pacman_cache),
            &[wanted],
        )
        .is_ok()
        {
            continue;
        }
        println!("{name} {} is gone from {}", pkg.version, repo.name);
        let fall_back = match declared.third_party_fallback {
            ThirdPartyFallback::Fail => bail!(
                "{name} {} is gone from {}; third_party_fallback = \"fail\"",
                pkg.version,
                repo.name
            ),
            ThirdPartyFallback::Fallback => true,
            ThirdPartyFallback::Ask => {
                noconfirm || confirm("fall back to its current version, the AUR, or the official repos?")?
            }
        };
        if !fall_back {
            bail!("aborted; nothing was changed");
        }
        let mut replacement = fallback_source(declared, archive, lock.date, repo, name)?;
        replacement.explicit = pkg.explicit;
        println!("using {name} {} from {}", replacement.version, replacement.repo);
        lock.packages.insert(name.clone(), replacement);
        lock.pinned.retain(|pinned| pinned != name);
        replaced = true;
    }
    if replaced {
        let git = aur_git(declared)?;
        review(&git, Some(&old), lock, declared.aur_review && !noconfirm, &confirm)?;
        let mut lockfile = Lockfile::load(&target.config_dir.join(LOCK_FILE))?;
        save_lock(target, &mut lockfile, Some(&old), lock)?;
    }
    Ok(replaced)
}

/// The next source down the chain: the custom repo's current version, then the AUR, then the official repos.
fn fallback_source(declared: &Declared, archive: &Archive, date: Date, repo: &Repo, name: &str) -> Result<Locked> {
    let cache = user_cache()?;
    let fresh = cache.join("custom-dbs");
    fs::create_dir_all(&fresh)?;
    let db = fresh.join(format!("{}.db", repo.name));
    if fetch_db(archive, repo, &db).is_ok()
        && let Some(pkg) = read_sync_db(&db)?.remove(name)
    {
        return Ok(Locked {
            version: pkg.version,
            repo: repo.name.clone(),
            file: pkg.filename,
            sha256: pkg.sha256,
            ..Default::default()
        });
    }
    let index = Index::load(archive, &declared.aur_url, &cache, true)?;
    if let Some(entry) = index.get(name) {
        let git = aur_git(declared)?;
        let base = entry.package_base.clone();
        git.sync(&base)?;
        let commit = git.head(&base)?;
        let info = read_srcinfo(&git, &base, &commit)?;
        if let Some(pkg) = info.package(name) {
            return Ok(Locked {
                version: info.version.clone(),
                repo: AUR_REPO.to_string(),
                file: info.file(pkg),
                base: Some(base),
                commit: Some(commit),
                ..Default::default()
            });
        }
    }
    let official: Vec<Repo> = declared.repos.iter().filter(|r| r.custom.is_none()).cloned().collect();
    let (_, db) = snapshot_conf(archive, &cache, date, &official)?;
    for repo in &official {
        if let Some(pkg) = read_sync_db(&db.join("sync").join(format!("{}.db", repo.name)))?.remove(name) {
            return Ok(Locked {
                version: pkg.version,
                repo: repo.name.clone(),
                file: pkg.filename,
                sha256: pkg.sha256,
                ..Default::default()
            });
        }
    }
    bail!(
        "{name} is gone from {} and is in neither the AUR nor the official repos",
        repo.name
    )
}

/// After a successful switch: outside changes are reconciled, integration files are in place, old garbage goes.
fn finish(paths: &Paths, boot: bool, repos: &[Repo]) -> Result<()> {
    integration::clear_outside_changes(paths)?;
    integration::ensure(paths, boot)?;
    integration::sync_pacman_conf(paths, repos)?;
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
    roll_back(to, noconfirm, true)
}

/// A rollback; at boot there's no network yet, so Flatpak apps are left for a later `archix rollback N`.
fn roll_back(to: Option<u32>, noconfirm: bool, pin_flatpak: bool) -> Result<()> {
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
        println!("generation {number} is current; putting back whatever differs from it");
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
    let current_generation = store.get(current)?;
    let system_change = SystemChange {
        from: current_generation.system,
        to: generation.system.clone(),
    };
    let system_lines = system::apply::describe(&system_change)?;
    system::apply::check(&paths, &system_change)?;
    let previous_flatpak = current_generation.flatpak;
    let (flatpak_lines, _) = flatpak::describe(&generation.flatpak, &previous_flatpak, &flatpak::installed()?);
    for line in system_lines.iter().chain(&flatpak_lines) {
        println!("{line}");
    }
    if changes.is_empty() && system_lines.is_empty() && flatpak_lines.is_empty() {
        store.activate(number)?;
        println!("{name} is current; no package changes were needed");
        return Ok(());
    }
    for change in &changes {
        println!("{change}");
    }
    if !noconfirm
        && !confirm(&format!(
            "roll back to {name} with {}?",
            count(
                changes.len() + system_lines.len() + flatpak_lines.len(),
                "change",
                "changes"
            )
        ))?
    {
        bail!("aborted; nothing was changed");
    }
    let archive = Archive::new(ARCHIVE_URL);
    let mut transaction = transaction_for(
        &archive,
        &paths,
        generation.date,
        &generation.packages,
        &changes,
        &BTreeMap::new(),
        &[],
    )?;
    transaction.system = Some(system_change);
    let mut journal = Journal::new(Kind::Switch, &generation.host, name.clone(), installed, transaction);
    journal.generation = Some(number);
    journal.save(&paths.journal())?;
    let mut soft_failures = apply::run(&paths, &mut journal, false)?;
    if (!generation.flatpak.is_empty() || !previous_flatpak.is_empty()) && generation.flatpak != previous_flatpak {
        if pin_flatpak {
            soft_failures.extend(flatpak::sync(&generation.flatpak, &previous_flatpak));
        } else {
            println!(
                "archix: Flatpak apps aren't pinned at boot; run `sudo archix rollback {number}` once you're online"
            );
        }
    }
    println!("rolled back to {name}; archix.lock is unchanged, so the next `archix switch` returns to the config");
    report_soft_failures(&soft_failures)
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
            "{marker} {:>4}  {created} {:02}:{:02} UTC  snapshot {}  {}{commit}",
            generation.number,
            minutes / 60,
            minutes % 60,
            generation.date,
            count(generation.packages.len(), "package", "packages")
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
            "archix: recorded {} changed outside archix; `archix switch` will offer to adopt them",
            count(change.packages.len(), "package", "packages")
        ),
        Ok(None) => {}
        Err(e) => println!("archix: could not record this transaction: {e:#}"),
    }
    if paths.journal().exists() {
        println!("archix: a switch is unfinished; run `sudo archix recover`");
    }
}

/// Run early at boot by archix-boot.service: if an older generation's entry was booted, make that generation current.
pub fn boot_check(cmdline: Option<String>) -> Result<()> {
    let cmdline = match cmdline {
        Some(cmdline) => cmdline,
        None => fs::read_to_string("/proc/cmdline").context("reading /proc/cmdline")?,
    };
    let Some(booted) = system::boot::booted_generation(&cmdline) else {
        return Ok(());
    };
    let paths = Paths::system();
    let release = fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    if system::boot::restore_modules(&paths, release.trim())? {
        println!("archix: restored the modules of kernel {}", release.trim());
    }
    if let Some(journal) = Journal::load(&paths.journal())? {
        println!(
            "archix: {}; not changing anything at boot, run `sudo archix recover`",
            super::describe_unfinished(&journal)
        );
        return Ok(());
    }
    let store = Store::new(&paths);
    match store.current()? {
        Some(current) if current == booted => Ok(()),
        Some(current) => {
            println!("archix: booted generation {booted}, but {current} is current; rolling back to {booted}");
            roll_back(Some(booted), true, false)
        }
        None => Ok(()),
    }
}

/// Restores a generation's core packages under `root` without pacman, for a system that no longer boots.
pub fn rescue(root: &std::path::Path, generation: Option<u32>, extra: &[String]) -> Result<()> {
    require_root("rescue")?;
    let store = Store::new(&Paths::under(root));
    let number = match generation {
        Some(number) => number,
        None => store
            .current()?
            .context("there is no current generation under that root; pass --generation")?,
    };
    let generation = store.get(number)?;
    println!(
        "restoring the core packages of generation {number} under {}",
        root.display()
    );
    let rescued = system::rescue::rescue(root, &generation, extra)?;
    for line in &rescued.restored {
        println!("restored {line}");
    }
    for line in &rescued.missing {
        println!("missing  {line}");
    }
    println!(
        "{} restored; boot normally, then run `sudo archix rollback {number}` to put everything else back",
        count(rescued.restored.len(), "package", "packages")
    );
    Ok(())
}

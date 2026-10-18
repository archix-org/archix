use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};

use archix::adopt::{adopt, newer_than_lock};
use archix::apply;
use archix::archive::Archive;
use archix::aur::build::Builder;
use archix::aur::build::built_version;
use archix::aur::index::Index;
use archix::aur::install::{BuildFailed, Installer, nearest, read_srcinfo};
use archix::aur::review::review;
use archix::date::Date;
use archix::downgrade::{merged, unsafe_downgrades};
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
use archix::plan::{Change, rebuilds, swapped_builds};
use archix::repos::{Repo, fetch_db};
use archix::resolve::snapshot_conf;
use archix::system::boot::BootPlan;
use archix::system::running::Running;
use archix::system::{self, SystemState, apply::SystemChange, snapshot};

use super::lock::{current_lock, save_lock};
use super::{
    Declared, Nearest, Snapshot, Target, ThirdPartyFallback, aur_git, aur_source, check_pacman_free, confirm,
    confirm_no, count, declared, edit_installed, ensure_flatpak_remotes, print_warned, recipes, refuse_if_unfinished,
    require_root, select_changes, settle_flatpak, transaction_for, user_cache,
};

/// Declares packages in the host's installed.lua and switches, undoing the edit if that fails.
pub fn declare_and_switch(target: &Target, names: &[String], noconfirm: bool) -> Result<()> {
    switch_after_declaring(target, names, noconfirm, None, false)
}

/// Adds out-of-band packages to installed.lua and switches to the result, undoing the edit if that fails.
fn switch_after_declaring(
    target: &Target,
    added: &[String],
    noconfirm: bool,
    accept_nearest: Option<Nearest>,
    allow_unsafe: bool,
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
        switch(&target, noconfirm, accept_nearest, allow_unsafe)
    });
    if result.is_err() {
        edit.restore();
    }
    result.context("installed.lua was restored")
}

pub fn switch(target: &Target, noconfirm: bool, accept_nearest: Option<Nearest>, allow_unsafe: bool) -> Result<()> {
    require_root("switch")?;
    let paths = Paths::system();
    refuse_if_unfinished(&paths)?;
    check_pacman_free(&paths)?;
    clean_up_kept_modules(&paths);
    let declared = declared(target.config)?;
    let (mut lock, _) = current_lock(target, &declared, Snapshot::Locked, true, noconfirm, false)?;
    if let Some(warning) = super::lock_age_warning(&lock, declared.lock_warn_after_days) {
        eprintln!("{warning}");
    }
    let installed = read_local_db(&paths.local_db())?;
    let archive = Archive::new(&declared.archive_url).with_caches(&declared.package_caches);
    archix::archive::remember(&paths.state, &declared.archive_url)?;
    archix::archive::remember_caches(&paths.state, &declared.package_caches)?;
    let beyond = archix::archive::beyond_cpu(
        lock.packages
            .iter()
            .map(|(name, pkg)| (name.as_str(), pkg.file.as_str())),
        &archix::archive::architectures(),
    );
    if !beyond.is_empty() {
        let shown: Vec<&str> = beyond.iter().take(6).map(String::as_str).collect();
        bail!(
            "archix.lock for {} has packages built for a newer CPU level than this machine runs, so it was locked elsewhere: {}{}. Run `archix lock` on this machine; nothing was changed",
            target.host,
            shown.join(", "),
            if beyond.len() > shown.len() { ", ..." } else { "" }
        );
    }

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
            let recipes = recipes(target, &declared)?;
            let aur = aur_source(&git, &declared, &archive, Some(&previous), false, &recipes);
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
                return switch_after_declaring(target, &added, noconfirm, accept_nearest, allow_unsafe);
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
                    "{} {} {:?} {:?} {:?} {}{}",
                    boot.kernel,
                    boot.cmdline,
                    boot.microcode,
                    boot.secure_boot,
                    boot.entries,
                    boot.esp,
                    if boot.protected_entry { " protected" } else { "" }
                )
            }),
        },
        config_commit: commit.as_ref().map(|(hash, _)| hash.clone()),
        config_dirty: commit.is_some_and(|(_, dirty)| dirty),
        system: SystemState::default(),
        snapshot: None,
        flatpak: lock.flatpak.clone(),
        rescue: Vec::new(),
    };
    let rendered = system::render(target.config, target.config_dir)?;
    generation.system = rendered.state.clone();
    let system_change = SystemChange {
        from: current.as_ref().map(|g| g.system.clone()).unwrap_or_default(),
        to: rendered.state.clone(),
    };
    let system_lines = system::apply::describe(&system_change)?;
    system::apply::check(&paths, &system_change)?;
    if let Some(rules) = rendered.state.files.get(system::firewall::RULES) {
        system::firewall::validate(&String::from_utf8_lossy(&rendered.blobs[&rules.sha256]))?;
        if system::apply::is_enabled("nftables.service", false)
            && rendered.state.services.get("nftables.service") != Some(&false)
        {
            eprintln!(
                "warning: nftables.service is enabled too, so /etc/nftables.conf (on Arch: drop everything but SSH) still applies on top of the archix firewall; set services[\"nftables.service\"] = false"
            );
        }
    }
    let previous_flatpak = current.as_ref().map(|g| g.flatpak.clone()).unwrap_or_default();
    let (mut flatpak_lines, _) = flatpak::describe(&lock.flatpak, &previous_flatpak, &flatpak::installed()?);
    flatpak_lines.extend(unresolved_flatpak.iter().map(|app| format!("flatpak    install {app}")));

    let (mut changes, warned) = select_changes(
        &lock.packages,
        &installed,
        declared.remove_undeclared,
        &declared.ignored,
    );
    changes.extend(rebuilds(
        &lock.packages,
        &installed,
        current.as_ref().map(|g| &g.packages),
    ));
    let snapshot_moved = current.as_ref().is_some_and(|current| current.date != lock.date);
    if snapshot_moved {
        changes.extend(broken_builds(
            &paths,
            &archive,
            &lock,
            &installed,
            &changes,
            &declared.repos,
        )?);
    }
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
            print_hooks(&system_change);
            let failures = run_hooks(&paths, &system_change, &BTreeSet::new());
            return report_soft_failures(&failures, "nothing was changed");
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
        print_hooks(&system_change);
        let failures = run_hooks(&paths, &system_change, &BTreeSet::new());
        return report_soft_failures(&failures, "the new generation is current");
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
    print_hooks(&system_change);
    if let Some(boot) = &declared.boot {
        system::boot::check_signing(boot.secure_boot)?;
        system::boot::check_space(&boot.esp, &boot.kernel, boot.protected_entry)?;
    }
    guard_downgrades(
        &unsafe_downgrades(&changes, &rendered.state.unsafe_downgrades),
        noconfirm,
        allow_unsafe,
    )?;
    let total = changes.len() + system_lines.len() + flatpak_lines.len() + usize::from(declared.boot.is_some());
    if !noconfirm && !confirm(&format!("apply {}?", count(total, "change", "changes")))? {
        bail!("aborted; nothing was changed");
    }
    system::blobs::ensure(&paths, &archive, &rendered)?;

    import_keys(&declared.repos)?;
    if third_party_fallback(target, &declared, &paths, &archive, &mut lock, &changes, noconfirm)? {
        changes = select_changes(
            &lock.packages,
            &installed,
            declared.remove_undeclared,
            &declared.ignored,
        )
        .0;
        changes.extend(rebuilds(
            &lock.packages,
            &installed,
            current.as_ref().map(|g| &g.packages),
        ));
        if snapshot_moved {
            changes.extend(broken_builds(
                &paths,
                &archive,
                &lock,
                &installed,
                &changes,
                &declared.repos,
            )?);
        }
        generation.packages = lock.packages.clone();
        carry_aur_hashes(&mut generation.packages, current.as_ref());
    }

    let aur_names: Vec<String> = changes
        .iter()
        .filter_map(|change| match change {
            Change::Install { name, .. }
            | Change::Upgrade { name, .. }
            | Change::Downgrade { name, .. }
            | Change::Rebuild { name, .. }
                if lock.packages[name].is_built() =>
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
    let mut versioned = Vec::new();
    for (name, path) in &built {
        if let Some(pkg) = lock.packages.get_mut(name) {
            let file = path.file_name().map(|f| f.to_string_lossy().into_owned());
            if let Some(version) = file.as_deref().and_then(|f| built_version(name, f))
                && version != pkg.version
            {
                versioned.push(format!("{name} {version}"));
                pkg.version = version;
            }
            let mut pkg = pkg.clone();
            pkg.sha256 = sha256(path)?;
            pkg.file = file.unwrap_or(pkg.file);
            generation.packages.insert(name.clone(), pkg);
        }
    }
    if !versioned.is_empty() {
        println!(
            "recorded the versions these packages gave themselves while building: {}",
            versioned.join(", ")
        );
        let path = target.config_dir.join(LOCK_FILE);
        let mut lockfile = Lockfile::load(&path)?;
        lockfile.hosts.insert(target.host.to_string(), lock.clone());
        lockfile.save(&path)?;
    }
    for (name, pkg) in &lock.packages {
        if pkg.is_built() && !built.contains_key(name) {
            let mut pkg = pkg.clone();
            if let Some(previous) = generation.packages.get(name) {
                pkg.sha256 = previous.sha256.clone();
                pkg.file = previous.file.clone();
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
    archix::conflicts::check(&transaction.install, &transaction.remove, &paths.local_db())?;
    apply::check_packages(&transaction)?;
    let rewritten = system::apply::out_of_date(&system_change)?;
    system::hooks::defer(
        &paths.pending_on_change(),
        &system::hooks::due(&system_change, &rewritten),
    )?;
    transaction.system = Some(system_change.clone());
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
            anticheat: boot.protected_entry,
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
    soft_failures.extend(run_hooks(&paths, &system_change, &rewritten));
    println!("switched to {name}");
    reboot_notice();
    report_soft_failures(&soft_failures, "the new generation is current")
}

/// Lists unsafe downgrades and asks, defaulting to no; `--noconfirm` refuses them unless `allow` is set.
pub(super) fn guard_downgrades(risky: &[String], noconfirm: bool, allow: bool) -> Result<()> {
    if risky.is_empty() {
        return Ok(());
    }
    println!("unsafe downgrades: the older versions may refuse or damage data the newer ones wrote");
    for line in risky {
        println!("  {line}");
    }
    if allow {
        return Ok(());
    }
    if noconfirm {
        bail!(
            "refusing unsafe downgrades with --noconfirm; to go ahead, run `archix switch` or `archix rollback` with --allow-unsafe-downgrade"
        );
    }
    if !confirm_no("downgrade anyway?")? {
        bail!("aborted; nothing was changed");
    }
    Ok(())
}

/// Says so when the switch replaced the running kernel, which only a reboot finishes.
pub(super) fn reboot_notice() {
    if Running::new().replaced() {
        println!("the running kernel was replaced: reboot to finish (its modules stay loadable until then)");
    }
}

/// Removes kernel modules archix kept for a kernel that's no longer running.
pub(super) fn clean_up_kept_modules(paths: &Paths) {
    if let Err(e) = Running::new().clean_up(paths) {
        eprintln!("warning: couldn't remove kernel modules kept for an earlier boot: {e:#}");
    }
}

/// The preview line naming the hooks a switch or rollback runs, in order.
fn print_hooks(change: &SystemChange) {
    if let Ok(names) = system::hooks::order(&change.to.hooks)
        && !names.is_empty()
    {
        println!("run hooks  {}", names.join(", "));
    }
}

/// on_change scripts and hooks, last thing in every switch, rollback and recovery, like NixOS activation.
pub(super) fn run_hooks(paths: &Paths, change: &SystemChange, rewritten: &BTreeSet<String>) -> Vec<String> {
    let pending = paths.pending_on_change();
    // A script this switch is due to run for the same file is newer than the one a boot rollback left behind.
    let mut due = system::hooks::deferred(&pending);
    due.extend(system::hooks::due(change, rewritten));
    if change.to.hooks.is_empty() && due.is_empty() {
        return Vec::new();
    }
    eprintln!(":: running activation hooks");
    let failures = system::hooks::run(change, &due);
    if let Err(e) = fs::remove_file(&pending)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("warning: could not clear {}: {e}", pending.display());
    }
    failures
}

/// Rebuilds for built packages that would lose a library they link once the snapshot moves; only the owners of those libraries are downloaded to check.
fn broken_builds(
    paths: &Paths,
    archive: &Archive,
    lock: &HostLock,
    installed: &BTreeMap<String, archix::pacman::LocalPkg>,
    changes: &[Change],
    repos: &[archix::repos::Repo],
) -> Result<Vec<Change>> {
    let rebuilding: BTreeSet<&str> = changes
        .iter()
        .filter_map(|change| match change {
            Change::Rebuild { name, .. }
            | Change::Install { name, .. }
            | Change::Upgrade { name, .. }
            | Change::Downgrade { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    let built: Vec<String> = lock
        .packages
        .iter()
        .filter(|(name, pkg)| pkg.is_built() && !pkg.build_only && !rebuilding.contains(name.as_str()))
        .filter(|(name, pkg)| installed.get(*name).is_some_and(|have| have.version == pkg.version))
        .map(|(name, _)| name.clone())
        .collect();
    let changing: BTreeSet<String> = changes
        .iter()
        .filter_map(|change| match change {
            Change::Upgrade { name, .. } | Change::Downgrade { name, .. } | Change::Remove { name, .. } => {
                Some(name.clone())
            }
            _ => None,
        })
        .collect();
    let broken = archix::soname::broken_builds(
        std::path::Path::new("/"),
        &paths.local_db(),
        &built,
        &changing,
        |owner| {
            let Some(pkg) = lock.packages.get(owner) else {
                return Ok(None);
            };
            let wanted = super::wanted_file(archive, lock.date, owner, pkg, repos);
            let file = fetch_all(
                archive,
                &paths.packages(),
                std::slice::from_ref(&paths.pacman_cache),
                &[wanted],
            )?;
            Ok(Some(archix::conflicts::contents(&file[0])?.files))
        },
    )?;
    Ok(broken
        .into_iter()
        .map(|name| Change::Rebuild {
            version: lock.packages[&name].version.clone(),
            name,
        })
        .collect())
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
        .filter(|(_, pkg)| pkg.is_built() && pkg.sha256.is_empty())
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

/// A build that failed on a nearly full disk says nothing about its recipe, so no other version is tried.
fn short_of_build_space(paths: &Paths, cache: &std::path::Path) -> Option<String> {
    const ROOM: u64 = 1 << 30;
    [paths.state.join("chroot"), cache.to_path_buf(), std::env::temp_dir()]
        .into_iter()
        .filter(|dir| dir.exists())
        .find_map(|dir| {
            let free = archix::conflicts::free_space(&dir)?;
            (free < ROOM).then(|| {
                format!(
                    "{} has only {} MiB free, which is likely why the build failed; nothing was changed. Free some space (`archix gc`, `pacman -Sc`) and try again",
                    dir.display(),
                    free >> 20
                )
            })
        })
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
        skip_check: &declared.skip_check,
    };
    let recipes = recipes(target, declared)?;
    let mut moved = BTreeSet::new();
    let before_fallbacks = lock.clone();
    loop {
        let result = Installer::new(&builder, lock, &declared.build_on_host, Some(&recipes))
            .and_then(|installer| installer.build(names));
        let error = match result {
            Ok(built) => {
                // A fallback version only reaches archix.lock once it has actually built.
                if !moved.is_empty() {
                    let mut lockfile = Lockfile::load(&target.config_dir.join(LOCK_FILE))?;
                    save_lock(target, &mut lockfile, Some(&before_fallbacks), lock)?;
                }
                return Ok(built);
            }
            Err(error) => error,
        };
        if let Some(short) = short_of_build_space(paths, &cache) {
            return Err(error.context(short));
        }
        let Some(failed) = error
            .downcast_ref::<BuildFailed>()
            .filter(|failed| !failed.base.contains(':'))
        else {
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
    }
}

/// Lists soft failures and fails the command; `state` says what did happen, such as "the new generation is current".
pub(super) fn report_soft_failures(failures: &[String], state: &str) -> Result<()> {
    if failures.is_empty() {
        return Ok(());
    }
    for failure in failures {
        eprintln!("failed: {failure}");
    }
    bail!(
        "{state}, but {} (listed above)",
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
            ThirdPartyFallback::Ask if noconfirm => bail!(
                "{name} {} is gone from {}; nothing was changed. Run without --noconfirm to choose a replacement, or set third_party_fallback = \"fallback\"",
                pkg.version,
                repo.name
            ),
            ThirdPartyFallback::Ask => confirm("fall back to its current version, the AUR, or the official repos?")?,
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
    let _ = fs::remove_file(paths.notes());
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
pub fn rollback(to: Option<u32>, noconfirm: bool, allow_unsafe: bool) -> Result<()> {
    roll_back(to, noconfirm, Some(allow_unsafe))
}

/// A rollback; at boot there's no network or session yet, so Flatpak apps and hooks are left for a later `archix rollback N`.
/// `allow_unsafe` None means a boot-time rollback, which goes ahead like NixOS and leaves a login note about unsafe downgrades.
fn roll_back(to: Option<u32>, noconfirm: bool, allow_unsafe: Option<bool>) -> Result<()> {
    let online = allow_unsafe.is_some();
    require_root("rollback")?;
    let paths = Paths::system();
    refuse_if_unfinished(&paths)?;
    check_pacman_free(&paths)?;
    if online {
        clean_up_kept_modules(&paths);
    }
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
    let current_generation = store.get(current)?;
    let (mut changes, warned) = select_changes(
        &generation.packages,
        &installed,
        generation.settings.remove_undeclared,
        &ignored,
    );
    changes.extend(rebuilds(
        &generation.packages,
        &installed,
        Some(&current_generation.packages),
    ));
    let swapped = swapped_builds(
        &generation.packages,
        &installed,
        Some(&current_generation.packages),
        &changes,
    );
    changes.extend(swapped);
    print_warned(&warned);
    let name = format!("generation {number} (snapshot {})", generation.date);
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
    let hooks_at_end = |rewritten: &BTreeSet<String>| {
        if online {
            return run_hooks(&paths, &system_change, rewritten);
        }
        let due = system::hooks::due(&system_change, rewritten);
        if let Err(e) = system::hooks::defer(&paths.pending_on_change(), &due) {
            println!("archix: could not record the on_change scripts to run later: {e:#}");
        }
        if !system_change.to.hooks.is_empty() || !due.is_empty() {
            println!(
                "archix: hooks don't run at boot; the next switch or `sudo archix rollback {number}` runs them once you're online"
            );
        }
        Vec::new()
    };
    if changes.is_empty() && system_lines.is_empty() && flatpak_lines.is_empty() {
        if online {
            print_hooks(&system_change);
        }
        store.activate(number)?;
        if online {
            let _ = fs::remove_file(paths.notes());
        }
        println!("{name} is current; no package changes were needed");
        return report_soft_failures(&hooks_at_end(&BTreeSet::new()), &format!("{name} is current"));
    }
    for change in &changes {
        println!("{change}");
    }
    if online {
        print_hooks(&system_change);
    }
    let configured = merged(
        &system_change.from.unsafe_downgrades,
        &generation.system.unsafe_downgrades,
    );
    let risky = unsafe_downgrades(&changes, &configured);
    match allow_unsafe {
        Some(allow) => guard_downgrades(&risky, noconfirm, allow)?,
        None if !risky.is_empty() => {
            let note = format!(
                "archix: the boot-menu rollback to generation {number} went back across versions that may not read newer data; check these services:\n{}\n",
                risky
                    .iter()
                    .map(|line| format!("  {line}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            let _ = fs::write(paths.notes(), note);
        }
        None => {}
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
    let archive = archix::archive::for_system(&paths.state);
    let mut transaction = transaction_for(
        &archive,
        &paths,
        generation.date,
        &generation.packages,
        &changes,
        &BTreeMap::new(),
        &[],
    )?;
    transaction.install = apply::without_signatures(&paths, &transaction.install)?;
    if online {
        archix::conflicts::check(&transaction.install, &transaction.remove, &paths.local_db())?;
        apply::check_packages(&transaction)?;
    }
    let rewritten = system::apply::out_of_date(&system_change)?;
    system::hooks::defer(
        &paths.pending_on_change(),
        &system::hooks::due(&system_change, &rewritten),
    )?;
    transaction.system = Some(system_change.clone());
    transaction.at_boot = !online;
    let mut journal = Journal::new(Kind::Switch, &generation.host, name.clone(), installed, transaction);
    journal.generation = Some(number);
    journal.save(&paths.journal())?;
    let mut soft_failures = apply::run(&paths, &mut journal, false)?;
    if (!generation.flatpak.is_empty() || !previous_flatpak.is_empty()) && generation.flatpak != previous_flatpak {
        if online {
            soft_failures.extend(flatpak::sync(&generation.flatpak, &previous_flatpak));
        } else {
            println!(
                "archix: Flatpak apps aren't pinned at boot; run `sudo archix rollback {number}` once you're online"
            );
        }
    }
    soft_failures.extend(hooks_at_end(&rewritten));
    println!("rolled back to {name}; archix.lock is unchanged, so the next `archix switch` returns to the config");
    if online {
        reboot_notice();
        let _ = fs::remove_file(paths.notes());
    }
    report_soft_failures(&soft_failures, &format!("{name} is current"))
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

/// Copies this machine's package files, their signatures and its AUR build records into a package cache directory, skipping what it already holds.
pub fn cache_push(dir: &std::path::Path) -> Result<()> {
    let paths = Paths::system();
    let mut copied = 0;
    let mut copy_dir = |from: &std::path::Path, to: &std::path::Path| -> Result<()> {
        fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
        for entry in fs::read_dir(from).into_iter().flatten().flatten() {
            let (source, dest) = (entry.path(), to.join(entry.file_name()));
            if !source.is_file()
                || dest
                    .metadata()
                    .is_ok_and(|have| source.metadata().is_ok_and(|want| want.len() == have.len()))
            {
                continue;
            }
            // Through a temporary name, so a machine reading the cache never sees half a file.
            let tmp = to.join(format!(".{}.part", entry.file_name().to_string_lossy()));
            fs::copy(&source, &tmp).with_context(|| format!("copying {}", source.display()))?;
            fs::rename(&tmp, &dest)?;
            copied += 1;
        }
        Ok(())
    };
    copy_dir(&paths.packages(), dir)?;
    for base in fs::read_dir(paths.cache.join("aur-builds"))
        .into_iter()
        .flatten()
        .flatten()
    {
        copy_dir(&base.path(), &dir.join("aur-builds").join(base.file_name()))?;
    }
    println!("copied {} to {}", count(copied, "file", "files"), dir.display());
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
    clean_up_kept_modules(&paths);
    let release = fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    if system::boot::restore_modules(&paths, release.trim())? {
        println!("archix: restored the modules of kernel {}", release.trim());
        // udev and modules-load already ran without them, so devices whose drivers were missing get another go.
        let _ = Command::new("systemctl")
            .args(["restart", "systemd-modules-load.service"])
            .status();
        let _ = Command::new("udevadm")
            .args(["trigger", "--action=add", "--type=devices"])
            .status();
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
            roll_back(Some(booted), true, None)
        }
        None => Ok(()),
    }
}

/// The unfinished switch under `root` when it stopped inside pacman (its Remove or Install step), which can tear any file pacman was writing; once per such switch.
fn cut_off_in_pacman(root: &std::path::Path) -> Result<Option<Journal>> {
    let paths = Paths::under(root);
    let Some(journal) = Journal::load(&paths.journal())? else {
        return Ok(None);
    };
    if !matches!(journal.next_step(), Some(Step::Remove | Step::Install)) {
        return Ok(None);
    }
    let marker = paths.state.join("auto-rescue");
    let started = journal.started.to_string();
    if fs::read_to_string(&marker).is_ok_and(|text| text.trim() == started) {
        return Ok(None);
    }
    archix::durable::write(&marker, started.as_bytes())?;
    println!(
        "archix: {}, inside pacman; restoring the packages it was changing so the system can boot",
        super::describe_unfinished(&journal)
    );
    Ok(Some(journal))
}

/// Packages an unfinished switch was changing or removing, the only ones whose files it can have left half-written.
fn touched(journal: &Journal) -> impl Iterator<Item = String> + '_ {
    journal
        .before
        .iter()
        .filter(|(name, pkg)| journal.transaction.versions.get(*name) != Some(&pkg.version))
        .map(|(name, _)| name.clone())
}

/// Restores a generation's core packages under `root` without pacman, for a system that no longer boots.
pub fn rescue(root: &std::path::Path, generation: Option<u32>, extra: &[String], initrd: bool) -> Result<()> {
    require_root("rescue")?;
    let store = Store::new(&Paths::under(root));
    let cmdline = fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let mut extra = extra.to_vec();
    if initrd {
        // An older generation's kernel finds its modules gone under a newer generation; mounts like a vfat /boot need them before the real root starts.
        let release = fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
        if system::boot::restore_into(&root.join("usr/lib/modules"), &Paths::under(root), release.trim())? {
            println!("archix: restored the modules of kernel {}", release.trim());
        }
    }
    if initrd && !cmdline.split_whitespace().any(|word| word == "archix.rescue") {
        let Some(journal) = cut_off_in_pacman(root)? else {
            return Ok(());
        };
        extra.extend(touched(&journal));
    } else if let Ok(Some(journal)) = Journal::load(&Paths::under(root).journal()) {
        extra.extend(touched(&journal));
    }
    // A rescue entry carries its own generation on the command line; restoring the current one couldn't undo a bad switch.
    let booted = system::boot::booted_generation(&cmdline);
    let number = match generation.or(booted) {
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
    let rescued = system::rescue::rescue(root, &generation, &extra)?;
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
    let record = Paths::under(root).state.join("last-rescue");
    let _ = fs::write(
        &record,
        format!(
            "generation {number}, {} packages restored, at unix time {}\n",
            rescued.restored.len(),
            now()
        ),
    );
    Ok(())
}

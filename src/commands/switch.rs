use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use archix::adopt::{adopt, newer_than_lock};
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
use archix::lock::{AUR_REPO, HostLock, Locked};
use archix::packages::{Packages, check_packages, without_signatures};
use archix::pacman::{read_local_db, read_sync_db};
use archix::paths::Paths;
use archix::plan::{Change, rebuilds, swapped_builds};
use archix::repos::{Repo, fetch_db};
use archix::resolve::snapshot_conf;
use archix::system::apply::{Accounts, SystemChange};
use archix::system::{self, SystemState, live};

use super::lock::{current_lock, load_lockfile, save_lock, store_lockfile};
use super::snapshot::Rollbacks;
use super::{
    Declared, Drift, Nearest, Snapshot, Target, ThirdPartyFallback, aur_git, aur_source, check_pacman_free, confirm,
    confirm_no, count, declared, edit_installed, ensure_flatpak_remotes, keep_third_party_copies,
    print_missing_ignored, print_warned, recipes, require_root, select_changes, settle_flatpak, third_party_repos,
    transaction_for, user_cache,
};

/// The root a switch applies to.
pub(super) fn current_root(paths: &Paths) -> PathBuf {
    paths.root.clone()
}

/// The /etc/crypttab the switch leaves: the one the config renders, the original archix kept when it stops managing one, or else the running system's.
pub(super) fn next_crypttab(
    paths: &Paths,
    here: &Path,
    change: &SystemChange,
    blobs: &BTreeMap<String, Vec<u8>>,
) -> String {
    const PATH: &str = "/etc/crypttab";
    let live = fs::read_to_string(here.join("etc/crypttab")).ok();
    if let Some(file) = change.to.files.get(PATH) {
        return blobs
            .get(&file.sha256)
            .map(|blob| String::from_utf8_lossy(blob).into_owned())
            .unwrap_or_default();
    }
    let Some(previous) = change.from.files.get(PATH) else {
        return live.unwrap_or_default();
    };
    if let Ok(original) = fs::read_to_string(paths.state.join("backups/etc/crypttab")) {
        return original;
    }
    match live {
        Some(text) if archix::fetch::sha256_bytes(text.as_bytes()) != previous.sha256 => text,
        _ => String::new(),
    }
}

/// The root's pacman database.
pub(super) fn installed_db(paths: &Paths) -> PathBuf {
    paths.local_db()
}

/// Where a managed path is on the root.
fn place_in(paths: &Paths) -> impl Fn(&str) -> Option<PathBuf> {
    let root = paths.root.clone();
    move |path: &str| Some(root.join(path.trim_start_matches('/')))
}

/// Declares packages in the host's installed.lua and switches; installed.lua is written only if the switch commits.
pub fn declare_and_switch(target: &Target, names: &[String], noconfirm: bool) -> Result<()> {
    switch_after_declaring(target, names, noconfirm, None, false)
}

/// Adds out-of-band packages to installed.lua and switches to the result; installed.lua is written only if the switch commits.
fn switch_after_declaring(
    target: &Target,
    added: &[String],
    noconfirm: bool,
    accept_nearest: Option<Nearest>,
    allow_unsafe: bool,
) -> Result<()> {
    edit_installed(target.config_dir, target.host, None, added, &[])?;
    let overlay = super::lock::deferred_overlay(target.config_dir);
    let config = match target.captured {
        Some(captured) => super::reevaluate(captured, target.host, &overlay).map_err(|e| anyhow::anyhow!("{e}"))?,
        None => {
            eval::evaluate(&eval::Input {
                config_dir: target.config_dir.to_path_buf(),
                hostname: target.host.to_string(),
                overlay,
            })
            .map_err(|errors| anyhow::anyhow!("{errors}"))?
            .config
        }
    };
    let unread: Vec<&str> = {
        let packages = declared(&config)?.packages;
        added
            .iter()
            .filter(|name| !packages.contains(*name))
            .map(String::as_str)
            .collect()
    };
    if !unread.is_empty() {
        bail!(
            "{} went into {}, but the config still doesn't declare them with it, so adding them can't settle their drift. Declare them in packages yourself, or set drift = \"keep\"; nothing was changed",
            unread.join(" "),
            archix::installed::default_dir(target.config_dir, target.host)
                .join(archix::installed::FILE)
                .display()
        );
    }
    let target = Target {
        config: &config,
        ..*target
    };
    switch(&target, noconfirm, accept_nearest, allow_unsafe).context("installed.lua was left as it was")
}

pub fn switch(target: &Target, noconfirm: bool, accept_nearest: Option<Nearest>, allow_unsafe: bool) -> Result<()> {
    require_root("switch")?;
    let started = now();
    let paths = Paths::system();
    check_pacman_free(&paths)?;
    refuse_unusual_root(&paths, "switch")?;
    let declared = declared(target.config)?;
    let boot = declared.boot_plan()?;
    let policy = archix::snapshot::Policy::from_config(target.config)?;
    let rollbacks = Rollbacks::of(&paths.root, policy.clone())?;
    let (mut lock, _) = current_lock(target, &declared, Snapshot::Locked, true, noconfirm, false)?;
    if let Some(warning) = super::lock_age_warning(&lock, declared.lock_warn_after_days) {
        eprintln!("{warning}");
    }
    let db = installed_db(&paths);
    let installed = read_local_db(&db)?;
    let archive = Archive::new(&declared.archive_url)
        .with_mirrors(&declared.archive_mirrors)
        .with_caches(&declared.package_caches);
    archix::archive::remember(&paths.state, &declared.archive_url)?;
    archix::archive::remember_caches(&paths.state, &declared.package_caches)?;
    archix::archive::remember_mirrors(&paths.state, &declared.archive_mirrors)?;
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
            let mut lockfile = load_lockfile(target)?;
            save_lock(target, &mut lockfile, Some(&previous), &adopted)?;
            drop(aur);
            lock = adopted;
        }
    }

    let mut untracked = archix::revert::kept_untracked(&paths.state);
    untracked.retain(|name| installed.contains_key(name) && !lock.packages.contains_key(name));
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
                && !declared.packages.contains(&pkg.name)
                && !declared.ignored.contains(&pkg.name)
                && !untracked.contains(&pkg.name)
        })
        .map(|pkg| pkg.name.clone())
        .collect();
    if !added.is_empty() {
        println!(
            "installed with pacman or an AUR helper since the last switch, not declared: {}",
            added.join(" ")
        );
        let add = match declared.drift {
            Drift::Add => true,
            Drift::Keep => false,
            Drift::Ask => noconfirm || confirm("add them to installed.lua? (no keeps them installed, untracked)")?,
        };
        if add {
            println!(
                "adding them to installed.lua, as drift = \"{}\" says",
                match declared.drift {
                    Drift::Add => "add",
                    _ => "ask",
                }
            );
            return switch_after_declaring(target, &added, noconfirm, accept_nearest, allow_unsafe);
        }
        println!("keeping them installed and untracked: no switch removes them or what they need");
        untracked.extend(added);
    }
    archix::revert::save_untracked(&paths.state, &untracked)?;

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
        format: archix::generations::FORMAT,
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
            boot: boot.clone(),
            rollback: policy.clone(),
        },
        config_commit: commit.as_ref().map(|(hash, _)| hash.clone()),
        config_dirty: commit.is_some_and(|(_, dirty)| dirty),
        applied: None,
        system: SystemState::default(),
        flatpak: lock.flatpak.clone(),
        package_capabilities: BTreeMap::new(),
    };
    let mut rendered = system::render_linked(target.config, target.tree, target.config_dir)?;
    for warning in &rendered.warnings {
        eprintln!("warning: {warning}");
    }
    if let Some(warning) = system::credentials::remote_warning(&rendered.state.users, target.config_dir) {
        eprintln!("{warning}");
    }
    let prepared = prepare_secrets(&paths, &mut rendered)?;
    let mut passwords = system::credentials::resolve(&rendered.state.users, &paths.state);
    system::credentials::resolve_secrets(&mut passwords, rendered.state.secrets.as_ref(), &paths.state);
    for line in system::credentials::override_lines(&passwords) {
        println!("{line}");
    }
    generation.system = rendered.state.clone();
    let system_change = SystemChange {
        from: current.as_ref().map(|g| g.system.clone()).unwrap_or_default(),
        to: rendered.state.clone(),
    };
    let here = current_root(&paths);
    let place = place_in(&paths);
    let accounts = Accounts::read(&here.join("etc"));
    let mut system_lines = system::apply::describe(&system_change, &place, &accounts, &here)?;
    system_lines.retain(|line| !line.contains(archix::secrets::DEPLOY_DIR));
    system_lines.extend(archix::secrets::prepare::describe(
        system_change.from.secrets.as_ref(),
        system_change.to.secrets.as_ref(),
    ));
    if let Some(manifest) = &system_change.to.secrets {
        system_lines.extend(archix::secrets::prepare::existing(
            manifest,
            system_change.from.secrets.as_ref(),
            &prepared,
            &place,
        )?);
    }
    system_lines.extend(system::credentials::describe(&passwords, &here.join("etc")));
    for kept in system::credentials::kept_locks(&passwords, &[&here.join("etc")]) {
        eprintln!("warning: {kept}");
    }
    system::apply::check(&paths, &system_change, &place)?;
    system::live::check_files(&system_change, &|path: &str| {
        rendered
            .state
            .files
            .get(path)
            .and_then(|file| rendered.blobs.get(&file.sha256).cloned())
    })?;
    system::live::refuse_packaged_usr_files(&system_change, &db)
        .map_err(|e| anyhow::anyhow!("{e:#}; nothing was changed"))?;
    system_lines.extend(system::fstab::describe_at(&here, target.config, &rendered.state.fstab));
    let new_text = |path: &str| match rendered.state.files.get(path) {
        Some(file) => rendered
            .blobs
            .get(&file.sha256)
            .map(|blob| String::from_utf8_lossy(blob).into_owned()),
        None => fs::read_to_string(here.join(path.trim_start_matches('/'))).ok(),
    };
    let crypttab = format!(
        "{}{}",
        new_text("/etc/crypttab").unwrap_or_default(),
        new_text("/etc/crypttab.initramfs").unwrap_or_default()
    );
    let cmdline = new_text("/etc/kernel/cmdline").unwrap_or_default();
    for warning in system::fstab::check(&paths.root, &rendered.state.fstab, &crypttab, &cmdline)? {
        eprintln!("{warning}");
    }
    let new_crypttab = next_crypttab(&paths, &here, &system_change, &rendered.blobs);
    for warning in system::fstab::check_drops(
        &paths.root,
        &here.join("etc"),
        system::fstab::esp_of(target.config),
        &rendered.state.fstab,
        &new_crypttab,
        &system::fstab::acknowledged(target.config),
    )? {
        eprintln!("{warning}");
    }
    if let Some(rules) = rendered.state.files.get(system::firewall::RULES) {
        system::firewall::validate(&String::from_utf8_lossy(&rendered.blobs[&rules.sha256]))?;
        system::firewall::check_others(&rendered.state.services, |unit| {
            system::apply::is_enabled(&here, unit, false)
        })?;
        if system::apply::is_enabled(&here, "nftables.service", false)
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
    if !flatpak::available() && lock.packages.contains_key("flatpak") {
        flatpak_lines = declared
            .flatpak_apps
            .iter()
            .map(|(_, id)| format!("flatpak    install {id} once the switch has installed flatpak"))
            .collect();
    }

    let (mut changes, warned, kept) = select_changes(
        &lock.packages,
        &installed,
        &db,
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
    for line in &kept {
        println!("{line}");
    }
    print_missing_ignored(&declared.ignored, &installed);
    let unchanged = current.as_ref().is_some_and(|current| current.same_system(&generation));
    let pacman_conf = here.join("etc/pacman.conf");
    let repos_stale = fs::read_to_string(&pacman_conf)
        .is_ok_and(|text| integration::with_custom_repos(&text, &declared.repos) != text);
    if repos_stale {
        println!("update     /etc/pacman.conf (custom repos)");
    }
    let nothing_changes = changes.is_empty() && system_lines.is_empty() && flatpak_lines.is_empty() && !repos_stale;
    if nothing_changes && unchanged {
        let current = current.expect("unchanged needs a current generation");
        super::lock::write_deferred()?;
        integration::clear_outside_changes(&paths)?;
        set_strict(target, &declared, &paths)?;
        let root = paths.root.clone();
        integration::write_all(&paths, &integration::own_path()?, &|path: &Path| {
            Ok(root.join(path.strip_prefix("/").unwrap_or(path)))
        })?;
        println!(
            "nothing to do: the system matches generation {} (snapshot {})",
            current.number, lock.date
        );
        if !system_change.to.hooks.is_empty() {
            eprintln!(":: running hooks");
        }
        let failures = system::hooks::run(&paths.root, &BTreeMap::new(), &system_change.to.hooks);
        return report_soft_failures(&failures, "nothing was changed");
    }
    for change in &changes {
        println!("{change}");
    }
    for line in system_lines
        .iter()
        .chain(&flatpak_lines)
        .map(String::as_str)
        .chain(rollbacks.line())
    {
        println!("{line}");
    }
    print_hooks(&system_change);
    guard_downgrades(
        &unsafe_downgrades(&changes, &rendered.state.unsafe_downgrades),
        noconfirm,
        allow_unsafe,
    )?;
    check_partial(&paths, &archive, &lock, &installed, &changes, &declared.repos)?;
    show_plan_diff(current.as_ref(), &lock.packages, &rendered.state);
    show_config_diff(target, &paths)?;
    let total = changes.len() + system_lines.len() + flatpak_lines.len() + 1;
    if !noconfirm && !confirm(&format!("apply {}?", count(total, "change", "changes")))? {
        bail!("aborted; nothing was changed");
    }
    system::blobs::ensure(&paths, &archive, &rendered)?;

    import_keys(&declared.repos)?;
    if third_party_fallback(target, &declared, &paths, &archive, &mut lock, &changes, noconfirm)? {
        changes = select_changes(
            &lock.packages,
            &installed,
            &db,
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
    let mut recorded = false;
    let mut differs = Vec::new();
    for (name, path) in &built {
        if let Some(pkg) = lock.packages.get_mut(name) {
            if !pkg.sha256.is_empty() && pkg.sha256 != sha256(path)? {
                differs.push(name.as_str());
            }
            // The first build of a recipe at the lock's snapshot is the one the lock names; package caches must serve that one.
            if pkg.sha256.is_empty() {
                pkg.sha256 = sha256(path)?;
                recorded = true;
            }
            // What the first build's sources were, so a rebuild of this lock gets them again.
            if pkg.sources.is_empty()
                && pkg.sha256 == sha256(path)?
                && let (Some(base), Some(commit)) = (&pkg.base, &pkg.commit)
            {
                pkg.sources = archix::aur::build::recorded_sources(&paths, base, commit, lock.date);
                recorded |= !pkg.sources.is_empty();
            }
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
    if !differs.is_empty() {
        println!(
            "built {} here, which differs from the build archix.lock records (another machine's, or an earlier one); its sources are pinned, so it's the same code. aur.require_locked_build refuses that instead",
            differs.join(", ")
        );
    }
    if !versioned.is_empty() {
        println!(
            "recorded the versions these packages gave themselves while building: {}",
            versioned.join(", ")
        );
    }
    if !versioned.is_empty() || recorded {
        let mut lockfile = load_lockfile(target)?;
        lockfile.hosts.insert(target.host.to_string(), lock.clone());
        store_lockfile(target, &lockfile)?;
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

    let package_changes = transaction_for(
        &archive,
        &paths,
        lock.date,
        &lock.packages,
        &changes,
        &built,
        &declared.repos,
    )?;
    archix::conflicts::check(&package_changes.install, &package_changes.remove, &db, &here)?;
    check_packages(&package_changes)?;
    keep_third_party_copies(&archive, &paths, lock.date, &lock.packages, &declared.repos);

    let rewritten = system::apply::out_of_date(&system_change, &place, &accounts)?;
    let number = store.next_number()?;
    generation.number = number;
    let label = match target.captured {
        Some(captured) => format!(
            "before switch to tree {}",
            &captured.tree[..captured.tree.len().min(12)]
        ),
        None => format!("before switch to generation {number}"),
    };
    if let Some(boot) = &boot {
        system::boot::check_signing(boot)?;
        system::menu::precheck(&paths, boot).map_err(|e| e.context("nothing was changed"))?;
    }
    let taken = rollbacks
        .before(&paths, current.as_ref(), label)
        .map_err(|e| e.context("nothing of the switch was applied"))?;
    archix::testhooks::mark("switch-snapshot");
    let marked = count_before_switch(&paths, &rollbacks, boot.as_ref(), number);
    let root = live::Root::new(&paths.root);
    eprintln!(":: applying generation {number}");
    let applied = live::apply(&live::Plan {
        paths: &paths,
        root: &root,
        packages: &package_changes,
        system: &system_change,
        repos: &declared.repos,
        esp: boot.as_ref().map(|boot| boot.esp.as_str()),
        credentials: Some(live::Credentials {
            passwords: &passwords,
            allow_no_password_login: system::allows_no_password_login(target.config),
            logins: &prepared.logins,
        }),
        package_capabilities: current.as_ref().map_or(&BTreeMap::new(), |g| &g.package_capabilities),
        rewritten: &rewritten,
        started,
    })
    .map_err(|e| {
        let untouched = e.downcast_ref::<live::NothingApplied>().is_some();
        let left = match (marked, untouched) {
            (true, true) => {
                let _ = archix::pacman::hooks::finished(&paths.state);
                String::new()
            }
            (true, false) => "; the system is marked as part way, so its next counted boot doesn't count as good and falls back to the slot, unless a switch finishes first".to_string(),
            (false, _) => String::new(),
        };
        e.context(format!(
            "generation {number} wasn't recorded, and generation {} stays current{}{left}",
            current.as_ref().map_or("none".to_string(), |g| g.number.to_string()),
            before_note(taken.as_ref())
        ))
    })?;
    archix::testhooks::mark("switch-applied");
    system::credentials::forget_overrides(&paths.state, &passwords);
    let mut soft_failures = applied.failures;
    if !prepared.values.is_empty()
        && let Err(e) = scan_live(&prepared, &paths)
    {
        soft_failures.push(format!("{e:#}"));
    }
    generation.package_capabilities = applied.package_capabilities;
    store.save(&generation)?;
    store.set_current(number)?;
    archix::testhooks::mark("switch-recorded");
    soft_failures.extend(write_boot(&paths, &rollbacks, boot.as_ref(), &generation.packages));
    if marked {
        // pacman doesn't flush what it extracted, nor the apply what it wrote; the mark goes only once they're on disk.
        archix::durable::sync_filesystems(&[&paths.root]);
        archix::testhooks::mark("switch-flushed");
        if let Err(e) = archix::pacman::hooks::finished(&paths.state) {
            soft_failures.push(format!(
                "{e:#}; the next boot won't count as good until a switch or transaction finishes"
            ));
        }
        archix::testhooks::mark("switch-finished");
    }
    after_apply(
        target,
        &declared,
        &paths,
        &mut lock,
        &store,
        number,
        &previous_flatpak,
        soft_failures,
    )
}

/// Turns strict mode's flag on, naming the config as pacman's hooks see it, or off.
fn set_strict(target: &Target, declared: &Declared, paths: &Paths) -> Result<()> {
    let flag = paths.state.join(archix::pacman::hooks::STRICT_FLAG);
    if !declared.strict {
        return match fs::remove_file(&flag) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e).context("removing strict mode's flag"),
            _ => Ok(()),
        };
    }
    let config = std::path::absolute(target.config_dir)?;
    let config = archix::pacman::hooks::config_in_root(&paths.root, &config);
    archix::durable::write(&flag, config.as_os_str().as_encoded_bytes())
}

/// Refuses `what` on a try boot, whose changes the next boot drops, and on a root archix didn't produce, whose records describe another one.
fn refuse_unusual_root(paths: &Paths, what: &str) -> Result<()> {
    if let Some(trying) = system::initrd::trying() {
        bail!(
            "this boot is snapshot {n} in try mode, read-only with a throwaway layer, so a {what} would be lost at the next boot; `archix rollback {n}` to keep it, or reboot into the current system; nothing was changed",
            n = trying.snapshot
        );
    }
    if let Some(why) = archix::snapshot::foreign_root(&paths.root) {
        bail!("{why}; nothing was changed");
    }
    let pending = paths
        .root
        .join(archix::pacman::hooks::KEEP_PENDING.trim_start_matches('/'));
    if let Ok(number) = fs::read_to_string(pending) {
        bail!(
            "rollback to snapshot {} is pending; reboot, or `archix rollback --cancel`; nothing was changed",
            number.trim()
        );
    }
    Ok(())
}

/// What's wrong with archix's pacman hooks, once a switch has written them: masked, shadowed, missing or changed.
pub(super) fn hook_warnings(paths: &Paths) -> Vec<String> {
    if Store::new(paths).current().ok().flatten().is_none() {
        return Vec::new();
    }
    let Ok(archix) = integration::own_path() else {
        return Vec::new();
    };
    archix::pacman::hooks::trouble(&paths.root, &integration::files(paths, &archix))
}

/// After an apply: builds the boot images of the root as it is now, prunes snapshots (keeping those with entries), then writes the menu, the rescue entry and the unblessed timeout; returns what failed, which leaves the applied system as it is.
fn write_boot(
    paths: &Paths,
    rollbacks: &Rollbacks,
    boot: Option<&system::boot::BootPlan>,
    lock: &BTreeMap<String, Locked>,
) -> Vec<String> {
    let mut failures = Vec::new();
    if let Err(e) = system::menu::adopt(paths) {
        failures.push(format!("moving the boot menu's state into @archix: {e:#}"));
    }
    let root = live::Root::new(&paths.root);
    if let Some(plan) = boot
        && plan.loader != system::boot::Loader::Limine
    {
        eprintln!(":: building the boot images");
        let built = system::boot::build(paths, plan, &root).and_then(|images| {
            let dir = system::menu::dir(paths);
            let mut state = system::menu::load(&dir)?;
            state.current = images;
            system::menu::save(&dir, &state)
        });
        if let Err(e) = built {
            failures.push(format!(
                "the boot images weren't built, so the boot menu still starts the old ones: {e:#}"
            ));
        }
    }
    for warning in rollbacks.after(paths, lock) {
        eprintln!("warning: {warning}");
    }
    let Some(plan) = boot else {
        return failures;
    };
    let cap = rollbacks.policy().map(|policy| policy.entries);
    if let Err(e) = system::esp::clean_esp(Path::new(&plan.esp))
        .and_then(|()| system::menu::refresh(paths, plan, cap, failures.is_empty()))
    {
        failures.push(format!("the boot menu wasn't written: {e:#}"));
    }
    match system::rescue::refresh(paths, plan, &root) {
        Ok(line) if !line.is_empty() => println!("{line}"),
        Ok(_) => {}
        Err(e) => failures.push(format!("the rescue entry wasn't refreshed: {e:#}")),
    }
    if failures.is_empty() {
        match system::limine::start(paths, plan) {
            Ok(Some(line)) => println!("{line}"),
            Ok(None) => {}
            Err(e) => failures.push(format!(
                "the move from Limine to systemd-boot didn't start, so Limine still boots this machine: {e:#}"
            )),
        }
    }
    if failures.is_empty()
        && plan.revoke_unused
        && let Err(e) = system::revoke::revoke(paths, false, true, &|_| Ok(true))
    {
        failures.push(format!("revoking boot images no entry uses: {e:#}"));
    }
    if let Err(e) = archix::durable::write(
        &paths.root.join(system::health::TIMEOUT_DROP_IN.trim_start_matches('/')),
        system::health::timeout_drop_in(plan.unblessed_timeout).as_bytes(),
    ) {
        failures.push(format!("recording boot.unblessed_timeout: {e:#}"));
    }
    failures
}

/// Before the apply, with rollbacks on: gives the current entry its tries and the slot its entry, so a boot this switch breaks part way falls back, then marks the switch unfinished so such a boot doesn't count as good; returns whether it marked it.
fn count_before_switch(
    paths: &Paths,
    rollbacks: &Rollbacks,
    boot: Option<&system::boot::BootPlan>,
    number: u32,
) -> bool {
    let (Some(plan), Some(policy)) = (boot, rollbacks.policy()) else {
        return false;
    };
    if plan.loader == system::boot::Loader::Limine {
        return false;
    }
    if let Err(e) = system::menu::refresh(paths, plan, Some(policy.entries), true) {
        eprintln!(
            "warning: the boot menu wasn't written before this switch, so a boot it breaks part way may not fall back: {e:#}"
        );
        return false;
    }
    let unfinished = archix::pacman::hooks::Unfinished {
        pacman: std::process::id(),
        started: archix::generations::now(),
        slot: system::menu::load(&system::menu::dir(paths))
            .ok()
            .and_then(|state| state.slot),
        switch: Some(number),
    };
    if let Err(e) = archix::pacman::hooks::mark_unfinished(&paths.state, &unfinished) {
        eprintln!(
            "warning: this switch isn't marked as running, so a power cut during it may leave a torn system that counts as good: {e:#}"
        );
        return false;
    }
    archix::testhooks::mark("switch-marked");
    true
}

/// Where the state before a failed apply is, for its error.
fn before_note(taken: Option<&archix::snapshot::Record>) -> String {
    match taken {
        Some(record) => format!(
            "; snapshot {} holds the system as it was before (`archix snapshot list`)",
            record.number
        ),
        None => String::new(),
    }
}

/// Prints what the evaluated plan changes from the current generation's: packages, files, units, users and the rest, with no secret or password hash in it.
fn show_plan_diff(current: Option<&Generation>, packages: &BTreeMap<String, Locked>, state: &SystemState) {
    let no_packages = BTreeMap::new();
    let no_state = SystemState::default();
    let lines = archix::plan_diff::lines(
        current.map_or(&no_packages, |g| &g.packages),
        current.map_or(&no_state, |g| &g.system),
        packages,
        state,
    );
    if !lines.is_empty() {
        println!(":: the plan since the last generation archix applied");
    }
    for line in archix::plan_diff::capped(&lines, 200) {
        println!("{line}");
    }
}

/// Prints how the captured config differs from the one archix last applied, from root's own copy of both.
fn show_config_diff(target: &Target, paths: &Paths) -> Result<()> {
    let Some(captured) = target.captured else {
        return Ok(());
    };
    let applied = archix::captured::Applied::at(&paths.root, &paths.state);
    applied.store_tree(captured)?;
    match applied.diff(&captured.tree, 200)? {
        Some(diff) if diff.is_empty() => {}
        Some(diff) => println!(":: the config since the last generation archix applied\n{diff}"),
        None => println!(
            ":: no generation applied by archix yet; this one records config tree {}",
            &captured.tree[..12]
        ),
    }
    Ok(())
}

/// Stores generation `number` in root's repository: the captured config tree, the lock this switch applied and the evaluated plan; and points the owner's refs/archix/generations/<number> at the tree.
fn record_applied(target: &Target, paths: &Paths, store: &Store, number: u32) -> Result<()> {
    let Some(captured) = target.captured else {
        return Ok(());
    };
    let applied = archix::captured::Applied::at(&paths.root, &paths.state);
    let tree = applied.store_tree(captured)?;
    let lock = super::lock::lock_text(target)?
        .with_context(|| format!("this switch applied no {}", archix::lock::LOCK_FILE))?;
    let stored = applied.record(
        number,
        &tree,
        lock.as_bytes(),
        &archix::eval::isolated::plan_json(target.config),
    )?;
    let mut generation = store.get(number)?;
    generation.applied = Some(stored);
    store.update(&generation)?;
    if let Err(e) = archix::captured::record_for_owner(target.config_dir, captured, number) {
        eprintln!("warning: couldn't point refs/archix/generations/{number} in the config's repository at it: {e:#}");
    }
    Ok(())
}

static WITHOUT_SECRETS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `switch --without-secrets`: secrets that don't decrypt here are left out instead of refusing the switch.
pub fn without_secrets() {
    WITHOUT_SECRETS.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Decrypts and checks the config's secrets as far as this switch can, filling in the manifest, and refuses a managed file that holds a value; returns the values, held until the commit.
fn prepare_secrets(paths: &Paths, rendered: &mut system::Rendered) -> Result<archix::secrets::prepare::Prepared> {
    use archix::secrets::{Kind, prepare, scan};
    let Some(mut manifest) = rendered.state.secrets.clone() else {
        return Ok(prepare::Prepared::default());
    };
    archix::secrets::memory::harden(false).context("keeping this process out of core dumps while it holds secrets")?;
    let mode = if WITHOUT_SECRETS.load(std::sync::atomic::Ordering::Relaxed) {
        prepare::Mode::Without
    } else if system::secureboot::in_chroot() {
        prepare::Mode::Chroot
    } else {
        prepare::Mode::Check
    };
    // Decrypting and scanning run with every page locked, age's own buffers too; the values stay locked after.
    let locked = archix::secrets::memory::LockAll::new()?;
    let prepared = {
        let rendered: &system::Rendered = rendered;
        prepare::prepare(&paths.state, &mut manifest, &|sha| rendered.ciphertext(sha), mode)?
    };
    rendered.set_secrets(manifest);
    let needles = prepared.needles(&[Kind::File, Kind::CryptHash]);
    let ciphertexts = format!("{}/", archix::secrets::DEPLOY_DIR);
    let mut hits = Vec::new();
    for (path, file) in &rendered.state.files {
        let own = path.starts_with(&ciphertexts) && !path.starts_with(&format!("{ciphertexts}templates/"));
        if let Some(bytes) = rendered.blobs.get(&file.sha256).filter(|_| !own) {
            hits.extend(scan::blob(bytes, path, &needles, false));
        }
    }
    archix::testhooks::record_locked_memory();
    drop(needles);
    drop(locked);
    prepared.relock();
    for warning in &prepared.warnings {
        eprintln!("warning: {warning}");
    }
    scan::judge(&hits, true)?;
    Ok(prepared)
}

/// After the apply, with secrets: no value in /etc (a short one only warns), reported as a soft failure since the files are written by now.
/// After the apply, scans /etc for the secrets' values: a long one in plain text is a soft failure, recorded so `archix status` names the file until it's gone; short ones only warn.
fn scan_live(prepared: &archix::secrets::prepare::Prepared, paths: &Paths) -> Result<()> {
    use archix::secrets::{Kind, scan};
    let files = prepared.needles(&[Kind::File]);
    let hits = scan::tree(&paths.root.join("etc"), "/etc", &files, false);
    let certain: Vec<String> = hits
        .iter()
        .filter(|hit| !hit.short)
        .map(|hit| hit.path.clone())
        .collect();
    archix::secrets::record_plaintext(paths, &certain)?;
    if !certain.is_empty() {
        let shown: Vec<String> = hits
            .iter()
            .filter(|hit| !hit.short)
            .map(|hit| format!("secret {} in {}", hit.secret, hit.path))
            .collect();
        bail!(
            "a secret's value is in plain text in /etc: {}; the switch applied, but take it out of there (secrets reach files only as links into {})",
            shown.join(", "),
            archix::secrets::RUN_DIR
        );
    }
    for warning in scan::judge(&hits, false)? {
        eprintln!("warning: {warning}");
    }
    Ok(())
}

/// Everything a switch does once its generation is applied and current: config writes, root's copy of the config, homes, Flatpak apps and gc; failures here are soft.
#[allow(clippy::too_many_arguments)]
fn after_apply(
    target: &Target,
    declared: &Declared,
    paths: &Paths,
    lock: &mut HostLock,
    store: &Store,
    number: u32,
    previous_flatpak: &BTreeMap<String, LockedApp>,
    mut soft_failures: Vec<String>,
) -> Result<()> {
    super::lock::write_deferred()?;
    adopt_package_cache(paths);
    for pkg in lock.packages.values().filter(|pkg| !pkg.file.is_empty()) {
        if let Err(e) = archix::gc::keep_copy(paths, &pkg.file) {
            soft_failures.push(format!("keeping {} in archix's package cache: {e:#}", pkg.file));
        }
    }
    if let Err(e) = record_applied(target, paths, store, number) {
        soft_failures.push(format!(
            "recording generation {number}'s config in root's repository: {e:#}"
        ));
    }
    integration::clear_outside_changes(paths)?;
    if let Err(e) = set_strict(target, declared, paths) {
        soft_failures.push(format!("setting strict mode: {e:#}"));
    }
    let _ = fs::remove_file(paths.notes());
    // Otherwise pacman warns on every run about a repo in pacman.conf whose database it never downloaded.
    let archive = archix::archive::for_system(&paths.state);
    let sync = paths.root.join("var/lib/pacman/sync");
    for repo in declared.repos.iter().filter(|repo| repo.custom.is_some()) {
        let db = sync.join(format!("{}.db", repo.name));
        if !db.exists()
            && let Err(e) = fs::create_dir_all(&sync)
                .map_err(anyhow::Error::from)
                .and_then(|()| fetch_db(&archive, repo, &db))
        {
            eprintln!("warning: couldn't download {}.db for pacman: {e:#}", repo.name);
        }
    }
    soft_failures.extend(finish_flatpak(target, declared, lock, store, number, previous_flatpak)?);
    super::lock::write_deferred()?;
    match gc::collect(paths, &user_cache()?) {
        Ok(report) => print_gc(&report, false),
        Err(e) => soft_failures.push(format!("garbage collection: {e:#}")),
    }
    println!("switched to generation {number} (snapshot {})", lock.date);
    push_to_cache(declared);
    report_soft_failures(&soft_failures, &format!("generation {number} is applied and current"))
}

/// Once @archix is mounted, moves the package copies the switch that set rollbacks up kept in the cache directory into @archix's cache, where reverts and keeps look; it only warns if it can't.
fn adopt_package_cache(paths: &Paths) {
    let old = paths.cache.join("pkg");
    if old == paths.packages() || !archix::snapshot::archix_on(&paths.root) {
        return;
    }
    match archix::gc::adopt_cache(&old, &paths.packages()) {
        Ok(0) => {}
        Ok(moved) => println!(
            "archix: moved {moved} package copies from {} into {}",
            old.display(),
            paths.packages().display()
        ),
        Err(e) => println!(
            "archix: couldn't move the package copies in {} into @archix: {e:#}",
            old.display()
        ),
    }
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
            "refusing unsafe downgrades with --noconfirm; to go ahead, run `archix switch` or `archix revert` with --allow-unsafe-downgrade"
        );
    }
    if !confirm_no("downgrade anyway?")? {
        bail!("aborted; nothing was changed");
    }
    Ok(())
}

/// The preview line naming the hooks a switch or rollback runs, in order, once it has applied.
fn print_hooks(change: &SystemChange) {
    if let Ok(names) = system::hooks::order(&change.to.hooks)
        && !names.is_empty()
    {
        println!("run hooks  {}", names.join(", "));
    }
}

/// Which of `packages` lose a library they link when `changing` move to `lock`'s versions; only those libraries' owners are downloaded to check.
fn soname_breaks(
    paths: &Paths,
    archive: &Archive,
    lock: &HostLock,
    packages: &[String],
    changing: &BTreeSet<String>,
    repos: &[archix::repos::Repo],
) -> Result<Vec<String>> {
    archix::soname::broken_builds(
        &current_root(paths),
        &installed_db(paths),
        packages,
        changing,
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
    )
}

/// Refuses a switch that downgrades declared packages (a revert) when that breaks a package archix doesn't track, by its declared dependencies or the sonames its binaries load, unless --allow-partial.
fn check_partial(
    paths: &Paths,
    archive: &Archive,
    lock: &HostLock,
    installed: &BTreeMap<String, archix::pacman::LocalPkg>,
    changes: &[Change],
    repos: &[archix::repos::Repo],
) -> Result<()> {
    if ALLOW_PARTIAL.load(std::sync::atomic::Ordering::Relaxed)
        || !changes.iter().any(|change| matches!(change, Change::Downgrade { .. }))
    {
        return Ok(());
    }
    let removed: BTreeSet<String> = changes
        .iter()
        .filter_map(|change| match change {
            Change::Remove { name, .. } => Some(name.clone()),
            _ => None,
        })
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
    let untracked = archix::revert::untracked(
        &installed.keys().cloned().collect(),
        &lock.packages.keys().cloned().collect(),
        &removed,
    );
    if untracked.is_empty() {
        return Ok(());
    }
    let target: BTreeMap<String, String> = installed
        .values()
        .filter(|pkg| !removed.contains(&pkg.name))
        .map(|pkg| {
            let version = lock
                .packages
                .get(&pkg.name)
                .map_or(&pkg.version, |locked| &locked.version);
            (pkg.name.clone(), version.clone())
        })
        .chain(
            lock.packages
                .iter()
                .filter(|(_, locked)| !locked.build_only)
                .map(|(name, locked)| (name.clone(), locked.version.clone())),
        )
        .collect();
    let mut breaks = archix::revert::dependency_breaks(&installed_db(paths), &untracked, &target);
    breaks.extend(soname_breaks(paths, archive, lock, &untracked, &changing, repos)?);
    if breaks.is_empty() {
        return Ok(());
    }
    bail!(
        "this switch downgrades declared packages, which would break packages archix doesn't track:\n  {}\ndeclare them, remove them, or pass --allow-partial to switch anyway; nothing was changed",
        breaks.join("\n  ")
    )
}

/// `switch --allow-partial`: a revert goes ahead though it breaks packages archix doesn't track.
pub fn allow_partial() {
    ALLOW_PARTIAL.store(true, std::sync::atomic::Ordering::Relaxed);
}

static ALLOW_PARTIAL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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
    let broken: Vec<String> = soname_breaks(paths, archive, lock, &built, &changing, repos)?
        .into_iter()
        .map(|reason| {
            eprintln!(":: rebuilding {reason}");
            reason.split(' ').next().unwrap_or_default().to_string()
        })
        .collect();
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
    for (name, pkg) in packages.iter_mut().filter(|(_, pkg)| pkg.is_built()) {
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
        require_locked: declared.require_locked_build,
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
                    let mut lockfile = load_lockfile(target)?;
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
            size: None,
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
        let mut lockfile = load_lockfile(target)?;
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

fn print_gc(report: &gc::Report, always: bool) {
    for recipient in &report.retired {
        println!("deleted the retired secrets identity {recipient}, which no kept generation needs");
    }
    if !report.generations.is_empty() {
        let numbers: Vec<String> = report.generations.iter().map(u32::to_string).collect();
        println!("deleted generation records {}", numbers.join(", "));
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

/// `archix revert`: applies an earlier generation's record live, in place, its packages from archix's cache and the archive, and makes it current without a new record; archix.lock and the config stay as they are.
pub fn revert(to: Option<u32>, noconfirm: bool, allow_unsafe: bool) -> Result<()> {
    require_root("revert")?;
    let started = now();
    let paths = Paths::system();
    check_pacman_free(&paths)?;
    refuse_unusual_root(&paths, "revert")?;
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
        println!("generation {number} is current already");
        return Ok(());
    }
    let generation = store.get(number)?;
    if let Some(why) = generation.too_new() {
        bail!("{why}; nothing was changed");
    }
    if let Some(applied) = &generation.applied {
        let stored = archix::captured::Applied::verified_at(
            &paths.root,
            &paths.state,
            number,
            &applied.commit,
            Some(&applied.tree),
        )
        .with_context(|| {
            format!("generation {number} can't be read back from root's repository; nothing was changed")
        })?;
        archix::eval::isolated::read_plan(&stored.plan)
            .with_context(|| format!("generation {number}'s stored plan doesn't parse; nothing was changed"))?;
    }
    let current_generation = store.get(current)?;
    let rollbacks = Rollbacks::of(&paths.root, current_generation.settings.rollback.clone())?;
    let db = paths.local_db();
    let installed = read_local_db(&db)?;
    let ignored = generation.settings.ignored.iter().cloned().collect();
    let (mut changes, warned, kept_lines) = select_changes(
        &generation.packages,
        &installed,
        &db,
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
    let name = format!("generation {number} (snapshot {})", generation.date);
    let system_change = SystemChange {
        from: current_generation.system.clone(),
        to: generation.system.clone(),
    };
    let configured = merged(
        &current_generation.system.unsafe_downgrades,
        &generation.system.unsafe_downgrades,
    );
    let risky = unsafe_downgrades(&changes, &configured);
    let previous_flatpak = current_generation.flatpak.clone();
    let (flatpak_lines, _) = flatpak::describe(&generation.flatpak, &previous_flatpak, &flatpak::installed()?);
    let place = place_in(&paths);
    let accounts = Accounts::read(&paths.root.join("etc"));
    let system_lines = system::apply::describe(&system_change, &place, &accounts, &paths.root)?;
    println!("reverting to {name}, applying its record live, in place");
    print_warned(&warned);
    for line in &kept_lines {
        println!("{line}");
    }
    for change in &changes {
        println!("{change}");
    }
    for line in system_lines
        .iter()
        .chain(&flatpak_lines)
        .map(String::as_str)
        .chain(rollbacks.line())
    {
        println!("{line}");
    }
    print_hooks(&system_change);
    if let Some(boot) = &generation.settings.boot {
        let target_fstab: String = generation.system.fstab.iter().map(|line| line.line() + "\n").collect();
        for warning in system::fstab::rollback_warnings(
            &paths.root,
            &fs::read_to_string(paths.root.join("etc/fstab")).unwrap_or_default(),
            &target_fstab,
            &boot.esp,
        ) {
            eprintln!("{warning}");
        }
    }
    system::apply::check(&paths, &system_change, &place)?;
    system::live::check_files(&system_change, &|path: &str| {
        let file = generation.system.files.get(path)?;
        system::blobs::read(&paths, &file.sha256).ok()
    })?;
    system::live::refuse_packaged_usr_files(&system_change, &db)
        .map_err(|e| anyhow::anyhow!("{e:#}; nothing was changed"))?;
    guard_downgrades(&risky, noconfirm, allow_unsafe)?;
    if !noconfirm && !confirm(&format!("revert to {name}?"))? {
        bail!("aborted; nothing was changed");
    }
    let archive = archix::archive::for_system(&paths.state);
    let repos = third_party_repos(&paths);
    let mut packages: Packages = transaction_for(
        &archive,
        &paths,
        generation.date,
        &generation.packages,
        &changes,
        &BTreeMap::new(),
        &repos,
    )?;
    packages.install = without_signatures(&paths, &packages.install)?;
    archix::conflicts::check(&packages.install, &packages.remove, &db, &paths.root)?;
    check_packages(&packages)?;
    let rewritten = system::apply::out_of_date(&system_change, &place, &accounts)?;
    if let Some(boot) = generation.settings.boot.as_ref() {
        system::boot::check_signing(boot)?;
    }
    let taken = rollbacks
        .before(
            &paths,
            Some(&current_generation),
            format!("before revert to generation {number}"),
        )
        .map_err(|e| e.context("nothing of the revert was applied"))?;
    let root = live::Root::new(&paths.root);
    eprintln!(":: applying {name}");
    let applied = live::apply(&live::Plan {
        paths: &paths,
        root: &root,
        packages: &packages,
        system: &system_change,
        repos: &repos,
        esp: generation.settings.boot.as_ref().map(|boot| boot.esp.as_str()),
        credentials: None,
        package_capabilities: &current_generation.package_capabilities,
        rewritten: &rewritten,
        started,
    })
    .map_err(|e| {
        e.context(format!(
            "generation {current} stays current{}",
            before_note(taken.as_ref())
        ))
    })?;
    let mut soft_failures = applied.failures;
    if applied.package_capabilities != generation.package_capabilities {
        let mut updated = generation.clone();
        updated.package_capabilities = applied.package_capabilities;
        store.update(&updated)?;
    }
    store.set_current(number)?;
    soft_failures.extend(write_boot(
        &paths,
        &rollbacks,
        generation.settings.boot.as_ref(),
        &generation.packages,
    ));
    if (!generation.flatpak.is_empty() || !previous_flatpak.is_empty()) && generation.flatpak != previous_flatpak {
        soft_failures.extend(flatpak::sync(&generation.flatpak, &previous_flatpak));
    }
    let _ = fs::remove_file(paths.notes());
    match gc::collect(&paths, &user_cache()?) {
        Ok(report) => print_gc(&report, false),
        Err(e) => soft_failures.push(format!("garbage collection: {e:#}")),
    }
    println!("reverted to {name}. archix.lock is unchanged, so the next `archix switch` returns to the config");
    report_soft_failures(&soft_failures, &format!("{name} is applied and current"))
}

pub fn generations() -> Result<()> {
    let paths = Paths::system();
    let store = Store::new(&paths);
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
    let snapshots = archix::snapshot::Store::open(&paths.root).list()?;
    if !snapshots.is_empty() {
        println!("snapshots, by the state each holds:");
    }
    for record in snapshots {
        let marker = if current.is_some() && record.generation == current {
            "*"
        } else {
            " "
        };
        println!("{marker} {}", super::snapshot::describe(&record));
    }
    Ok(())
}

/// Copies what this machine keeps into `package_cache_push`, so other machines find it there; a cache that can't be written only warns.
fn push_to_cache(declared: &Declared) {
    let Some(dir) = &declared.package_cache_push else {
        return;
    };
    if let Err(e) = cache_push(std::path::Path::new(dir.strip_prefix("file://").unwrap_or(dir))) {
        eprintln!("warning: couldn't copy this machine's packages to the package cache at {dir}: {e:#}");
    }
}

/// Copies this machine's package files, their signatures and its AUR build records into a package cache directory, skipping what it already holds.
pub fn cache_push(dir: &std::path::Path) -> Result<()> {
    let paths = Paths::system();
    // Builds of private recipes, init's rebuilds of installed packages, may hold files that aren't anyone else's to have.
    let (private_records, private_files) = archix::aur::build::private_builds(&paths);
    let mut copied = 0;
    let mut copy_dir = |from: &std::path::Path, to: &std::path::Path| -> Result<()> {
        for entry in fs::read_dir(from).into_iter().flatten().flatten() {
            let (source, dest) = (entry.path(), to.join(entry.file_name()));
            if private_records.contains(&source) || private_files.contains(&*entry.file_name().to_string_lossy()) {
                continue;
            }
            if !source.is_file()
                || dest
                    .metadata()
                    .is_ok_and(|have| source.metadata().is_ok_and(|want| want.len() == have.len()))
            {
                continue;
            }
            // Created only for something to copy, so a base with only private builds leaves no trace.
            fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
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

/// Run early at boot by archix-boot.service: the hibernation guard, and a login note when the entry systemd-boot started names a generation but isn't what archix wrote.
pub fn boot_check() -> Result<()> {
    let paths = Paths::system();
    let recovered = super::keep::recover_after_cut(&paths).unwrap_or_else(|e| {
        vec![format!(
            "archix: couldn't finish a rollback a power cut stopped half way: {e:#}"
        )]
    });
    for line in &recovered {
        println!("{line}");
    }
    super::keep::after_boot(&paths);
    if let Err(e) = system::sleep::sync(&paths.root) {
        println!("archix: couldn't update the hibernation guard: {e:#}");
    }
    if let Err(e) = system::menu::adopt(&paths) {
        println!("archix: couldn't move the boot menu's state into @archix: {e:#}");
    }
    adopt_package_cache(&paths);
    if let Some(entry) = system::boot::tampered_selected_entry() {
        system::boot::add_login_note(
            &paths,
            &format!(
                "archix: the boot entry {entry} isn't what archix wrote, so it was changed on the ESP outside archix (archix's copy is {})",
                system::boot::entry_record(&paths, &entry).display()
            ),
        );
    }
    if let Ok(esp) = fs::read_to_string(paths.state.join("boot").join("esp")) {
        match system::menu::drop_second_names(Path::new(esp.trim()), system::boot::boot_count_file().as_deref()) {
            Ok(dropped) if !dropped.is_empty() => {
                println!(
                    "archix: dropped boot entries a power cut left under a second name: {}",
                    dropped.join(", ")
                )
            }
            Ok(_) => {}
            Err(e) => println!("archix: couldn't drop boot entries left under a second name: {e:#}"),
        }
    }
    if let Err(e) = record_failed_entries(&paths) {
        println!("archix: couldn't record which snapshot entries failed: {e:#}");
    }
    let mut notices = recovered;
    if let Some(trying) = system::initrd::trying() {
        notices.push(system::initrd::notice(&trying));
        if let Some(failure) = system::health::read_failure(&paths) {
            notices.push(system::health::fallback_note(&failure));
        }
        if let Some(failure) = system::emergency::read(&paths) {
            notices.push(system::emergency::fallback_note(&failure));
        }
    } else if let Some(why) = archix::snapshot::foreign_root(&paths.root) {
        notices.push(format!("archix: {why}"));
    }
    notices.extend(hook_warnings(&paths).into_iter().map(|line| format!("archix: {line}")));
    for (dir, file) in [("/run/motd.d", "archix.motd"), ("/run/issue.d", "archix.issue")] {
        let path = Path::new(dir).join(file);
        match notices.is_empty() {
            true => {
                let _ = fs::remove_file(&path);
            }
            false => {
                let _ = fs::create_dir_all(dir);
                let _ = fs::write(&path, notices.join("\n\n") + "\n\n");
            }
        }
    }
    for notice in &notices {
        println!("{notice}");
    }
    Ok(())
}

/// Notes the snapshot entries systemd-boot gave up on, which the missing-snapshot guard never boots.
fn record_failed_entries(paths: &Paths) -> Result<()> {
    let Ok(esp) = fs::read_to_string(paths.state.join("boot").join("esp")) else {
        return Ok(());
    };
    let dir = system::menu::dir(paths);
    let mut state = system::menu::load(&dir)?;
    let before = state.failed.clone();
    for name in system::menu::on_esp(Path::new(esp.trim())).values() {
        if let Some(system::menu::Entry::Snapshot(number)) = system::menu::Entry::parse(name)
            && system::boot::is_bad(name)
        {
            state.failed.insert(number);
        }
    }
    if state.failed != before {
        system::menu::save(&dir, &state)?;
    }
    Ok(())
}

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
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
use archix::deploy::{self, Keep, build::Plan, layout};
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
use archix::system::apply::{Accounts, SystemChange, deployment_tree};
use archix::system::boot::{self, BootPlan};
use archix::system::{self, SystemState};

use super::lock::{current_lock, load_lockfile, save_lock, store_lockfile};
use super::{
    Declared, Nearest, Snapshot, Target, ThirdPartyFallback, aur_git, aur_source, check_pacman_free, confirm,
    confirm_no, count, declared, edit_installed, ensure_flatpak_remotes, keep_third_party_copies,
    print_missing_ignored, print_warned, recipes, require_root, select_changes, settle_flatpak, third_party_repos,
    transaction_for, user_cache,
};

/// The current deployment's directory, which holds etc/ and usr/ like a root, or the live root before the first layout.
pub(super) fn current_root(paths: &Paths) -> PathBuf {
    match Store::new(paths).current() {
        Ok(Some(number)) => paths.root.join("archix/deploy").join(number.to_string()),
        _ => paths.root.clone(),
    }
}

/// The pacman database of the current deployment, which the next one is staged from.
pub(super) fn installed_db(paths: &Paths) -> PathBuf {
    let here = current_root(paths);
    match here == paths.root {
        true => paths.root.join("var/lib/pacman/local"),
        false => here.join("pacman"),
    }
}

/// Where a managed path is now: in the current deployment for its trees, else on the live system.
fn place_in(paths: &Paths) -> impl Fn(&str) -> Option<PathBuf> {
    let here = current_root(paths);
    let root = paths.root.clone();
    move |path: &str| {
        let rel = path.trim_start_matches('/');
        Some(match deployment_tree(path) {
            Some(_) => here.join(rel),
            None => root.join(rel),
        })
    }
}

/// Where the live pass writes a managed path outside the deployment trees.
fn live_place(paths: &Paths) -> impl Fn(&str) -> Option<PathBuf> {
    let root = paths.root.clone();
    move |path: &str| {
        deployment_tree(path)
            .is_none()
            .then(|| root.join(path.trim_start_matches('/')))
    }
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
    let evaluation = eval::evaluate(&eval::Input {
        config_dir: target.config_dir.to_path_buf(),
        hostname: target.host.to_string(),
        overlay: super::lock::deferred_overlay(target.config_dir),
    })
    .map_err(|errors| anyhow::anyhow!("{errors}"))?;
    let target = Target {
        config: &evaluation.config,
        ..*target
    };
    switch(&target, noconfirm, accept_nearest, allow_unsafe).context("installed.lua was left as it was")
}

pub fn switch(target: &Target, noconfirm: bool, accept_nearest: Option<Nearest>, allow_unsafe: bool) -> Result<()> {
    require_root("switch")?;
    let paths = Paths::system();
    check_pacman_free(&paths)?;
    let declared = declared(target.config)?;
    let boot = declared.boot_plan()?;
    let laid_out = layout::laid_out(&paths.root);
    if !laid_out && let Some(why) = layout::unsupported(&paths.root, Path::new(&boot.esp)) {
        bail!("{why}; nothing was changed");
    }
    if let Some(why) = layout::separate_var(&paths.root) {
        bail!("{why}; nothing was changed");
    }
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
            deployments_keep: declared.deployments_keep,
            boot: Some(boot.clone()),
        },
        config_commit: commit.as_ref().map(|(hash, _)| hash.clone()),
        config_dirty: commit.is_some_and(|(_, dirty)| dirty),
        system: SystemState::default(),
        flatpak: lock.flatpak.clone(),
        before_archix: false,
    };
    let rendered = system::render(target.config, target.config_dir)?;
    generation.system = rendered.state.clone();
    let system_change = SystemChange {
        from: current.as_ref().map(|g| g.system.clone()).unwrap_or_default(),
        to: rendered.state.clone(),
    };
    let here = current_root(&paths);
    let place = place_in(&paths);
    let accounts = Accounts::read(&here.join("etc"));
    let system_lines = system::apply::describe(&system_change, &place, &accounts, &here)?;
    system::apply::check(&paths, &system_change, &place)?;
    if let Some(rules) = rendered.state.files.get(system::firewall::RULES) {
        system::firewall::validate(&String::from_utf8_lossy(&rendered.blobs[&rules.sha256]))?;
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
            .map(|(_, id)| format!("flatpak    install {id} at the new deployment's first boot, which brings flatpak"))
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
    if laid_out && nothing_changes && unchanged {
        let current = current.expect("unchanged needs a current generation");
        ensure_entry(&paths, &boot, current.number)?;
        super::lock::write_deferred()?;
        carry_config(
            target.config_dir,
            &paths,
            &paths.root.join("archix/deploy").join(current.number.to_string()),
        )?;
        integration::clear_outside_changes(&paths)?;
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
    for line in system_lines.iter().chain(&flatpak_lines) {
        println!("{line}");
    }
    if !laid_out {
        println!(
            "lay out   the system as deployment 1: /usr, /etc, /opt, /boot and pacman's database move into {}, with a boot entry of its own",
            paths.root.join("archix/deploy/1").display()
        );
    }
    if !(nothing_changes && !laid_out) {
        println!("a new deployment with its own boot entry; it boots at the next restart");
    }
    print_hooks(&system_change);
    system::boot::check_signing(boot.secure_boot)?;
    let esp = PathBuf::from(&boot.esp);
    let pinned: BTreeSet<u32> = match laid_out {
        true => {
            let pins = Keep::read(
                declared.deployments_keep as usize,
                &deploy::Store::open(&paths.root)?,
                &esp,
            )?;
            [pins.booted, pins.current, pins.boot_default]
                .into_iter()
                .flatten()
                .collect()
        }
        false => BTreeSet::new(),
    };
    let kernel = boot::owned_version(&db, &boot.kernel)
        .map(|version| here.join("usr/lib/modules").join(version).join("vmlinuz"));
    system::boot::check_space(&esp, &pinned, kernel.as_deref(), boot.anticheat)?;
    guard_downgrades(
        &unsafe_downgrades(&changes, &rendered.state.unsafe_downgrades),
        noconfirm,
        allow_unsafe,
    )?;
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
    let mut soft_failures = Vec::new();
    if !laid_out {
        let first = if nothing_changes {
            system::apply::keep_originals(&paths, &system_change, &place, &accounts)?;
            Generation {
                number: 1,
                ..generation.clone()
            }
        } else {
            before_archix(&generation, &installed)
        };
        first_layout(&paths, &boot, &first)?;
        if nothing_changes {
            let (live, at_boot) =
                system::hooks::split_live(system::hooks::due(&system_change, &rewritten), &paths.root);
            if let Err(e) = system::hooks::defer(&paths.state, 1, &at_boot, &system_change.to.hooks) {
                soft_failures.push(format!("recording the hooks for deployment 1's boot: {e:#}"));
            }
            return after_commit(
                target,
                &declared,
                &paths,
                &mut lock,
                &store,
                1,
                &system_change,
                &previous_flatpak,
                live,
                Vec::new(),
                soft_failures,
            );
        }
    }
    let deployments = deploy::Store::open(&paths.root)?;
    let base = deployments
        .current()?
        .context("there's no current deployment to build the next one from")?;
    ensure_entry(&paths, &boot, base)?;
    let number = store.next_number()?.max(deployments.next_number()?);
    generation.number = number;
    let system_change = SystemChange {
        from: store.get(base)?.system,
        to: generation.system.clone(),
    };
    let mut due = BTreeMap::new();
    if system::boot::booted_deployment(&paths.root) != Some(base) {
        due.extend(system::hooks::pending(&paths.state, base).on_change);
    }
    let (live, at_boot) = system::hooks::split_live(system::hooks::due(&system_change, &rewritten), &paths.root);
    due.extend(at_boot);
    system::hooks::defer(&paths.state, number, &due, &system_change.to.hooks)?;
    eprintln!(":: building deployment {number} from deployment {base}");
    let made = deploy::build::build(
        &deployments,
        &Plan {
            paths: &paths,
            number,
            base,
            packages: &package_changes,
            system: &system_change,
            boot: &boot,
            repos: &declared.repos,
            record: &generation,
            keep: declared.deployments_keep as usize,
            config: &|staged: &Path| {
                carry_config(target.config_dir, &paths, staged)?;
                super::lock::stage_deferred(&paths, &staged.join("etc"))
            },
        },
    )
    .inspect_err(|_| {
        let _ = fs::remove_file(system::hooks::pending_path(&paths.state, number));
    })?;
    soft_failures.extend(made.failures);
    after_commit(
        target,
        &declared,
        &paths,
        &mut lock,
        &store,
        number,
        &system_change,
        &previous_flatpak,
        live,
        made.users,
        soft_failures,
    )
}

/// Writes the managed files outside the deployment trees, then runs the on_change scripts of those that changed; when the write fails, the scripts wait for the deployment's boot instead.
fn write_live_files(
    paths: &Paths,
    number: u32,
    change: &SystemChange,
    accounts: &Accounts,
    on_change: &BTreeMap<String, system::Hook>,
    soft_failures: &mut Vec<String>,
) {
    if let Err(e) = system::apply::files(paths, change, &live_place(paths), accounts) {
        soft_failures.push(format!("writing files outside the deployment: {e:#}"));
        if let Err(e) = system::hooks::defer(&paths.state, number, on_change, &change.to.hooks) {
            soft_failures.push(format!(
                "recording on_change scripts for deployment {number}'s boot: {e:#}"
            ));
        }
        return;
    }
    if !on_change.is_empty() {
        eprintln!(":: running the on_change scripts of files written now");
    }
    soft_failures.extend(system::hooks::run(&paths.root, on_change, &BTreeMap::new()));
}

/// Everything a switch does once its deployment is committed: config writes, files outside the deployment, homes, Flatpak apps, hooks and gc; failures here are soft.
#[allow(clippy::too_many_arguments)]
fn after_commit(
    target: &Target,
    declared: &Declared,
    paths: &Paths,
    lock: &mut HostLock,
    store: &Store,
    number: u32,
    system_change: &SystemChange,
    previous_flatpak: &BTreeMap<String, LockedApp>,
    live_on_change: BTreeMap<String, system::Hook>,
    users: Vec<String>,
    mut soft_failures: Vec<String>,
) -> Result<()> {
    // A config outside /etc gets this switch's writes only here, so a crash right after the commit leaves it as it was.
    super::lock::write_deferred()?;
    let deployment = paths.root.join("archix/deploy").join(number.to_string());
    let accounts = Accounts::read(&deployment.join("etc"));
    write_live_files(
        paths,
        number,
        system_change,
        &accounts,
        &live_on_change,
        &mut soft_failures,
    );
    if let Err(e) = system::apply::create_homes(&users, &accounts, &deployment.join("etc/skel"), &paths.root) {
        soft_failures.push(format!("creating home directories: {e:#}"));
    }
    integration::clear_outside_changes(paths)?;
    let _ = fs::remove_file(paths.notes());
    system::reboot::refresh_note(&paths.root);
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
    soft_failures.extend(finish_flatpak(
        target,
        declared,
        lock,
        store,
        &deployment,
        number,
        previous_flatpak,
    )?);
    super::lock::write_deferred()?;
    if let Err(e) = carry_config(target.config_dir, paths, &deployment) {
        soft_failures.push(format!("copying the config into deployment {number}: {e:#}"));
    }
    collect_and_revoke(
        paths,
        declared.boot.as_ref().is_some_and(|boot| boot.revoke_unused),
        &mut soft_failures,
    )?;
    println!(
        "switched to generation {number} (snapshot {}); its deployment boots at the next restart",
        lock.date
    );
    if super::lock::pending_etc(paths).is_some() {
        println!("{}", super::lock::pending_note(number));
    }
    push_to_cache(declared);
    report_soft_failures(
        &soft_failures,
        &format!("generation {number} is committed and boots at the next restart"),
    )
}

/// The record of the system as the first switch found it, which deployment 1 holds when the switch changes something.
fn before_archix(target: &Generation, installed: &BTreeMap<String, archix::pacman::LocalPkg>) -> Generation {
    let packages = installed
        .values()
        .map(|pkg| {
            let locked = Locked {
                version: pkg.version.clone(),
                explicit: pkg.explicit,
                ..Default::default()
            };
            (pkg.name.clone(), locked)
        })
        .collect();
    Generation {
        number: 1,
        packages,
        system: SystemState::default(),
        flatpak: BTreeMap::new(),
        config_commit: None,
        config_dirty: false,
        before_archix: true,
        ..target.clone()
    }
}

/// Lays the live system out as deployment 1 with its own boot entry: archix's integration and the initramfs hook first, then the layout, the entry and the live binds; what they hide is pruned at the next boot, once no service still uses it.
fn first_layout(paths: &Paths, boot: &BootPlan, record: &Generation) -> Result<()> {
    eprintln!(":: laying the system out as deployment 1");
    let archix = integration::own_path()?;
    let root = paths.root.clone();
    integration::write_all(paths, &archix, &|path: &Path| {
        Ok(root.join(path.strip_prefix("/").unwrap_or(path)))
    })?;
    let store = Store::new(paths);
    match store.get(1) {
        Ok(_) => store.update(record)?,
        Err(_) => store.save(record)?,
    }
    for warning in system::boot::firmware_warnings(&paths.root.join("sys/firmware/efi/efivars"), Path::new(&boot.esp)) {
        eprintln!("warning: {warning}");
    }
    let deployments = layout::lay_out(&paths.root)?;
    deploy::build::entry_for(&deployments, paths, boot, 1)?;
    layout::bind_live(&paths.root, &deployments, 1)?;
    println!(
        "laid the system out as deployment 1, which boots from its own entry; changes from now on take effect at the next boot"
    );
    Ok(())
}

fn identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).ok().map(|meta| (meta.dev(), meta.ino()))
}

/// Copies the config directory as it is now into a deployment's /etc when it lives in /etc itself, so the deployment boots with the lock its switch wrote, not the copy it was staged with; one linked out of /etc is shared already.
pub(super) fn carry_config(config_dir: &Path, paths: &Paths, deployment: &Path) -> Result<()> {
    let (Ok(dir), Ok(etc)) = (fs::canonicalize(config_dir), fs::canonicalize(paths.root.join("etc"))) else {
        return Ok(());
    };
    let Ok(rel) = dir.strip_prefix(&etc) else {
        return Ok(());
    };
    let target = deployment.join("etc").join(rel);
    if identity(&target).is_some() && identity(&target) == identity(&dir) {
        return Ok(());
    }
    let (Some(parent), Some(name)) = (target.parent(), target.file_name()) else {
        return Ok(());
    };
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let fresh = parent.join(format!(".{}.archix-new", name.to_string_lossy()));
    if fresh.symlink_metadata().is_ok() {
        fs::remove_dir_all(&fresh).with_context(|| format!("removing {}", fresh.display()))?;
    }
    let copied = Command::new("cp")
        .args(["-a", "--no-target-directory"])
        .arg(&dir)
        .arg(&fresh)
        .status()
        .context("running cp")?;
    if !copied.success() {
        bail!("copying {} to {} failed", dir.display(), fresh.display());
    }
    if target.symlink_metadata().is_err() {
        fs::rename(&fresh, &target).with_context(|| format!("renaming {}", fresh.display()))?;
        return archix::durable::sync_dir(&target);
    }
    let c = |path: &Path| std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).context("path has a NUL byte");
    let (c_fresh, c_target) = (c(&fresh)?, c(&target)?);
    // Swapped in one step, so a crash leaves the deployment one whole config or the other.
    let swapped = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            c_fresh.as_ptr(),
            libc::AT_FDCWD,
            c_target.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if swapped != 0 {
        return Err(io::Error::last_os_error()).with_context(|| format!("replacing {}", target.display()));
    }
    archix::durable::sync_dir(&target)?;
    fs::remove_dir_all(&fresh).with_context(|| format!("removing {}", fresh.display()))
}

/// Makes the fallback UKI boot deployment `number`, a copy of its entry's UKI, writing the entry first if the ESP lost it.
fn follow_with_fallback(paths: &Paths, boot: &BootPlan, deployments: &deploy::Store, number: u32) -> Result<()> {
    if system::boot::follow_with_fallback(paths, Path::new(&boot.esp), number)? {
        return Ok(());
    }
    deploy::build::entry_for(deployments, paths, boot, number)
}

/// Writes the boot entry of deployment `number` when the ESP has none, as after a first layout cut off before its entry.
fn ensure_entry(paths: &Paths, boot: &BootPlan, number: u32) -> Result<()> {
    if system::boot::entries(Path::new(&boot.esp))
        .iter()
        .any(|(n, extra, _)| *n == number && !extra)
    {
        return Ok(());
    }
    eprintln!(":: deployment {number} has no boot entry on {}; writing it", boot.esp);
    deploy::build::entry_for(&deploy::Store::open(&paths.root)?, paths, boot, number)
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

/// The preview line naming the hooks a switch or rollback runs, in order, at the boot of the deployment it makes current.
fn print_hooks(change: &SystemChange) {
    if let Ok(names) = system::hooks::order(&change.to.hooks)
        && !names.is_empty()
    {
        println!("run hooks  {}, when the deployment boots", names.join(", "));
    }
}

/// gc and revoking the boot images no entry uses, after every switch or rollback that commits a deployment.
fn collect_and_revoke(paths: &Paths, revoke_unused: bool, soft_failures: &mut Vec<String>) -> Result<()> {
    match gc::collect(paths, &user_cache()?) {
        Ok(report) => print_gc(&report, false),
        Err(e) => soft_failures.push(format!("garbage collection: {e:#}")),
    }
    if revoke_unused && let Err(e) = system::revoke::revoke(paths, false, true, &|_| Ok(true)) {
        soft_failures.push(format!("revoking boot images no entry uses: {e:#}"));
    }
    Ok(())
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
    let broken =
        archix::soname::broken_builds(&current_root(paths), &installed_db(paths), &built, &changing, |owner| {
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
        })?;
    Ok(broken
        .into_iter()
        .map(|name| Change::Rebuild {
            version: lock.packages[&name].version.clone(),
            name,
        })
        .collect())
}

/// Locks Flatpak apps that couldn't be locked before flatpak was installed, then installs and pins them; when flatpak comes only with the new deployment, leaves that to its first boot.
fn finish_flatpak(
    target: &Target,
    declared: &Declared,
    lock: &mut HostLock,
    store: &Store,
    deployment: &Path,
    number: u32,
    previous: &BTreeMap<String, LockedApp>,
) -> Result<Vec<String>> {
    let state = &Paths::system().state;
    if declared.flatpak_apps.is_empty() && previous.is_empty() {
        flatpak::drop_pending(state, None);
        return Ok(Vec::new());
    }
    if !flatpak::available() && deployment.join("usr/bin/flatpak").exists() {
        let remotes = declared
            .flatpak_remotes
            .iter()
            .filter(|(name, _)| declared.flatpak_apps.iter().any(|(remote, _)| remote == *name))
            .map(|(name, remote)| (name.clone(), remote.clone()))
            .collect();
        let pending = flatpak::Pending {
            config_dir: std::path::absolute(target.config_dir)?,
            host: target.host.to_string(),
            apps: declared.flatpak_apps.clone(),
            remotes,
            locked: lock.flatpak.clone(),
            previous: previous.clone(),
        };
        flatpak::defer(state, number, &pending)?;
        let ids: Vec<&str> = declared.flatpak_apps.iter().map(|(_, id)| id.as_str()).collect();
        println!(
            "flatpak comes with deployment {number}, so deployment {number}'s first boot installs them once the network is up: {}",
            ids.join(", ")
        );
        return Ok(Vec::new());
    }
    flatpak::drop_pending(state, None);
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
    if !report.deployments.is_empty() {
        let numbers: Vec<String> = report.deployments.iter().map(u32::to_string).collect();
        println!("deleted deployments {}", numbers.join(", "));
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

/// The kept deployment nearest generation `number`, which a rollback to it rebuilds from.
fn nearest_deployment(committed: &[u32], number: u32) -> Option<u32> {
    committed
        .iter()
        .copied()
        .min_by_key(|n| (n.abs_diff(number), *n < number))
}

/// Makes an earlier generation current: by repointing when its deployment is kept, else by rebuilding that deployment from the nearest kept one and the package cache; archix.lock stays as it is.
pub fn rollback(config_dir: &Path, to: Option<u32>, noconfirm: bool, allow_unsafe: bool) -> Result<()> {
    require_root("rollback")?;
    let paths = Paths::system();
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
        if layout::laid_out(&paths.root) {
            deploy::Store::open(&paths.root)?.choose(number)?;
        }
        println!("generation {number} is current already; it stays, and gc may now drop newer deployments");
        return Ok(());
    }
    let generation = store.get(number)?;
    let current_generation = store.get(current)?;
    let boot = generation
        .settings
        .boot
        .clone()
        .with_context(|| format!("generation {number} has no boot settings, so it can't boot"))?;
    let deployments = deploy::Store::open(&paths.root)?;
    let committed = deployments.deployments()?;
    let kept = committed.contains(&number);
    let base = match kept {
        true => number,
        false => {
            if generation.before_archix {
                bail!(
                    "generation {number} is the system as archix found it, and its deployment was dropped, so it can't be rebuilt; nothing was changed"
                );
            }
            nearest_deployment(&committed, number).context("there's no deployment to rebuild from")?
        }
    };
    let base_db = deployments.path(base).join("pacman");
    let installed = read_local_db(&base_db)?;
    let ignored = generation.settings.ignored.iter().cloned().collect();
    let base_generation = store.get(base)?;
    let (mut changes, warned, kept_lines) = select_changes(
        &generation.packages,
        &installed,
        &base_db,
        generation.settings.remove_undeclared,
        &ignored,
    );
    changes.extend(rebuilds(
        &generation.packages,
        &installed,
        Some(&base_generation.packages),
    ));
    let swapped = swapped_builds(
        &generation.packages,
        &installed,
        Some(&base_generation.packages),
        &changes,
    );
    changes.extend(swapped);
    let name = format!("generation {number} (snapshot {})", generation.date);
    let current_change = SystemChange {
        from: current_generation.system.clone(),
        to: generation.system.clone(),
    };
    let current_db = deployments.path(current).join("pacman");
    // A kept deployment boots as it is, which plain pacman may have changed since its generation was recorded.
    let target = match kept {
        true => read_local_db(&base_db)?
            .into_values()
            .map(|pkg| {
                let locked = Locked {
                    version: pkg.version,
                    explicit: pkg.explicit,
                    ..Default::default()
                };
                (pkg.name, locked)
            })
            .collect(),
        false => generation.packages.clone(),
    };
    let (versus_current, _, _) = select_changes(&target, &read_local_db(&current_db)?, &current_db, true, &ignored);
    let configured = merged(
        &current_change.from.unsafe_downgrades,
        &generation.system.unsafe_downgrades,
    );
    let risky = unsafe_downgrades(&versus_current, &configured);
    let previous_flatpak = current_generation.flatpak.clone();
    let (flatpak_lines, _) = flatpak::describe(&generation.flatpak, &previous_flatpak, &flatpak::installed()?);
    if kept {
        println!(
            "{name} still has its deployment: rolling back points current and the boot default at it, nothing is reinstalled"
        );
        for change in &versus_current {
            println!("{change}");
        }
    } else {
        println!("{name}'s deployment was dropped: rebuilding it from deployment {base} and the package cache");
        print_warned(&warned);
        for line in &kept_lines {
            println!("{line}");
        }
        for change in &changes {
            println!("{change}");
        }
    }
    for line in &flatpak_lines {
        println!("{line}");
    }
    print_hooks(&current_change);
    guard_downgrades(&risky, noconfirm, allow_unsafe)?;
    if !noconfirm && !confirm(&format!("roll back to {name}?"))? {
        bail!("aborted; nothing was changed");
    }
    let mut soft_failures = Vec::new();
    let mut users = Vec::new();
    let esp = PathBuf::from(&boot.esp);
    if kept {
        deploy::verify::ensure_intact(&paths, &deployments, number)?;
        ensure_entry(&paths, &boot, number)?;
        // The default first: a crash between the two leaves the boot default on `number`, whose boot makes it current.
        boot::set_default_entry(&esp, number)?;
        archix::durable::sync_filesystems_checked(&[&esp])?;
        follow_with_fallback(&paths, &boot, &deployments, number)?;
        deployments.point_current(number)?;
        deployments.choose(number)?;
    } else {
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
        archix::conflicts::check(&packages.install, &packages.remove, &base_db, &deployments.path(base))?;
        check_packages(&packages)?;
        let system_change = SystemChange {
            from: base_generation.system.clone(),
            to: generation.system.clone(),
        };
        eprintln!(":: rebuilding deployment {number} from deployment {base}");
        let made = deploy::build::build(
            &deployments,
            &Plan {
                paths: &paths,
                number,
                base,
                packages: &packages,
                system: &system_change,
                boot: &boot,
                repos: &repos,
                record: &generation,
                keep: generation.settings.deployments_keep as usize,
                config: &|staged: &Path| carry_config(config_dir, &paths, staged),
            },
        )?;
        soft_failures.extend(made.failures);
        users = made.users;
    }
    let (live, at_boot) = system::hooks::split_live(system::hooks::due(&current_change, &BTreeSet::new()), &paths.root);
    let hooks = system::hooks::defer(&paths.state, number, &at_boot, &generation.system.hooks);
    if let Err(e) = hooks {
        soft_failures.push(format!("recording the hooks for deployment {number}'s boot: {e:#}"));
    }
    let deployment = deployments.path(number);
    if let Err(e) = carry_config(config_dir, &paths, &deployment) {
        soft_failures.push(format!("copying the config into deployment {number}: {e:#}"));
    }
    let accounts = Accounts::read(&deployment.join("etc"));
    write_live_files(&paths, number, &current_change, &accounts, &live, &mut soft_failures);
    if let Err(e) = system::apply::create_homes(&users, &accounts, &deployment.join("etc/skel"), &paths.root) {
        soft_failures.push(format!("creating home directories: {e:#}"));
    }
    if (!generation.flatpak.is_empty() || !previous_flatpak.is_empty()) && generation.flatpak != previous_flatpak {
        flatpak::drop_pending(&paths.state, None);
        soft_failures.extend(flatpak::sync(&generation.flatpak, &previous_flatpak));
    }
    let _ = fs::remove_file(paths.notes());
    system::reboot::refresh_note(&paths.root);
    if !kept {
        collect_and_revoke(&paths, boot.revoke_unused, &mut soft_failures)?;
    }
    println!(
        "rolled back to {name}; it boots at the next restart. archix.lock is unchanged, so the next `archix switch` returns to the config"
    );
    if super::lock::pending_etc(&paths).is_some() {
        println!("{}", super::lock::pending_note(number));
    }
    report_soft_failures(
        &soft_failures,
        &format!("{name} is current and boots at the next restart"),
    )
}

pub fn generations() -> Result<()> {
    let paths = Paths::system();
    let store = Store::new(&paths);
    let current = store.current()?;
    let booted = boot::booted_deployment(&paths.root);
    let deployed: BTreeSet<u32> = match layout::laid_out(&paths.root) {
        true => deploy::Store::open(&paths.root)?.deployments()?.into_iter().collect(),
        false => BTreeSet::new(),
    };
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
        let state = match (deployed.contains(&generation.number), booted == Some(generation.number)) {
            (true, true) => "  deployment, booted",
            (true, false) => "  deployment",
            (false, _) => "  record",
        };
        println!(
            "{marker} {:>4}  {created} {:02}:{:02} UTC  snapshot {}  {}{commit}{state}",
            generation.number,
            minutes / 60,
            minutes % 60,
            generation.date,
            count(generation.packages.len(), "package", "packages")
        );
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
}

/// Run by archix-flatpak.service once the network is up: installs the Flatpak apps left for the booted deployment, records their commits in its generation and in archix.lock, and keeps the record for the next boot if anything failed.
pub fn deploy_flatpak() -> Result<()> {
    let paths = Paths::system();
    let Some(number) = system::boot::booted_deployment(&paths.root) else {
        return Ok(());
    };
    let Some(mut pending) = flatpak::pending(&paths.state, number) else {
        return Ok(());
    };
    let ids: Vec<&str> = pending.apps.iter().map(|(_, id)| id.as_str()).collect();
    println!(
        "archix: installing deployment {number}'s Flatpak apps: {}",
        ids.join(", ")
    );
    let (locked, mut failures) = flatpak::install_pending(&pending);
    let store = Store::new(&paths);
    if let Ok(mut generation) = store.get(number)
        && generation.flatpak != locked
    {
        generation.flatpak = locked.clone();
        if let Err(e) = store.update(&generation) {
            failures.push(format!("recording the commits in generation {number}: {e:#}"));
        }
    }
    let lock_path = pending.config_dir.join(archix::lock::LOCK_FILE);
    let newly: Vec<(&String, &LockedApp)> = locked
        .iter()
        .filter(|(id, _)| !pending.locked.contains_key(*id))
        .collect();
    if !newly.is_empty()
        && let Err(e) = archix::lock::Lockfile::load(&lock_path).and_then(|mut lockfile| {
            let Some(host) = lockfile.hosts.get_mut(&pending.host) else {
                return Ok(());
            };
            for (id, app) in &newly {
                host.flatpak.entry((*id).clone()).or_insert_with(|| (*app).clone());
            }
            super::lock::write_config(&lock_path, lockfile.render().as_bytes())
        })
    {
        failures.push(format!("writing the commits to {}: {e:#}", lock_path.display()));
    }
    if failures.is_empty() {
        flatpak::drop_pending(&paths.state, Some(number));
        return Ok(());
    }
    pending.locked = locked;
    let _ = flatpak::defer(&paths.state, number, &pending);
    let mut notes = fs::read_to_string(paths.notes()).unwrap_or_default();
    for failure in &failures {
        println!("archix: flatpak: {failure}; it's tried again at the next boot");
        notes.push_str(&format!(
            "archix: at boot, flatpak: {failure}; it's tried again at the next boot\n"
        ));
    }
    let _ = archix::durable::write(&paths.notes(), notes.as_bytes());
    bail!("installing deployment {number}'s Flatpak apps failed")
}

/// Run by `archix prune-hidden` in its own mount namespace.
pub fn prune_hidden(root: &Path) -> Result<()> {
    let removed = layout::prune_hidden_here(root)?;
    if removed > 0 {
        println!(
            "removed {} from before the first layout",
            count(removed, "entry", "entries")
        );
    }
    Ok(())
}

/// Run early at boot by archix-boot.service: a boot of another deployment than the current one, picked in the menu or fallen back to after failed tries, makes that one current; nothing is reinstalled.
pub fn boot_check(cmdline: Option<String>) -> Result<()> {
    let paths = Paths::system();
    let laid_out = layout::laid_out(&paths.root);
    if laid_out && !paths.state.join("layout-pruned").exists() {
        match integration::own_path().and_then(|archix| layout::prune_hidden(&paths.root, &archix)) {
            Ok(()) => {
                let _ = archix::durable::write(&paths.state.join("layout-pruned"), b"");
            }
            Err(e) => println!("archix: {e:#}"),
        }
    }
    let booted = match cmdline {
        Some(cmdline) => system::boot::booted_generation(&cmdline),
        None => system::boot::booted(),
    };
    let Some(booted) = booted else {
        if let Some(entry) = system::boot::tampered_selected_entry() {
            println!(
                "archix: the boot entry {entry} isn't the entry archix wrote for that generation, so it may have been changed on the ESP; leaving current as it is"
            );
        }
        return Ok(());
    };
    if !laid_out {
        return Ok(());
    }
    let deployments = deploy::Store::open(&paths.root)?;
    if !deployments.deployments()?.contains(&booted) {
        println!("archix: the boot entry names generation {booted}, which has no deployment; leaving current as it is");
        return Ok(());
    }
    let current = deployments.current()?;
    if current == Some(booted) {
        return Ok(());
    }
    let esp = fs::read_to_string(paths.state.join("boot").join("esp")).map(|esp| PathBuf::from(esp.trim()));
    if let Some(current) = current
        && booted > current
        && !esp.as_ref().is_ok_and(|esp| system::boot::loader_names(esp, booted))
    {
        // Only loader.conf going missing or torn makes systemd-boot start the newest entry on its own, which would undo a rollback.
        println!(
            "archix: systemd-boot started generation {booted}, newer than the current {current}, though loader.conf doesn't name it; {current} stays current and the boot default (`archix rollback {booted}` makes {booted} current)"
        );
        if let Ok(esp) = &esp
            && let Err(e) = system::boot::set_default_entry(esp, current)
        {
            println!("archix: couldn't make generation {current} the boot default again: {e:#}");
        }
        return Ok(());
    }
    let note = match current {
        Some(current) if system::boot::failed_to_boot(&paths, current) => Some(format!(
            "archix: generation {current} didn't finish booting in the tries it had, so systemd-boot started generation {booted}, which is current now"
        )),
        Some(current) => {
            println!("archix: booted generation {booted}, but {current} was current; {booted} is current now");
            None
        }
        None => None,
    };
    if let Ok(esp) = fs::read_to_string(paths.state.join("boot").join("esp"))
        && let Err(e) = system::boot::set_default_entry(Path::new(esp.trim()), booted)
    {
        println!("archix: couldn't make generation {booted} the boot default: {e:#}");
    }
    if let Ok(esp) = &esp
        && let Err(e) = system::boot::follow_with_fallback(&paths, esp, booted)
    {
        println!("archix: couldn't point the fallback UKI at generation {booted}: {e:#}");
    }
    deployments.point_current(booted)?;
    if let Some(note) = note {
        println!("{note}");
        let mut notes = fs::read_to_string(paths.notes()).unwrap_or_default();
        notes.push_str(&note);
        notes.push('\n');
        let _ = fs::write(paths.notes(), notes);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dropped_generation_is_rebuilt_from_the_nearest_kept_deployment() {
        assert_eq!(nearest_deployment(&[2, 7, 9], 6), Some(7));
        assert_eq!(nearest_deployment(&[2, 7, 9], 3), Some(2));
        assert_eq!(nearest_deployment(&[4, 8], 6), Some(8), "a tie goes to the newer one");
        assert_eq!(nearest_deployment(&[], 6), None);
    }
}

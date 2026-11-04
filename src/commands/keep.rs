//! `archix rollback N`: keeps snapshot N, which becomes `@` from the next boot; and `archix rollback --cancel`, which swaps the replaced root back before that boot.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use archix::paths::Paths;
use archix::snapshot::{self, layout, varlib};
use archix::system::{self, accounts, boot, menu};
use archix::value::Value;

use super::snapshot::{Top, archix_mounted, contents, current_generation, root_device, store};
use super::{check_pacman_free, confirm, require_root};

/// In @archix's snapshot directory while a keep waits for its reboot: what `--cancel` needs to undo it.
const DONE: &str = "keep-done.json";
/// In the kept root's archix state: the accounts the keep brought back locked, which `archix user unlock` may enable.
pub(super) const PLACEHOLDERS: &str = "placeholders.json";

#[derive(Serialize, Deserialize)]
struct Done {
    from: u32,
    replaced: u32,
    /// The menu's images before the keep, which a cancel puts back.
    images: menu::Images,
    #[serde(default)]
    moved: Option<PathBuf>,
}

/// The users the config applied in a snapshot declares, from the plan root's repository holds for its generation; refuses when that copy doesn't verify.
fn declared_users(paths: &Paths, record: &snapshot::Record) -> Result<BTreeSet<String>> {
    let (Some(generation), Some(commit)) = (record.generation, &record.commit) else {
        return Ok(BTreeSet::new());
    };
    let stored =
        archix::captured::Applied::verified_at(&paths.root, &paths.state, generation, commit, record.tree.as_deref())
            .with_context(|| {
            format!(
                "snapshot {}'s generation {generation} can't be read back from root's repository; nothing was changed",
                record.number
            )
        })?;
    let plan = archix::eval::isolated::read_plan(&stored.plan)
        .with_context(|| format!("generation {generation}'s stored plan doesn't parse; nothing was changed"))?;
    let Value::Map(config) = plan else {
        return Ok(BTreeSet::new());
    };
    Ok(match config.get("users") {
        Some(Value::Map(users)) => users.keys().cloned().collect(),
        _ => BTreeSet::new(),
    })
}

/// archix's per-root state inside a root that isn't mounted as one: on the rollback layout, under /var/rollback.
fn state_in(root: &Path) -> Option<PathBuf> {
    let state = root.join(layout::ON_ROOT).join("archix");
    state.is_dir().then_some(state)
}

/// pacman's database inside a root that isn't mounted as one.
fn local_db_in(root: &Path) -> PathBuf {
    let rolled = root.join(layout::ON_ROOT).join("pacman/local");
    match rolled.is_dir() {
        true => rolled,
        false => root.join("var/lib/pacman/local"),
    }
}

/// Carries the generation records `from` has and `to` lacks, so numbers stay unique across a keep, and makes `generation` current in `to`.
fn carry_generations(from: &Path, to: &Path, generation: Option<u32>) -> Result<()> {
    let (from_dir, to_dir) = (from.join("generations"), to.join("generations"));
    fs::create_dir_all(&to_dir).with_context(|| format!("creating {}", to_dir.display()))?;
    for entry in fs::read_dir(&from_dir).into_iter().flatten().flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().parse::<u32>().is_err() || to_dir.join(&name).exists() {
            continue;
        }
        let output = Command::new("cp")
            .args(["-a", "--reflink=auto"])
            .arg(entry.path())
            .arg(to_dir.join(&name))
            .output()
            .context("running cp")?;
        if !output.status.success() {
            bail!(
                "couldn't copy {}: {}",
                entry.path().display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    if let Some(generation) = generation
        && to_dir.join(generation.to_string()).is_dir()
    {
        archix::durable::write(&to_dir.join("current"), format!("{generation}\n").as_bytes())?;
    }
    Ok(())
}

/// Writes the account files of the clone at `clone` for a keep, from its own and the replaced root's at `replaced`; returns the placeholders and what was left out.
fn carry_accounts(clone: &Path, replaced: &Path, declared: &BTreeSet<String>) -> Result<accounts::Kept> {
    let read = |root: &Path| -> accounts::Texts {
        accounts::FILES.map(|file| fs::read_to_string(root.join("etc").join(file)).ok())
    };
    let kept = accounts::keep(&read(clone), &read(replaced), declared);
    for (file, text) in accounts::FILES.iter().zip(&kept.texts) {
        if let Some(text) = text {
            let path = clone.join("etc").join(file);
            // Truncated in place, so the file keeps its mode and owner.
            fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        }
    }
    Ok(kept)
}

/// The images the kept root boots: the snapshot's own when the ESP still has them, else the current ones when the kernel and initramfs config are the same, else built from the clone at `clone` and put on the ESP.
fn kept_images(
    paths: &Paths,
    plan: &boot::BootPlan,
    number: u32,
    replaced_root: &Path,
    snapshot_root: &Path,
    clone: &Path,
) -> Result<menu::Images> {
    let state = menu::load(&menu::dir(paths))?;
    let esp = Path::new(&plan.esp);
    // The record names only the main image, so a snapshot with more kernels or the protected twin needs all of its images found or built.
    let extras = plan.anticheat
        || boot::kernels_in(&local_db_in(snapshot_root))
            .iter()
            .any(|(name, _)| *name != plan.kernel && !name.contains('+'));
    if let Some(name) = state.snapshots.get(&number)
        && !extras
        && esp.join(menu::IMAGES).join(name).is_file()
    {
        let version = boot::owned_version(&local_db_in(snapshot_root), &plan.kernel).unwrap_or_default();
        return Ok(menu::Images {
            main: Some(menu::Image {
                name: name.clone(),
                version,
            }),
            extras: Vec::new(),
        });
    }
    let on_esp = |images: &menu::Images| {
        images
            .main
            .iter()
            .chain(images.extras.iter().map(|(_, _, image)| image))
            .all(|image| esp.join(menu::IMAGES).join(&image.name).is_file())
    };
    if state.current.main.is_some()
        && on_esp(&state.current)
        && boot::limine_keep_refusal(replaced_root, snapshot_root).is_none()
    {
        return Ok(state.current);
    }
    eprintln!(":: building snapshot {number}'s boot image, which the ESP no longer has");
    let mounted: Vec<PathBuf> = ["proc", "sys", "dev"].iter().map(|dir| clone.join(dir)).collect();
    for (point, what) in mounted.iter().zip(["/proc", "/sys", "/dev"]) {
        let status = Command::new("mount")
            .args(["--rbind", what])
            .arg(point)
            .status()
            .context("running mount")?;
        if !status.success() {
            bail!("couldn't bind {what} into {}", point.display());
        }
    }
    let built = boot::build(paths, plan, &system::live::Root::new(clone));
    for point in mounted.iter().rev() {
        let _ = Command::new("umount").args(["-R", "-l"]).arg(point).status();
    }
    let images = built.context("building the snapshot's boot image")?;
    let all = images
        .main
        .iter()
        .chain(images.extras.iter().map(|(_, _, image)| image));
    for image in all {
        let target = esp.join(menu::IMAGES).join(&image.name);
        if !target.is_file() {
            archix::durable::copy(
                &clone.join(boot::BUILT.trim_start_matches('/')).join(&image.name),
                &target,
            )?;
        }
    }
    archix::durable::sync_esp(esp)?;
    Ok(images)
}

/// Package `name` `version` for `arch` as the snapshot had it: from archix's cache, pacman's, then the archive, downloaded into archix's cache.
fn package_file(
    paths: &Paths,
    lock: &std::collections::BTreeMap<String, archix::lock::Locked>,
    name: &str,
    version: &str,
    arch: &str,
) -> Result<PathBuf> {
    let locked = lock.get(name).filter(|pkg| pkg.version == version);
    let file = match locked {
        Some(pkg) => pkg.file.clone(),
        None => format!("{name}-{version}-{arch}.pkg.tar.zst"),
    };
    for dir in [paths.packages(), paths.pacman_cache.clone()] {
        if dir.join(&file).is_file() {
            return Ok(dir.join(file));
        }
    }
    let archive = archix::archive::for_system(&paths.state);
    let wanted = archix::fetch::Wanted {
        name: name.to_string(),
        file: file.clone(),
        urls: vec![archive.package_url(name, &file)],
        sha256: locked.map(|pkg| pkg.sha256.clone()).filter(|sha| !sha.is_empty()),
        size: None,
    };
    let fetched = archix::fetch::fetch_all(&archive, &paths.packages(), &[], &[wanted])?;
    fetched.into_iter().next().context("nothing fetched")
}

fn pending_path(root: &Path) -> PathBuf {
    root.join(archix::pacman::hooks::KEEP_PENDING.trim_start_matches('/'))
}

fn motd_path(root: &Path) -> PathBuf {
    root.join("run/motd.d/archix-keep.motd")
}

/// The snapshots a keep goes by; a try boot's root is an overlay, not btrfs, but its @archix is mounted as usual.
fn keep_store(root: &Path) -> Result<snapshot::Store> {
    if system::initrd::trying().is_none() {
        return store();
    }
    if !archix_mounted(root) {
        bail!(
            "{} isn't mounted in this try boot, so its snapshots can't be reached",
            root.join(snapshot::MOUNT).display()
        );
    }
    Ok(snapshot::Store::open(root))
}

/// What a pending keep's notice says.
pub fn pending_notice(number: u32) -> String {
    format!(
        "archix: rollback to snapshot {number} is pending: it becomes the root at the next boot. Package changes and /etc edits made before that reboot are lost; `sudo archix rollback --cancel` keeps the running system instead"
    )
}

/// `archix rollback N`.
pub fn keep(number: u32, noconfirm: bool) -> Result<()> {
    require_root("rollback")?;
    let paths = Paths::system();
    let root = paths.root.clone();
    if let Some(pending) = snapshot::keep_pending(&root) {
        bail!(
            "rollback to snapshot {pending} is pending already; reboot, or `archix rollback --cancel` first; nothing was changed"
        );
    }
    let store = keep_store(&root)?;
    if let Some(why) = snapshot::foreign_root(&root) {
        bail!("{why}; nothing was changed");
    }
    check_pacman_free(&paths)?;
    let record = store
        .get(number)?
        .with_context(|| format!("there's no snapshot {number}; `archix snapshot list` shows the ones there are"))?;
    let current = current_generation(&paths)?;
    let plan = current.as_ref().and_then(|g| g.settings.boot.clone());
    let snapshot_root = store.path(number);
    let declared = declared_users(&paths, &record)?;
    // The top level through @archix's mount, as a try boot's root is an overlay.
    let (uuid, _) = root_device(&root.join(snapshot::MOUNT))?;
    let top = Top::mount(&uuid)?;
    let replaced_root = top.dir.join(snapshot::ROOT);
    if let Some(plan) = &plan
        && plan.loader == boot::Loader::Limine
        && let Some(why) = boot::limine_keep_refusal(&replaced_root, &snapshot_root)
    {
        bail!("{why}; nothing was changed");
    }
    let trying = system::initrd::trying().is_some();
    let var_lib = top.dir.join(layout::VAR_LIB);
    let split = var_lib.is_dir()
        && (trying
            || layout::subvolume_of(&root.join("var/lib")).is_some_and(|path| path == format!("/{}", layout::VAR_LIB)));
    println!(
        "keeping snapshot {number}: {}",
        super::snapshot::describe(&record).trim_start()
    );
    println!(
        "from the next boot it is the root; the running system is kept as a snapshot of kind replaced. Package changes and /etc edits made before that reboot are lost"
    );
    if split {
        println!(
            "/var/lib stays: package-owned files there go back to the snapshot's versions where unchanged, and files of packages installed since move to /var/lib/{}/{number}",
            varlib::MOVED
        );
    }
    if !noconfirm && !confirm(&format!("keep snapshot {number}?"))? {
        bail!("aborted; nothing was changed");
    }
    let mut placeholders = Vec::new();
    let mut images = None;
    let previous = menu::load(&menu::dir(&paths))?;
    let replaced = store.keep(
        number,
        &top.dir,
        contents(current.as_ref(), format!("replaced by keeping snapshot {number}"), None),
        |clone| {
            let kept = carry_accounts(clone, &replaced_root, &declared)?;
            for skipped in &kept.skipped {
                eprintln!(
                    "warning: left out of the kept root's accounts, as its ID belongs to another name there: {skipped}"
                );
            }
            if let (Some(from), Some(to)) = (state_in(&replaced_root), state_in(clone)) {
                carry_generations(&from, &to, record.generation)?;
                archix::durable::write(
                    &to.join(PLACEHOLDERS),
                    serde_json::to_string_pretty(&kept.placeholders)?.as_bytes(),
                )?;
            }
            placeholders = kept.placeholders;
            if let Some(plan) = &plan
                && plan.loader != boot::Loader::Limine
            {
                images = Some(kept_images(
                    &paths,
                    plan,
                    number,
                    &replaced_root,
                    &snapshot_root,
                    clone,
                )?);
            }
            Ok(())
        },
    )?;
    match trying {
        true => println!("snapshot {number} is now @, from the next boot; the @ it replaces is snapshot {replaced}"),
        false => println!("snapshot {number} is now @, from the next boot; the running system is snapshot {replaced}"),
    }
    for name in &placeholders {
        println!(
            "{name} was deleted since snapshot {number}, so it comes back locked (no password, expired, nologin shell); `archix user unlock {name}` enables a declared one"
        );
    }
    let mut warnings = Vec::new();
    let mut moved = None;
    if split {
        let lock = record.packages.clone();
        let report = varlib::restore(
            &var_lib,
            &local_db_in(&snapshot_root),
            &local_db_in(&store.path(replaced)),
            number,
            &mut |name, version, arch| package_file(&paths, &lock, name, version, arch),
        );
        match report {
            Ok(report) => {
                if !report.restored.is_empty() {
                    println!(
                        "put back {} package-owned files under /var/lib as snapshot {number} has them",
                        report.restored.len()
                    );
                }
                if let Some(dir) = &report.dir
                    && !report.moved.is_empty()
                {
                    println!(
                        "moved {} files of packages installed since snapshot {number} to {}",
                        report.moved.len(),
                        dir.display()
                    );
                }
                warnings.extend(report.warnings);
                moved = report
                    .dir
                    .as_deref()
                    .and_then(|dir| dir.strip_prefix(&var_lib).ok())
                    .map(Path::to_path_buf);
            }
            Err(e) => warnings.push(format!("putting /var/lib in step with snapshot {number}: {e:#}")),
        }
    }
    archix::testhooks::mark("keep-var-lib");
    if let Some(plan) = &plan
        && let Some(images) = images
    {
        let dir = menu::dir(&paths);
        let updated = menu::load(&dir).and_then(|mut state| {
            if let Some(main) = &previous.current.main {
                state.snapshots.insert(replaced, main.name.clone());
            }
            state.current = images;
            menu::save(&dir, &state)?;
            let cap = current
                .as_ref()
                .and_then(|g| g.settings.rollback.as_ref())
                .map_or(0, |p| p.entries);
            menu::refresh(&paths, plan, Some(cap), true)
        });
        if let Err(e) = updated {
            warnings.push(format!(
                "the boot menu wasn't updated, so the current entry may start the replaced system's kernel: {e:#}"
            ));
        }
    }
    let done = Done {
        from: number,
        replaced,
        images: previous.current,
        moved,
    };
    archix::testhooks::mark("keep-menu");
    archix::durable::write(&store.dir().join(DONE), serde_json::to_string_pretty(&done)?.as_bytes())?;
    archix::durable::write(&pending_path(&root), number.to_string().as_bytes())?;
    if let Some(parent) = motd_path(&root).parent() {
        let _ = fs::create_dir_all(parent);
        let _ = fs::write(motd_path(&root), pending_notice(number) + "\n\n");
    }
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }
    println!("{}", pending_notice(number));
    Ok(())
}

/// `archix rollback --cancel`.
pub fn cancel() -> Result<()> {
    require_root("rollback --cancel")?;
    let paths = Paths::system();
    let root = paths.root.clone();
    let Some(number) = snapshot::keep_pending(&root) else {
        bail!("no rollback is pending; nothing was changed");
    };
    let store = keep_store(&root)?;
    let text = fs::read_to_string(store.dir().join(DONE))
        .with_context(|| format!("reading {}, which says what to undo", store.dir().join(DONE).display()))?;
    let done: Done = serde_json::from_str(&text).context("parsing what the keep did")?;
    let (uuid, _) = root_device(&root.join(snapshot::MOUNT))?;
    let top = Top::mount(&uuid)?;
    let replaced = done.replaced;
    store.cancel_keep(&top.dir, replaced)?;
    let warnings = undo_keep(&paths, &top, done);
    fs::remove_file(pending_path(&root)).context("removing the pending rollback's mark")?;
    let _ = fs::remove_file(motd_path(&root));
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }
    println!(
        "the rollback to snapshot {number} is cancelled: the running system stays the root, and snapshot {} is gone again",
        replaced
    );
    Ok(())
}

/// What's left of a cancel once its swap is done: /var/lib and the boot menu as they were before the keep; returns warnings.
fn undo_keep(paths: &Paths, top: &Top, done: Done) -> Vec<String> {
    let store = snapshot::Store::open(&paths.root);
    let mut warnings = Vec::new();
    let var_lib = top.dir.join(layout::VAR_LIB);
    if let Some(dir) = &done.moved
        && let Err(e) = varlib::undo(&var_lib, &var_lib.join(dir))
    {
        warnings.push(format!("putting /var/lib back as it was: {e:#}"));
    }
    if let Ok(Some(generation)) = current_generation(paths)
        && let Some(plan) = &generation.settings.boot
    {
        let dir = menu::dir(paths);
        let restored = menu::load(&dir).and_then(|mut state| {
            state.current = done.images;
            state.snapshots.remove(&done.replaced);
            menu::save(&dir, &state)?;
            let cap = generation.settings.rollback.as_ref().map_or(0, |p| p.entries);
            menu::refresh(paths, plan, Some(cap), false)
        });
        if let Err(e) = restored {
            warnings.push(format!("the boot menu wasn't put back: {e:#}"));
        }
    }
    let _ = fs::remove_file(store.dir().join(DONE));
    warnings
}

/// At boot, before anything reads the records: finishes or drops a keep a power cut stopped half way, and finishes or drops a `--cancel` likewise, so the records describe the root that booted; returns what to tell the user.
pub fn recover_after_cut(paths: &Paths) -> Result<Vec<String>> {
    if system::initrd::trying().is_some() || snapshot::keep_pending(&paths.root).is_some() {
        return Ok(Vec::new());
    }
    let store = snapshot::Store::open(&paths.root);
    let done: Option<Done> = fs::read_to_string(store.dir().join(DONE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    let cut = store.cut_keep();
    if cut.is_none() && done.is_none() {
        return Ok(Vec::new());
    }
    let (uuid, _) = root_device(&paths.root.join(snapshot::MOUNT))?;
    let top = Top::mount(&uuid)?;
    let mut said = Vec::new();
    if let Some(replaced) = cut {
        store.recover_keep(&top.dir)?;
        said.push(match store.get(replaced)?.is_some() {
            true => format!(
                "archix: a power cut stopped `archix rollback` after the swap, so it was finished at this boot: the root it replaced is snapshot {replaced}; package-owned files under /var/lib weren't put back to the kept snapshot's versions, and the boot menu may start the replaced root's kernel until the next switch"
            ),
            false => "archix: a power cut stopped `archix rollback` before the swap, so nothing changed and the running system stays the root".to_string(),
        });
    }
    if let Some(done) = done {
        match store.cut_cancel(&top.dir, done.replaced) {
            snapshot::CutCancel::Swapped => {
                let replaced = done.replaced;
                store.cancel_keep(&top.dir, replaced)?;
                said.extend(
                    undo_keep(paths, &top, done)
                        .into_iter()
                        .map(|w| format!("archix: warning: {w}")),
                );
                said.push(format!(
                    "archix: a power cut stopped `archix rollback --cancel` after its swap, so it was finished at this boot: snapshot {replaced} is gone again"
                ));
            }
            snapshot::CutCancel::NotSwapped => {
                store.drop_cancel_mark(&top.dir)?;
                said.push(format!(
                    "archix: a power cut stopped `archix rollback --cancel` before its swap, so the rollback stands: snapshot {} is the root, and the root it replaced is snapshot {}",
                    done.from, done.replaced
                ));
            }
            snapshot::CutCancel::None => {}
        }
    }
    Ok(said)
}

/// `archix user unlock NAME`: enables an account a keep brought back as a placeholder, when the current config declares it.
pub fn unlock(name: &str) -> Result<()> {
    require_root("user unlock")?;
    let paths = Paths::system();
    let etc = paths.root.join("etc");
    let passwd = fs::read_to_string(etc.join("passwd")).context("reading /etc/passwd")?;
    let shadow = fs::read_to_string(etc.join("shadow")).context("reading /etc/shadow")?;
    if !accounts::is_placeholder(&passwd, &shadow, name) {
        bail!("{name} isn't an account a rollback brought back locked; nothing was changed");
    }
    let generation = current_generation(&paths)?.context("there's no current generation, so no declared users")?;
    let Some(user) = generation.system.users.get(name) else {
        bail!(
            "{name} isn't declared in the current config, so it stays locked; declare it and switch, or remove it with userdel"
        );
    };
    let root = system::live::Root::new(&paths.root);
    let shell = user.shell.clone();
    root.run(&["usermod", "--expiredate", "", "--shell", &shell, name])?;
    let hashed = user.hashed_password.as_deref().filter(|hash| !hash.is_empty());
    match hashed {
        Some(hash) => {
            root.run(&["usermod", "--password", hash, name])?;
            println!("{name} is unlocked with its declared password and shell {shell}");
        }
        None => println!("{name} is unlocked with shell {shell}; it has no password yet: set one with `passwd {name}`"),
    }
    let state = paths.state.join(PLACEHOLDERS);
    if let Ok(text) = fs::read_to_string(&state)
        && let Ok(mut names) = serde_json::from_str::<Vec<String>>(&text)
    {
        names.retain(|other| other != name);
        let _ = archix::durable::write(&state, serde_json::to_string_pretty(&names)?.as_bytes());
    }
    Ok(())
}

/// Marks in archix's state that `archix status` noted snap-pac once.
const SNAP_PAC_NOTED: &str = "snap-pac-noted";

/// Once, while snap-pac is installed: that it also snapshots every pacman run, which archix leaves alone and ignores.
pub fn snap_pac_note(paths: &Paths) -> Option<String> {
    let installed = fs::read_dir(paths.local_db())
        .ok()?
        .flatten()
        .any(|entry| archix::pacman::name_from_entry(&entry.file_name().to_string_lossy()) == "snap-pac");
    let flag = paths.state.join(SNAP_PAC_NOTED);
    if !installed || flag.exists() {
        return None;
    }
    let _ = archix::durable::write(&flag, b"");
    Some(
        "note: snap-pac is installed, so snapper also snapshots every pacman run beside archix's own snapshots; archix leaves snapper's configs and snapshots alone and ignores them".to_string(),
    )
}

/// At boot: a keep's undo record goes once its reboot has happened, as /run's pending mark has.
pub fn after_boot(paths: &Paths) {
    if snapshot::keep_pending(&paths.root).is_none() {
        let _ = fs::remove_file(snapshot::Store::open(&paths.root).dir().join(DONE));
    }
}

/// After a boot that counted as good: finishes a move from Limine to systemd-boot that this boot came through.
pub fn limine_after_good_boot(paths: &Paths) {
    if system::initrd::trying().is_some() || boot::booted() != Some(menu::Entry::Current) {
        return;
    }
    let Ok(Some(generation)) = current_generation(paths) else {
        return;
    };
    let Some(plan) = &generation.settings.boot else { return };
    match system::limine::finish(paths, plan) {
        Ok(lines) => {
            for line in lines {
                println!("{line}");
            }
        }
        Err(e) => println!("archix: finishing the move from Limine to systemd-boot: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_notes_snap_pac_once_while_it_is_installed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        fs::create_dir_all(paths.local_db()).unwrap();
        fs::create_dir_all(&paths.state).unwrap();
        assert_eq!(snap_pac_note(&paths), None, "not installed");
        fs::create_dir_all(paths.local_db().join("snap-pac-3.0.1-1")).unwrap();
        assert!(snap_pac_note(&paths).is_some_and(|note| note.contains("snap-pac")));
        assert_eq!(snap_pac_note(&paths), None, "only once");
    }
}

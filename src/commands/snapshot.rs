use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use archix::date::Date;
use archix::generations::Generation;
use archix::paths::Paths;
use archix::snapshot::{self, Kind, Meta, Record, Store, layout};
use archix::system::mounts;

/// The btrfs top level of the root's filesystem, mounted under /run for as long as this lives.
pub(super) struct Top {
    pub(super) dir: PathBuf,
}

impl Top {
    /// Mounts the top level of the btrfs `root` is on, from the device the mount table names for it, with mount(2) alone.
    pub(super) fn mount(root: &Path) -> Result<Top> {
        let device = mounts::of(root)
            .filter(|mount| mount.fs_type == "btrfs")
            .with_context(|| format!("{} isn't on a mounted btrfs", root.display()))?
            .source;
        let dir = PathBuf::from(format!("/run/archix/top-{}", std::process::id()));
        mounts::mount(&device, &dir, "btrfs", "subvolid=5")
            .with_context(|| format!("mounting the top level of the root's btrfs ({device})"))?;
        Ok(Top { dir })
    }
}

impl Drop for Top {
    fn drop(&mut self) {
        let _ = mounts::unmount_under(&self.dir);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// The UUID and mount options of the btrfs `root` is on.
pub(super) fn root_device(root: &Path) -> Result<(String, String)> {
    let mount = mounts::of(root).with_context(|| format!("{} isn't on a mount /proc lists", root.display()))?;
    let uuid = archix::system::btrfs::filesystem_uuid(&mount.point)
        .with_context(|| format!("{} isn't on btrfs, so it has no top level to mount", root.display()))?;
    Ok((uuid, mount.options))
}

/// Whether @archix is mounted at `root`/.archix, as setup's fstab line does from the boot after setup on.
pub(super) fn archix_mounted(root: &Path) -> bool {
    layout::subvolume_of(&root.join(snapshot::MOUNT)).is_some_and(|path| path == format!("/{}", layout::ARCHIX))
}

pub(super) fn store() -> Result<Store> {
    let root = archix::testhooks::root();
    if let Some(why) = snapshot::unsupported(&root) {
        bail!("{why}");
    }
    if !archix_mounted(&root) {
        bail!(
            "{} isn't mounted, so a snapshot's record wouldn't survive a rollback; run `sudo archix snapshot setup` and reboot",
            root.join(snapshot::MOUNT).display()
        );
    }
    Ok(Store::open(&root))
}

/// What a snapshot of the system as `current` left it records: that generation's tree, commit and lock, all none before the first switch.
pub(super) fn contents(current: Option<&Generation>, label: String, note: Option<String>) -> Meta {
    Meta {
        tree: current.and_then(|g| g.applied.as_ref()).map(|a| a.tree.clone()),
        commit: current.and_then(|g| g.applied.as_ref()).map(|a| a.commit.clone()),
        generation: current.map(|g| g.number),
        packages: current.map(|g| g.packages.clone()).unwrap_or_default(),
        label,
        note,
    }
}

pub(super) fn current_generation(paths: &Paths) -> Result<Option<Generation>> {
    let store = archix::generations::Store::new(paths);
    store.current()?.map(|number| store.get(number)).transpose()
}

pub fn create(note: Option<String>) -> Result<()> {
    let store = store()?;
    let paths = Paths::system();
    let current = current_generation(&paths)?;
    let record = store.create(
        &paths.root,
        Kind::Manual,
        contents(current.as_ref(), "taken by hand".into(), note),
    )?;
    println!(
        "snapshot {} taken: {}",
        record.number,
        store.path(record.number).display()
    );
    Ok(())
}

pub fn list() -> Result<()> {
    let store = Store::open(&archix::testhooks::root());
    let records = store.list()?;
    if records.is_empty() {
        println!("no snapshots yet");
    }
    for record in records {
        println!("{}", describe(&record));
    }
    Ok(())
}

/// One snapshot by the state it holds: "N  date  kind  system as of tree Y (generation G), before switch to tree X".
pub(super) fn describe(record: &Record) -> String {
    let minutes = record.created % 86_400 / 60;
    let state = match (&record.tree, record.generation) {
        (Some(tree), Some(generation)) => format!(
            "system as of tree {} (generation {generation})",
            &tree[..tree.len().min(12)]
        ),
        (None, Some(generation)) => format!("system as of generation {generation}"),
        (Some(tree), None) => format!("system as of tree {}", &tree[..tree.len().min(12)]),
        (None, None) => "system before archix's first switch".to_string(),
    };
    let label = match record.label.is_empty() {
        true => String::new(),
        false => format!(", {}", record.label),
    };
    let note = record
        .note
        .as_deref()
        .map(|note| format!("  ({note})"))
        .unwrap_or_default();
    format!(
        "{:>4}  {} {:02}:{:02} UTC  {:<8}  {state}{label}{note}",
        record.number,
        Date::from_unix(record.created),
        minutes / 60,
        minutes % 60,
        record.kind.name(),
    )
}

pub fn delete(number: u32) -> Result<()> {
    let store = store()?;
    if store.get(number)?.is_none() {
        bail!("there's no snapshot {number}");
    }
    store.delete(number)?;
    if let Err(e) = store.settle() {
        eprintln!("warning: snapshot {number} is deleted, but btrfs may not have freed its space yet: {e:#}");
    }
    println!("snapshot {number} deleted");
    Ok(())
}

pub fn setup(move_root: bool) -> Result<()> {
    let root = archix::testhooks::root();
    if move_root {
        return schedule_root_move(&root);
    }
    if let Some(why) = snapshot::unsupported(&root) {
        bail!("{why}");
    }
    let (uuid, options) = root_device(&root)?;
    let top = Top::mount(&root)?;
    match layout::setup(&root, &top.dir, &uuid, &options)? {
        layout::Setup::Done => println!("the root is laid out for rollbacks"),
        layout::Setup::AtNextBoot => println!(
            "the root is laid out for rollbacks once you reboot: the next boot moves /var/lib into its own subvolume before any service starts, so services' data stays put when you roll back"
        ),
    }
    Ok(())
}

/// Schedules the move of a root that is btrfs's top level into `@`, which the initramfs makes at the next boot.
pub(super) fn schedule_root_move(root: &Path) -> Result<()> {
    if !layout::subvolume_of(root).is_some_and(|path| path == "/") {
        bail!(
            "{} isn't btrfs's top level, so there's nothing to move; nothing was changed",
            root.display()
        );
    }
    if root.exists() {
        bail!("the move happened in archix's initramfs hook, which is gone with its boot management; nothing was changed");
    }
    snapshot::rootmove::schedule(root)?;
    println!(
        "the root moves into an @ subvolume at the next boot, in the initramfs: don't power off while it says so. Other subvolumes at the top level stay as they are, and get fstab lines to stay mounted where they are now, which the next switch asks you to declare in file_systems; after that boot, the next switch lays the root out for rollbacks"
    );
    Ok(())
}

/// Run at boot by archix-split-var-lib.service, before /var/lib is mounted.
pub fn split_var_lib() -> Result<()> {
    if !layout::split_is_due(&std::fs::read_to_string("/etc/fstab").unwrap_or_default()) {
        println!(
            "archix: /etc/fstab doesn't mount /var/lib yet, so it stays where it is until a switch finishes laying the root out"
        );
        return Ok(());
    }
    let top = Top::mount(Path::new("/"))?;
    layout::split_var_lib(Path::new("/"), &top.dir.join(layout::VAR_LIB))
}

/// Mounts the root on `device` with its layout, binds and ESP, runs arch-chroot there with `command`, and unmounts it all again.
pub fn chroot(device: &str, esp: Option<&str>, command: &[String]) -> Result<()> {
    let target = PathBuf::from(format!("/run/archix/chroot-{}", std::process::id()));
    let mounted = layout::mount_all(device, &target).and_then(|()| layout::mount_esp(&target, esp).map(|_| ()));
    let ran = mounted.and_then(|()| {
        let status = Command::new("arch-chroot")
            .arg(&target)
            .args(command)
            .status()
            .context("running arch-chroot (in arch-install-scripts)")?;
        if !status.success() {
            bail!("arch-chroot exited with {status}");
        }
        Ok(())
    });
    let _ = mounts::unmount_under(&target);
    let _ = std::fs::remove_dir(&target);
    ran
}

/// The subvolume path, from the top level, whose UUID is `uuid` in `btrfs subvolume list -u`'s output.
fn listed_path(listing: &str, uuid: &str) -> Option<String> {
    listing.lines().find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let at = fields.iter().position(|field| *field == "uuid")?;
        if fields.get(at + 1) != Some(&uuid) {
            return None;
        }
        fields.get(at + 3).map(|path| path.to_string())
    })
}

/// The subvolume path, from the top level, whose UUID is `uuid`, among those under the mounted top level `top`.
fn subvolume_by_uuid(top: &Path, uuid: &str) -> Option<String> {
    let output = Command::new("btrfs")
        .args(["subvolume", "list", "-u"])
        .arg(top)
        .output()
        .ok()?;
    listed_path(&String::from_utf8_lossy(&output.stdout), uuid)
}

/// `archix resync`: adopts a root another tool put in `@`'s place: lays it out for rollbacks again if it isn't, carries accounts over from the root it replaced when that root can still be found by its recorded UUID, and records the new root.
pub fn resync() -> Result<()> {
    let root = archix::testhooks::root();
    let Some(why) = snapshot::foreign_root(&root) else {
        println!("the root is the one archix last produced; nothing to resync");
        return Ok(());
    };
    println!("{why}");
    // Read first: setting the layout up again records the new root.
    let recorded = snapshot::recorded_root(&root.join(snapshot::MOUNT)).unwrap_or_default();
    let (uuid, options) = root_device(&root)?;
    let top = Top::mount(&root)?;
    if !archix_mounted(&root) || !root.join(layout::ON_ROOT).is_dir() {
        match layout::setup(&root, &top.dir, &uuid, &options)? {
            layout::Setup::Done => println!("laid the root out for rollbacks again"),
            layout::Setup::AtNextBoot => {
                println!("laid the root out for rollbacks again; /var/lib moves at the next boot")
            }
        }
    }
    match subvolume_by_uuid(&top.dir, &recorded) {
        Some(path) => {
            let old = top.dir.join(path.trim_start_matches('/'));
            let read = |etc: &Path| -> archix::system::accounts::Texts {
                archix::system::accounts::FILES.map(|file| std::fs::read_to_string(etc.join(file)).ok())
            };
            let carried = archix::system::accounts::carry(&read(&old.join("etc")), &read(&root.join("etc")));
            for (file, text) in archix::system::accounts::FILES.iter().zip(&carried.texts) {
                if let Some(text) = text {
                    let path = root.join("etc").join(file);
                    archix::durable::write_config(&path, text.as_bytes())?;
                }
            }
            println!("carried users, groups and passwords over from the root it replaced ({path})");
            if !carried.skipped.is_empty() {
                println!(
                    "left out, as their IDs belong to other names here: {}",
                    carried.skipped.join(", ")
                );
            }
        }
        None => println!(
            "the root it replaced ({recorded}) isn't among the subvolumes any more (another tool may have moved or deleted it), so no accounts were carried over; check users and passwords"
        ),
    }
    snapshot::record_root(&root.join(snapshot::MOUNT), &top.dir)?;
    println!("recorded the root; switches and keeps work again");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_root_a_foreign_one_replaced_is_found_by_its_uuid() {
        let listing = "ID 256 gen 10 top level 5 uuid 1c0a-aa path @\nID 300 gen 12 top level 5 uuid 77-bb path @snapshots/4/snapshot\n";
        assert_eq!(
            super::listed_path(listing, "77-bb").as_deref(),
            Some("@snapshots/4/snapshot")
        );
        assert_eq!(super::listed_path(listing, "1c0a-aa").as_deref(), Some("@"));
        assert_eq!(super::listed_path(listing, "gone"), None);
    }
}

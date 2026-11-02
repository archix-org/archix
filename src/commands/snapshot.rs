use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use archix::date::Date;
use archix::snapshot::{self, Kind, Meta, Store, layout};

/// The btrfs top level of the root's filesystem, mounted under /run for as long as this lives.
struct Top {
    dir: PathBuf,
}

impl Top {
    fn mount(uuid: &str) -> Result<Top> {
        let dir = PathBuf::from("/run/archix/top");
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let status = Command::new("mount")
            .args(["-o", "subvolid=5"])
            .arg(format!("UUID={uuid}"))
            .arg(&dir)
            .status()
            .context("running mount")?;
        if !status.success() {
            bail!(
                "couldn't mount the top level of the root's btrfs ({uuid}) at {}",
                dir.display()
            );
        }
        Ok(Top { dir })
    }
}

impl Drop for Top {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg(&self.dir).status();
    }
}

/// The root filesystem's UUID and mount options.
fn root_device() -> Result<(String, String)> {
    let output = Command::new("findmnt")
        .args(["-n", "-o", "UUID,OPTIONS", "/"])
        .output()
        .context("running findmnt")?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut fields = text.split_whitespace();
    match (fields.next(), fields.next()) {
        (Some(uuid), Some(options)) if output.status.success() => Ok((uuid.to_string(), options.to_string())),
        _ => bail!("findmnt couldn't say which filesystem / is"),
    }
}

fn store() -> Result<Store> {
    let root = archix::testhooks::root();
    if let Some(why) = snapshot::unsupported(&root) {
        bail!("{why}");
    }
    if !layout::subvolume_of(&root.join(snapshot::MOUNT)).is_some_and(|path| path == format!("/{}", layout::ARCHIX)) {
        bail!(
            "{} isn't mounted, so a snapshot's record wouldn't survive a rollback; run `sudo archix snapshot setup` and reboot",
            root.join(snapshot::MOUNT).display()
        );
    }
    Ok(Store::open(&root))
}

pub fn create(note: Option<String>) -> Result<()> {
    let store = store()?;
    // The applied tree and lock come with the in-place switch, which records them.
    let record = store.create(
        Path::new("/"),
        Kind::Manual,
        Meta {
            label: "taken by hand".into(),
            note,
            ..Meta::default()
        },
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
        let minutes = record.created % 86_400 / 60;
        let tree = match &record.tree {
            Some(hash) => format!("  tree {}", &hash[..hash.len().min(10)]),
            None => String::new(),
        };
        let note = record
            .note
            .as_deref()
            .map(|note| format!("  ({note})"))
            .unwrap_or_default();
        println!(
            "{:>4}  {} {:02}:{:02} UTC  {:<8}  {}{tree}{note}",
            record.number,
            Date::from_unix(record.created),
            minutes / 60,
            minutes % 60,
            record.kind.name(),
            record.label
        );
    }
    Ok(())
}

pub fn delete(number: u32) -> Result<()> {
    let store = store()?;
    if store.get(number)?.is_none() {
        bail!("there's no snapshot {number}");
    }
    store.delete(number)?;
    println!("snapshot {number} deleted");
    Ok(())
}

pub fn setup() -> Result<()> {
    let (uuid, options) = root_device()?;
    let top = Top::mount(&uuid)?;
    match layout::setup(Path::new("/"), &top.dir, &uuid, &options)? {
        layout::Setup::Done => println!("the root is laid out for rollbacks"),
        layout::Setup::AtNextBoot => println!(
            "the root is laid out for rollbacks once you reboot: the next boot moves /var/lib into its own subvolume before any service starts, so services' data stays put when you roll back"
        ),
    }
    Ok(())
}

/// Run at boot by archix-split-var-lib.service, before /var/lib is mounted.
pub fn split_var_lib() -> Result<()> {
    let (uuid, _) = root_device()?;
    let top = Top::mount(&uuid)?;
    layout::split_var_lib(Path::new("/"), &top.dir.join(layout::VAR_LIB))
}

/// Mounts the root on `device` with its layout and binds, runs arch-chroot there with `command`, and unmounts it all again.
pub fn chroot(device: &str, command: &[String]) -> Result<()> {
    let target = PathBuf::from(format!("/run/archix/chroot-{}", std::process::id()));
    let mounted = layout::mount_all(device, &target);
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
    let _ = Command::new("umount").arg("-R").arg(&target).status();
    let _ = std::fs::remove_dir(&target);
    ran
}

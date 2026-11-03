use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use archix::date::Date;
use archix::generations::Generation;
use archix::paths::Paths;
use archix::snapshot::{self, Kind, Meta, Policy, Record, Store, layout};

/// The btrfs top level of the root's filesystem, mounted under /run for as long as this lives.
struct Top {
    dir: PathBuf,
}

impl Top {
    fn mount(uuid: &str) -> Result<Top> {
        let dir = PathBuf::from(format!("/run/archix/top-{}", std::process::id()));
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
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// The UUID and mount options of the filesystem `root` is on.
fn root_device(root: &Path) -> Result<(String, String)> {
    let output = Command::new("findmnt")
        .args(["-n", "-o", "UUID,OPTIONS", "-T"])
        .arg(root)
        .output()
        .context("running findmnt")?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut fields = text.split_whitespace();
    match (fields.next(), fields.next()) {
        (Some(uuid), Some(options)) if output.status.success() => Ok((uuid.to_string(), options.to_string())),
        _ => bail!("findmnt couldn't say which filesystem {} is", root.display()),
    }
}

/// Whether @archix is mounted at `root`/.archix, as setup's fstab line does from the boot after setup on.
fn archix_mounted(root: &Path) -> bool {
    layout::subvolume_of(&root.join(snapshot::MOUNT)).is_some_and(|path| path == format!("/{}", layout::ARCHIX))
}

fn store() -> Result<Store> {
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
fn contents(current: Option<&Generation>, label: String, note: Option<String>) -> Meta {
    Meta {
        tree: current.and_then(|g| g.applied.as_ref()).map(|a| a.tree.clone()),
        commit: current.and_then(|g| g.applied.as_ref()).map(|a| a.commit.clone()),
        generation: current.map(|g| g.number),
        packages: current.map(|g| g.packages.clone()).unwrap_or_default(),
        label,
        note,
    }
}

fn current_generation(paths: &Paths) -> Result<Option<Generation>> {
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

/// What a switch or rollback does about snapshots, decided before its prompt.
pub(super) enum Rollbacks {
    /// Rollbacks off, or a root that can't have them.
    Off,
    /// Rollbacks on, but the root isn't laid out for them yet: this switch sets it up and takes no snapshot.
    Setup,
    /// A snapshot before the change, pruned after it by the policy.
    On(Policy),
}

impl Rollbacks {
    pub(super) fn of(root: &Path, policy: Option<Policy>) -> Rollbacks {
        let Some(policy) = policy else { return Rollbacks::Off };
        if let Some(why) = snapshot::unsupported(root) {
            eprintln!("warning: rollback.enable is set, but {why}");
            return Rollbacks::Off;
        }
        match archix_mounted(root) {
            true => Rollbacks::On(policy),
            false => Rollbacks::Setup,
        }
    }

    /// The preview line, if any.
    pub(super) fn line(&self) -> Option<&'static str> {
        match self {
            Rollbacks::Setup => Some(
                "set up     rollbacks: the @archix and @varlib subvolumes and their fstab lines; snapshots start after the next reboot",
            ),
            _ => None,
        }
    }

    /// After the prompt, before anything is applied: sets the root up, or takes the snapshot of the system as `current` left it, labelled with what comes next.
    pub(super) fn before(&self, root: &Path, current: Option<&Generation>, label: String) -> Result<Option<Record>> {
        match self {
            Rollbacks::Off => Ok(None),
            Rollbacks::Setup => {
                let (uuid, options) = root_device(root)?;
                let top = Top::mount(&uuid)?;
                layout::setup(root, &top.dir, &uuid, &options).context("setting up rollbacks")?;
                println!(
                    "the root is set up for rollbacks; snapshots start after the next reboot, once @archix is mounted"
                );
                Ok(None)
            }
            Rollbacks::On(_) => {
                let record = Store::open(root)
                    .create(root, Kind::Switch, contents(current, label, None))
                    .context("taking the snapshot before the switch")?;
                println!(
                    "snapshot {} holds the system as it was before this change",
                    record.number
                );
                Ok(Some(record))
            }
        }
    }

    /// After the apply: prunes by the policy, then drops refetchable packages from archix's cache if the root is still short; returns warnings.
    pub(super) fn after(&self, paths: &Paths, current: &BTreeMap<String, archix::lock::Locked>) -> Vec<String> {
        let Rollbacks::On(policy) = self else { return Vec::new() };
        let pruned = snapshot::prune_root(paths, policy, current, || archix::system::btrfs::space(&paths.root));
        if !pruned.deleted.is_empty() {
            let numbers: Vec<String> = pruned.deleted.iter().map(u32::to_string).collect();
            println!("deleted snapshots {}", numbers.join(", "));
        }
        if pruned.dropped > 0 {
            println!(
                "dropped {} MiB of cached packages the archive can serve again",
                pruned.dropped >> 20
            );
        }
        pruned.warnings
    }
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
    println!("snapshot {number} deleted");
    Ok(())
}

pub fn setup() -> Result<()> {
    let root = archix::testhooks::root();
    let (uuid, options) = root_device(&root)?;
    let top = Top::mount(&uuid)?;
    match layout::setup(&root, &top.dir, &uuid, &options)? {
        layout::Setup::Done => println!("the root is laid out for rollbacks"),
        layout::Setup::AtNextBoot => println!(
            "the root is laid out for rollbacks once you reboot: the next boot moves /var/lib into its own subvolume before any service starts, so services' data stays put when you roll back"
        ),
    }
    Ok(())
}

/// Run at boot by archix-split-var-lib.service, before /var/lib is mounted.
pub fn split_var_lib() -> Result<()> {
    let (uuid, _) = root_device(Path::new("/"))?;
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

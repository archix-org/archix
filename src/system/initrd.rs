//! The initramfs side of the menu: maps the entry systemd-boot started to what it boots through archix's records, and boots an older snapshot read-only in try mode, with a throwaway layer over the root and /var/lib.

use std::ffi::CString;
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::menu::{self, Entry};
use crate::snapshot::layout;

/// The kernel command line argument GRUB's entries name a snapshot with, as GRUB sets no LoaderEntrySelected.
pub const SNAPSHOT_ARG: &str = "archix.snapshot";
/// What the initramfs leaves for the booted system, under /run, which switch_root carries over.
pub const RUN: &str = "/run/archix";
/// The try-mode record in RUN: present only on a try boot.
pub const TRY: &str = "try";
/// The size the throwaway layer's file starts at, then the smaller ones tried on a fuller disk, before tmpfs.
pub const LAYER_SIZES: [u64; 3] = [4 << 30, 1 << 30, 256 << 20];
const LAYER_FILE: &str = "layer.img";

/// What this boot boots.
#[derive(Debug, Clone, PartialEq)]
pub enum Choice {
    Current,
    /// A snapshot, read-only with a throwaway layer, and why it isn't the one the entry named, if it isn't.
    Try {
        snapshot: u32,
        why: Option<String>,
    },
    /// Nothing archix may boot: the entry's snapshot is gone and no other has an entry that hasn't failed.
    Rescue {
        why: String,
    },
}

/// What a try boot records in RUN/try for the system to show.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trying {
    pub snapshot: u32,
    #[serde(default)]
    pub why: Option<String>,
    /// "file" for a loop file in @archix/try, "tmpfs" when none could be allocated.
    pub layer: String,
    pub size: u64,
}

/// The try record of this boot, if it's one.
pub fn trying() -> Option<Trying> {
    serde_json::from_str(&fs::read_to_string(crate::testhooks::run_dir().join(TRY)).ok()?).ok()
}

/// The try boot's notice, for the login message, the motd, `archix status` and the desktop.
pub fn notice(trying: &Trying) -> String {
    let n = trying.snapshot;
    let mut text = format!(
        "archix: this boot is snapshot {n}, read-only with a throwaway layer (try mode): changes are lost at the next boot, even if you keep it. `sudo archix rollback {n}` keeps it from the next boot on; a plain reboot goes back to the current system's entry."
    );
    if let Some(why) = &trying.why {
        text.push_str(&format!(" Why: {why}."));
    }
    match trying.layer.as_str() {
        "file" => text.push_str(&format!(
            " The layer holds {} MiB of changes; past that, writes fail with \"no space left on device\".",
            trying.size >> 20
        )),
        layer => text.push_str(&format!(
            " The disk had no room for the layer's file, so it's {} in memory ({} MiB); past that, writes fail with \"no space left on device\".",
            layer,
            trying.size >> 20
        )),
    }
    text
}

fn snapshot_exists(store: &Path, number: u32) -> bool {
    let dir = store.join("snapshots").join(number.to_string());
    dir.join("record.json").is_file() && dir.join("root").is_dir()
}

/// The snapshot the boot asked for: the selected entry's when archix recorded writing that entry, else GRUB's command line argument.
fn requested(store: &Path, selected: Option<&str>, cmdline: &str) -> Option<u32> {
    let boot = store.join("boot");
    if let Some(selected) = selected {
        return match Entry::parse(selected) {
            Some(Entry::Snapshot(number)) if menu::record_path(&boot, selected).is_file() => Some(number),
            _ => None,
        };
    }
    cmdline
        .split_whitespace()
        .find_map(|arg| arg.strip_prefix(SNAPSHOT_ARG)?.strip_prefix('=')?.parse().ok())
        .filter(|number| menu::record_path(&boot, &Entry::Snapshot(*number).id()).is_file())
}

/// What to boot, from @archix mounted at `store`: the snapshot the entry names when it's there; when it's gone, the newest surviving snapshot with an entry that hasn't failed, else the rescue entry; otherwise the current system.
pub fn choose(store: &Path, selected: Option<&str>, cmdline: &str) -> Choice {
    let Some(number) = requested(store, selected, cmdline) else {
        return Choice::Current;
    };
    if snapshot_exists(store, number) {
        return Choice::Try {
            snapshot: number,
            why: None,
        };
    }
    let failed = menu::load(&store.join("boot"))
        .map(|state| state.failed)
        .unwrap_or_default();
    let mut numbers: Vec<u32> = fs::read_dir(store.join("snapshots"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
        .filter(|n| snapshot_exists(store, *n) && !failed.contains(n))
        .filter(|n| menu::record_path(&store.join("boot"), &Entry::Snapshot(*n).id()).is_file())
        .collect();
    numbers.sort_unstable();
    match numbers.last() {
        Some(newest) => Choice::Try {
            snapshot: *newest,
            why: Some(format!(
                "snapshot {number}, which the boot entry named, is gone, so the newest snapshot with an entry that hasn't failed boots instead"
            )),
        },
        None => Choice::Rescue {
            why: format!(
                "snapshot {number}, which the boot entry named, is gone, and no other snapshot has an entry that hasn't failed"
            ),
        },
    }
}

fn c_path(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).with_context(|| format!("{} has a NUL byte", path.display()))
}

fn sys_mount(source: &str, target: &Path, fs_type: &str, flags: libc::c_ulong, data: &str) -> Result<()> {
    let (source_c, target_c, type_c, data_c) = (
        CString::new(source)?,
        c_path(target)?,
        CString::new(fs_type)?,
        CString::new(data)?,
    );
    let done = unsafe {
        libc::mount(
            source_c.as_ptr(),
            target_c.as_ptr(),
            type_c.as_ptr(),
            flags,
            data_c.as_ptr().cast(),
        )
    };
    if done != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("mounting {source} ({fs_type}, {data}) on {}", target.display()));
    }
    Ok(())
}

fn umount(target: &Path) {
    if let Ok(target) = c_path(target) {
        unsafe { libc::umount2(target.as_ptr(), libc::MNT_DETACH) };
    }
}

/// The device and filesystem type of the mount at `point`, the topmost one, from /proc/self/mountinfo.
fn mounted_at(point: &Path) -> Option<(String, String)> {
    let info = fs::read_to_string("/proc/self/mountinfo").ok()?;
    info.lines().rev().find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let dash = fields.iter().position(|field| *field == "-")?;
        if Path::new(fields.get(4)?) != point {
            return None;
        }
        Some((fields.get(dash + 2)?.to_string(), fields.get(dash + 1)?.to_string()))
    })
}

const FS_IOC_GETFLAGS: libc::c_ulong = 0x8008_6601;
const FS_IOC_SETFLAGS: libc::c_ulong = 0x4008_6602;
const FS_NOCOW_FL: libc::c_long = 0x0080_0000;

/// Turns copy-on-write off for files created in `dir` from now on, as `chattr +C` does.
fn no_cow(dir: &Path) -> Result<()> {
    let file = fs::File::open(dir).with_context(|| format!("opening {}", dir.display()))?;
    let mut flags: libc::c_long = 0;
    if unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_GETFLAGS as _, &mut flags) } != 0 {
        return Err(std::io::Error::last_os_error()).context("reading the directory's flags");
    }
    flags |= FS_NOCOW_FL;
    if unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_SETFLAGS as _, &flags) } != 0 {
        return Err(std::io::Error::last_os_error()).context("turning copy-on-write off");
    }
    Ok(())
}

const LOOP_CTL_GET_FREE: libc::c_ulong = 0x4C82;
const LOOP_SET_FD: libc::c_ulong = 0x4C00;
const LOOP_SET_STATUS64: libc::c_ulong = 0x4C04;
/// struct loop_info64's size, and where its lo_flags are.
const LOOP_INFO64: usize = 232;
const LO_FLAGS_OFFSET: usize = 52;
const LO_FLAGS_AUTOCLEAR: u32 = 4;

/// Attaches `file` to a free loop device, returning it and the handle that keeps it attached: with autoclear, the device goes at its last close, so the caller holds the handle until the mount has its own.
fn attach_loop(file: &Path) -> Result<(PathBuf, fs::File)> {
    let control = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/loop-control")
        .context("opening /dev/loop-control")?;
    let number = unsafe { libc::ioctl(control.as_raw_fd(), LOOP_CTL_GET_FREE as _) };
    if number < 0 {
        return Err(std::io::Error::last_os_error()).context("finding a free loop device");
    }
    let device = PathBuf::from(format!("/dev/loop{number}"));
    let backing = fs::OpenOptions::new().read(true).write(true).open(file)?;
    let loop_device = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&device)
        .with_context(|| format!("opening {}", device.display()))?;
    if unsafe { libc::ioctl(loop_device.as_raw_fd(), LOOP_SET_FD as _, backing.as_raw_fd()) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("attaching {}", device.display()));
    }
    // Detached by the kernel once its last user, the mount, goes.
    let mut info = [0u8; LOOP_INFO64];
    info[LO_FLAGS_OFFSET..LO_FLAGS_OFFSET + 4].copy_from_slice(&LO_FLAGS_AUTOCLEAR.to_ne_bytes());
    if unsafe { libc::ioctl(loop_device.as_raw_fd(), LOOP_SET_STATUS64 as _, info.as_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("setting up {}", device.display()));
    }
    Ok((device, loop_device))
}

/// Room a layer file needs beyond its own size, for the filesystem's metadata (btrfs writes into preallocated extents need it) and the root's own writes: a sixteenth of it, at least 64 MiB.
pub fn layer_margin(size: u64) -> u64 {
    (size / 16).max(64 << 20)
}

fn free_bytes(dir: &Path) -> Result<u64> {
    let path = c_path(dir)?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("reading the free space of {}", dir.display()));
    }
    Ok(stat.f_bavail as u64 * stat.f_frsize as u64)
}

/// ext4's superblock magic, at byte 0x38 of the superblock, which starts 1024 bytes in.
const EXT4_MAGIC_AT: u64 = 1024 + 0x38;

/// Whether `path` reads back an ext4 superblock, as the mount will need.
fn reads_back(path: &Path) -> Result<()> {
    use std::os::unix::fs::FileExt;
    let handle = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut magic = [0u8; 2];
    handle
        .read_exact_at(&mut magic, EXT4_MAGIC_AT)
        .with_context(|| format!("reading {}'s superblock", path.display()))?;
    if magic != [0x53, 0xef] {
        bail!("{} holds no ext4 superblock", path.display());
    }
    Ok(())
}

/// Makes the throwaway layer at `at`: a preallocated NOCOW file in `dir` (@archix/try) formatted ext4 and loop mounted, trying `sizes` largest first, else tmpfs; returns what it made and its size.
pub fn make_layer(dir: &Path, at: &Path, sizes: &[u64]) -> (String, u64) {
    let (kind, size, notes) = make_layer_with(dir, at, sizes, &|_, _| {});
    for note in notes {
        println!("archix: {note}");
    }
    (kind, size)
}

/// make_layer, with what it couldn't do as notes; `tamper` sees each formatted file before it's checked, for tests.
pub fn make_layer_with(
    dir: &Path,
    at: &Path,
    sizes: &[u64],
    tamper: &dyn Fn(&Path, u64),
) -> (String, u64, Vec<String>) {
    let mut notes = Vec::new();
    let file = dir.join(LAYER_FILE);
    let _ = fs::remove_file(&file);
    let _ = fs::create_dir_all(dir);
    let _ = no_cow(dir);
    for size in sizes {
        let made = (|| -> Result<()> {
            let free = free_bytes(dir)?;
            let needed = size + layer_margin(*size);
            if free < needed {
                bail!(
                    "only {} MiB free, under the {} MiB it needs with room for the filesystem's own writes",
                    free >> 20,
                    needed >> 20
                );
            }
            let handle = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&file)
                .with_context(|| format!("creating {}", file.display()))?;
            if unsafe { libc::fallocate(handle.as_raw_fd(), 0, 0, *size as libc::off_t) } != 0 {
                return Err(std::io::Error::last_os_error()).context("allocating the layer");
            }
            drop(handle);
            let status = Command::new("mkfs.ext4")
                .args(["-q", "-F", "-O", "^has_journal", "-E", "nodiscard"])
                .arg(&file)
                .status()
                .context("running mkfs.ext4")?;
            if !status.success() {
                bail!("mkfs.ext4 failed");
            }
            tamper(&file, *size);
            // A write into the preallocated file that the full disk refused shows only when it's flushed, or as EIO at the mount.
            fs::File::open(&file)
                .and_then(|handle| handle.sync_all())
                .context("flushing the formatted layer file")?;
            reads_back(&file).context("the formatted layer file didn't read back")?;
            let (device, _attached) = attach_loop(&file)?;
            reads_back(&device).context("the layer's loop device didn't read back")?;
            fs::create_dir_all(at)?;
            sys_mount(&device.to_string_lossy(), at, "ext4", 0, "")
        })();
        match made {
            Ok(()) => return ("file".to_string(), *size, notes),
            Err(e) => {
                notes.push(format!("no {} MiB layer file: {e:#}", size >> 20));
                let _ = fs::remove_file(&file);
            }
        }
    }
    let _ = fs::create_dir_all(at);
    let size = sizes.last().copied().unwrap_or(256 << 20);
    match sys_mount("archix-try", at, "tmpfs", 0, &format!("size={size},mode=0755")) {
        Ok(()) => ("tmpfs".to_string(), size, notes),
        Err(e) => {
            notes.push(format!(
                "no tmpfs layer either: {e:#}; the layer is the initramfs's own memory"
            ));
            ("memory".to_string(), 0, notes)
        }
    }
}

/// The root's fstab for a try boot: no line for `/`, which is the overlay now, none for swap files on it, which swapon can't use on an overlay and the snapshot doesn't hold (their subvolume isn't in it), and /var/lib as an overlay of @varlib, mounted read-only at `varlib`, with its upper half in the layer.
pub fn try_fstab(fstab: &str, varlib: Option<&Path>, layer: &Path) -> String {
    let entries = || {
        fstab
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .map(|line| line.split_whitespace().collect::<Vec<_>>())
    };
    let elsewhere: Vec<&str> = entries()
        .filter(|fields| fields.len() > 2 && fields[2] != "swap" && !matches!(fields[1], "/" | "/var/lib"))
        .map(|fields| fields[1])
        .collect();
    let on_root = |file: &str| {
        file.starts_with('/')
            && !file.starts_with("/dev/")
            && !elsewhere.iter().any(|point| Path::new(file).starts_with(point))
    };
    let mut text = String::new();
    for line in fstab.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let point = fields.get(1).copied();
        let comment = line.trim_start().starts_with('#');
        match point {
            Some("/") if !comment => text.push_str(&format!("# archix try mode: the root is an overlay\n# {line}\n")),
            Some("none") if !comment && fields.get(2) == Some(&"swap") && on_root(fields[0]) => {
                text.push_str(&format!("# archix try mode: no swap file in the overlay\n# {line}\n"))
            }
            Some("/var/lib") if !comment => match varlib {
                Some(varlib) => text.push_str(&format!(
                    "# archix try mode: {line}\noverlay /var/lib overlay lowerdir={},upperdir={},workdir={} 0 0\n",
                    varlib.display(),
                    layer.join("varlib").display(),
                    layer.join("varlib-work").display()
                )),
                None => text.push_str(&format!("{line}\n")),
            },
            _ => text.push_str(&format!("{line}\n")),
        }
    }
    text
}

/// The pacman hook a try boot gets in its throwaway /etc, so no package changes into a layer that's gone at the next boot.
pub fn refusal(snapshot: u32) -> (String, String) {
    (
        "[Trigger]\nOperation = Install\nOperation = Upgrade\nOperation = Remove\nType = Package\nTarget = *\n\n\
         [Action]\nDescription = archix: refusing package changes in try mode\nWhen = PreTransaction\n\
         Exec = /bin/sh /etc/pacman.d/archix-try-refusal.sh\nAbortOnFail\n"
            .to_string(),
        format!(
            "echo 'booted snapshot {snapshot} read-only; `archix rollback {snapshot}` to keep, or reboot' >&2\nexit 1\n"
        ),
    )
}

/// In the initramfs, with the root mounted at `root`: works out what the started entry boots and, for a snapshot, mounts it over `root` read-only with the throwaway layer; for the current system leaves `root` as it is and drops the last try's layer. `run` is RUN, where the choice is recorded.
pub fn mount(root: &Path, run: &Path, cmdline: &str) -> Result<Choice> {
    if super::rescue::requested(cmdline) {
        return Ok(Choice::Current);
    }
    let Some((device, fs_type)) = mounted_at(root) else {
        bail!("nothing is mounted at {}", root.display());
    };
    if fs_type != "btrfs" {
        return Ok(Choice::Current);
    }
    root_in_at(root, &device)?;
    let store = run.join("store");
    fs::create_dir_all(&store).with_context(|| format!("creating {}", store.display()))?;
    if sys_mount(&device, &store, "btrfs", 0, &format!("subvol=/{}", layout::ARCHIX)).is_err() {
        let _ = fs::remove_dir(&store);
        return Ok(Choice::Current);
    }
    super::boot::mount_efivars();
    let selected = crate::testhooks::selected_entry();
    let choice = choose(&store, selected.as_deref(), cmdline);
    match &choice {
        Choice::Current => {
            let _ = fs::remove_file(store.join("try").join(LAYER_FILE));
            umount(&store);
            let _ = fs::remove_dir(&store);
        }
        Choice::Try { snapshot, why } => {
            if let Some(why) = why {
                println!("archix: {why}");
            }
            try_snapshot(root, run, &store, &device, *snapshot, why.clone())?;
            println!("archix: booting snapshot {snapshot} read-only (try mode); changes are lost at the next boot");
        }
        Choice::Rescue { why } => {
            umount(&store);
            rescue_next(why)?;
        }
    }
    Ok(choice)
}

fn try_snapshot(root: &Path, run: &Path, store: &Path, device: &str, snapshot: u32, why: Option<String>) -> Result<()> {
    let layer = run.join("layer");
    let (kind, size) = make_layer(&store.join("try"), &layer, &LAYER_SIZES);
    for dir in ["root", "root-work", "varlib", "varlib-work"] {
        fs::create_dir_all(layer.join(dir)).with_context(|| format!("creating the layer's {dir}"))?;
    }
    let lower = store.join("snapshots").join(snapshot.to_string()).join("root");
    sys_mount(
        "overlay",
        root,
        "overlay",
        0,
        &format!(
            "lowerdir={},upperdir={},workdir={}",
            lower.display(),
            layer.join("root").display(),
            layer.join("root-work").display()
        ),
    )
    .with_context(|| format!("mounting snapshot {snapshot} with its layer"))?;
    let varlib = run.join("varlib");
    fs::create_dir_all(&varlib)?;
    let varlib = sys_mount(
        device,
        &varlib,
        "btrfs",
        libc::MS_RDONLY,
        &format!("subvol=/{}", layout::VAR_LIB),
    )
    .is_ok()
    .then_some(varlib);
    let fstab = root.join("etc/fstab");
    let text = fs::read_to_string(&fstab).unwrap_or_default();
    fs::write(&fstab, try_fstab(&text, varlib.as_deref(), &layer)).context("writing the try boot's fstab")?;
    let (hook, script) = refusal(snapshot);
    let hooks = root.join("etc/pacman.d/hooks");
    fs::create_dir_all(&hooks)?;
    fs::write(hooks.join("00-archix-try.hook"), hook)?;
    fs::write(root.join("etc/pacman.d/archix-try-refusal.sh"), script)?;
    let record = Trying {
        snapshot,
        why,
        layer: kind,
        size,
    };
    fs::write(run.join(TRY), serde_json::to_string_pretty(&record)?).context("recording the try boot")?;
    Ok(())
}

/// The subvolume path and whether read-only, of what's mounted at `point`.
fn mounted_subvolume(point: &Path) -> Option<(String, bool)> {
    let info = fs::read_to_string("/proc/self/mountinfo").ok()?;
    info.lines().rev().find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if Path::new(fields.get(4)?) != point {
            return None;
        }
        Some((fields.get(3)?.to_string(), fields.get(5)?.split(',').any(|o| o == "ro")))
    })
}

/// With btrfs's top level mounted at `root` as the command line has it: finishes a scheduled move of the root into `@`, and from then on mounts `@` there instead, as the old root's files are gone from the top level.
fn root_in_at(root: &Path, device: &str) -> Result<()> {
    let Some((subvolume, read_only)) = mounted_subvolume(root) else {
        return Ok(());
    };
    let (mark, moved) = (
        root.join(crate::snapshot::rootmove::MARK),
        root.join(crate::snapshot::rootmove::MOVED),
    );
    if subvolume != "/" || !(mark.exists() || moved.exists()) {
        return Ok(());
    }
    if mark.exists() {
        println!("archix: moving the root into @, which takes a while once; don't power off");
        sys_mount(device, root, "btrfs", libc::MS_REMOUNT, "").context("mounting the top level read-write")?;
    }
    match crate::snapshot::rootmove::finish(root)? {
        crate::snapshot::rootmove::Finished::NotMoved => return Ok(()),
        crate::snapshot::rootmove::Finished::InAt { kept } => {
            for path in kept {
                println!(
                    "archix: {} is a subvolume, so it stays at the top level",
                    path.display()
                );
            }
        }
    }
    umount(root);
    let flags = if read_only { libc::MS_RDONLY } else { 0 };
    sys_mount(
        device,
        root,
        "btrfs",
        flags,
        &format!("subvol=/{}", crate::snapshot::ROOT),
    )
    .context("mounting @ as the root")
}

/// Makes the rescue entry the next boot's, once, and reboots into it; with no EFI variables to set, fails so the initramfs opens its shell.
fn rescue_next(why: &str) -> Result<()> {
    println!("archix: {why}; starting the rescue entry");
    let Some(dir) = crate::testhooks::efivars() else {
        bail!("{why}, and there are no EFI variables to start the rescue entry with");
    };
    let path = dir.join("LoaderEntryOneShot-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f");
    let mut bytes = vec![7u8, 0, 0, 0];
    bytes.extend(
        super::rescue::ENTRY
            .encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes),
    );
    let _ = sys_mount("efivarfs", &dir, "efivarfs", libc::MS_REMOUNT, "");
    fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    if cfg!(debug_assertions) && std::env::var_os("ARCHIX_TEST_NO_REBOOT").is_some() {
        return Ok(());
    }
    // An EFI variable is written through to the firmware at once, and nothing else here was written.
    unsafe {
        libc::reboot(libc::RB_AUTOBOOT);
    }
    bail!("the reboot into the rescue entry failed")
}

/// Whether the root at `root` builds its initramfs with archix's hook, which a move of the root into `@` needs.
pub fn hook_installed(root: &Path) -> bool {
    root.join("etc/mkinitcpio.conf.d/archix.conf").is_file()
}

/// The mkinitcpio hook for both initramfs types, its initrd unit and the conf.d line that adds it.
pub fn hook_files() -> Vec<(PathBuf, String)> {
    let static_binary = crate::integration::STATIC_BINARY;
    let root = Path::new("/");
    vec![
        (
            root.join("etc/initcpio/install/archix"),
            format!(
                "#!/bin/bash\n\nbuild() {{\n    add_binary {static_binary} /usr/bin/archix\n    if type -P mkfs.ext4 > /dev/null; then\n        add_binary mkfs.ext4\n    fi\n    if type -P btrfs > /dev/null; then\n        add_binary btrfs\n    fi\n    \
                 local module path\n    for module in loop overlay; do\n        path=$(modinfo -k \"$KERNELVERSION\" -n \"$module\" 2> /dev/null) || continue\n        [ ! -e \"$path\" ] || add_module \"$module\"\n    done\n    add_runscript\n    \
                 add_file /etc/initcpio/archix/archix-boot-mount.service /usr/lib/systemd/system/archix-boot-mount.service\n    \
                 add_symlink /usr/lib/systemd/system/initrd-root-fs.target.requires/archix-boot-mount.service /usr/lib/systemd/system/archix-boot-mount.service\n}}\n\n\
                 help() {{\n    echo \"archix: boots the snapshot a boot entry names, read-only with a throwaway layer\"\n}}\n"
            ),
        ),
        (
            root.join("etc/initcpio/hooks/archix"),
            "#!/usr/bin/ash\n\nrun_latehook() {\n    if ! /usr/bin/archix boot-mount /new_root; then\n        \
             err \"archix couldn't mount what this boot entry boots; pick another entry\"\n        \
             launch_interactive_shell\n    fi\n}\n"
                .to_string(),
        ),
        (
            root.join("etc/initcpio/archix/archix-boot-mount.service"),
            MOUNT_UNIT.to_string(),
        ),
        (
            root.join("etc/mkinitcpio.conf.d/archix.conf"),
            "HOOKS+=(archix)\n".to_string(),
        ),
    ]
}

const MOUNT_UNIT: &str = "[Unit]\nDescription=archix: mount what this boot entry boots\nDefaultDependencies=no\n\
     Requires=sysroot.mount\nAfter=sysroot.mount\nBefore=initrd-root-fs.target initrd-parse-etc.service\n\n\
     [Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=/usr/bin/archix boot-mount /sysroot\nStandardOutput=journal+console\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(store: &Path, number: u32, entry: bool) {
        let dir = store.join("snapshots").join(number.to_string());
        fs::create_dir_all(dir.join("root")).unwrap();
        fs::write(dir.join("record.json"), format!("{{\"number\": {number}}}")).unwrap();
        if entry {
            let record = menu::record_path(&store.join("boot"), &Entry::Snapshot(number).id());
            fs::create_dir_all(record.parent().unwrap()).unwrap();
            fs::write(record, "title t\n").unwrap();
        }
    }

    #[test]
    fn a_snapshot_entry_archix_wrote_boots_its_snapshot_and_anything_else_the_current_system() {
        let store = tempfile::tempdir().unwrap();
        let store = store.path();
        snapshot(store, 3, true);
        snapshot(store, 4, false);
        let choose = |selected: Option<&str>, cmdline: &str| choose(store, selected, cmdline);
        assert_eq!(
            choose(Some("archix-snapshot-3.conf"), ""),
            Choice::Try { snapshot: 3, why: None }
        );
        assert_eq!(
            choose(Some("archix-snapshot-4.conf"), ""),
            Choice::Current,
            "archix never recorded an entry for 4"
        );
        assert_eq!(choose(Some("archix-current.conf"), ""), Choice::Current);
        assert_eq!(choose(Some("windows.conf"), "archix.snapshot=3"), Choice::Current);
        assert_eq!(
            choose(None, "root=x archix.snapshot=3"),
            Choice::Try { snapshot: 3, why: None },
            "GRUB names it on the command line"
        );
        assert_eq!(choose(None, "root=x"), Choice::Current);
    }

    #[test]
    fn a_missing_snapshot_boots_the_newest_survivor_whose_entry_never_failed_else_rescue() {
        let store = tempfile::tempdir().unwrap();
        let store = store.path();
        for n in [2, 3, 4] {
            snapshot(store, n, true);
        }
        snapshot(store, 5, false);
        let gone = menu::record_path(&store.join("boot"), "archix-snapshot-9.conf");
        fs::write(&gone, "title t\n").unwrap();
        let state = menu::State {
            failed: [4].into(),
            ..menu::State::default()
        };
        menu::save(&store.join("boot"), &state).unwrap();
        match choose(store, Some("archix-snapshot-9+0-1.conf"), "") {
            Choice::Try {
                snapshot: 3,
                why: Some(why),
            } => assert!(why.contains("snapshot 9"), "{why}"),
            other => panic!("{other:?}"),
        }
        for n in [2, 3] {
            fs::remove_dir_all(store.join("snapshots").join(n.to_string())).unwrap();
        }
        assert!(matches!(
            choose(store, Some("archix-snapshot-9.conf"), ""),
            Choice::Rescue { .. }
        ));
    }

    #[test]
    fn the_try_notice_says_changes_are_lost_how_to_keep_and_how_big_the_layer_is() {
        let file = Trying {
            snapshot: 7,
            why: None,
            layer: "file".into(),
            size: 4 << 30,
        };
        let text = notice(&file);
        assert!(text.contains("snapshot 7, read-only") && text.contains("lost at the next boot, even if you keep it"));
        assert!(
            text.contains("`sudo archix rollback 7` keeps it") && text.contains("4096 MiB"),
            "{text}"
        );
        let degraded = Trying {
            why: Some("snapshot 9, which the boot entry named, is gone".into()),
            layer: "tmpfs".into(),
            size: 256 << 20,
            ..file
        };
        let text = notice(&degraded);
        assert!(
            text.contains("Why: snapshot 9") && text.contains("tmpfs in memory (256 MiB)"),
            "{text}"
        );
    }

    #[test]
    fn a_try_boots_fstab_drops_the_root_and_overlays_var_lib() {
        let fstab = "# root\nUUID=a / btrfs rw,subvol=/@ 0 0\nUUID=a /var/lib btrfs rw,subvol=/@varlib 0 0\nUUID=a /home btrfs subvol=/@home 0 0\n";
        let text = try_fstab(
            fstab,
            Some(Path::new("/run/archix/varlib")),
            Path::new("/run/archix/layer"),
        );
        assert_eq!(
            text,
            "# root\n# archix try mode: the root is an overlay\n# UUID=a / btrfs rw,subvol=/@ 0 0\n\
             # archix try mode: UUID=a /var/lib btrfs rw,subvol=/@varlib 0 0\n\
             overlay /var/lib overlay lowerdir=/run/archix/varlib,upperdir=/run/archix/layer/varlib,workdir=/run/archix/layer/varlib-work 0 0\n\
             UUID=a /home btrfs subvol=/@home 0 0\n"
        );
        let (hook, script) = refusal(7);
        assert!(
            hook.contains("AbortOnFail") && hook.contains("PreTransaction"),
            "{hook}"
        );
        assert!(script.contains("booted snapshot 7 read-only; `archix rollback 7` to keep, or reboot"));
    }

    #[test]
    fn a_try_boots_fstab_leaves_out_swap_files_on_the_overlaid_root_only() {
        let fstab = "UUID=a / btrfs rw,subvol=/@ 0 0\nUUID=d /mnt/data ext4 nofail 0 2\n\
                     /swap/swapfile none swap nofail 0 0\n/var/lib/swapfile none swap defaults 0 0\n\
                     /mnt/data/swapfile none swap defaults 0 0\nUUID=s none swap defaults 0 0\n/dev/zram1 none swap defaults 0 0\n";
        let text = try_fstab(fstab, None, Path::new("/run/archix/layer"));
        for kept in [
            "\nUUID=d /mnt/data ext4 nofail 0 2\n",
            "\n/mnt/data/swapfile none swap defaults 0 0\n",
            "\nUUID=s none swap defaults 0 0\n",
            "\n/dev/zram1 none swap defaults 0 0\n",
        ] {
            assert!(text.contains(kept), "{kept:?} kept in {text}");
        }
        for dropped in [
            "/swap/swapfile none swap nofail 0 0",
            "/var/lib/swapfile none swap defaults 0 0",
        ] {
            assert!(
                text.contains(&format!(
                    "# archix try mode: no swap file in the overlay\n# {dropped}\n"
                )),
                "{dropped} left out of {text}"
            );
        }
    }
}

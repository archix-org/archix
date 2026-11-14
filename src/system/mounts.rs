//! Mounts made, read and undone with syscalls and /proc alone, and filesystems told apart by their superblocks: a rollback must work on a root whose mount, umount, findmnt and blkid can't start.

use std::ffi::CString;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// One line of /proc/self/mountinfo.
#[derive(Debug, Clone, PartialEq)]
pub struct Mount {
    /// The path inside the filesystem that is mounted, such as `/@` for a btrfs subvolume.
    pub root: String,
    pub point: PathBuf,
    pub fs_type: String,
    pub source: String,
    /// The mount's options, then the filesystem's, as findmnt's OPTIONS joins them.
    pub options: String,
}

fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes
            .get(i + 1..i + 4)
            .filter(|digits| bytes[i] == b'\\' && digits.iter().all(|digit| (b'0'..=b'7').contains(digit)));
        match octal.and_then(|digits| u8::from_str_radix(std::str::from_utf8(digits).ok()?, 8).ok()) {
            Some(byte) => {
                out.push(byte);
                i += 4;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse(mountinfo: &str) -> Vec<Mount> {
    mountinfo
        .lines()
        .filter_map(|line| {
            let (before, after) = line.split_once(" - ")?;
            let before: Vec<&str> = before.split_whitespace().collect();
            let after: Vec<&str> = after.split_whitespace().collect();
            let mut options: Vec<&str> = before.get(5)?.split(',').collect();
            for option in after.get(2).unwrap_or(&"").split(',') {
                if !option.is_empty() && !options.contains(&option) {
                    options.push(option);
                }
            }
            Some(Mount {
                root: unescape(before.get(3)?),
                point: PathBuf::from(unescape(before.get(4)?)),
                fs_type: after.first()?.to_string(),
                source: unescape(after.get(1)?),
                options: options.join(","),
            })
        })
        .collect()
}

/// Every mount this process sees, in the order they were made.
pub fn table() -> Vec<Mount> {
    parse(&fs::read_to_string("/proc/self/mountinfo").unwrap_or_default())
}

fn holding<'a>(table: &'a [Mount], path: &Path) -> Option<&'a Mount> {
    table
        .iter()
        .filter(|mount| path.starts_with(&mount.point))
        .max_by_key(|mount| mount.point.as_os_str().len())
}

/// The mount `path` is on, as `findmnt -T` finds it.
pub fn of(path: &Path) -> Option<Mount> {
    let path = fs::canonicalize(path).ok()?;
    holding(&table(), &path).cloned()
}

/// The mount at exactly `point`, the topmost one there.
pub fn at(point: &Path) -> Option<Mount> {
    table().into_iter().rev().find(|mount| mount.point == point)
}

fn c_string(bytes: &[u8]) -> io::Result<CString> {
    CString::new(bytes).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL byte in a mount argument"))
}

/// The mount(2) flags among comma-separated `options`, and the rest, which is the filesystem's own.
fn split_options(options: &str) -> (libc::c_ulong, String) {
    let mut flags = 0;
    let mut data = Vec::new();
    for option in options.split(',').filter(|option| !option.is_empty()) {
        match option {
            "rw" | "defaults" | "relatime" | "auto" | "noauto" | "nofail" => {}
            "ro" => flags |= libc::MS_RDONLY,
            "noatime" => flags |= libc::MS_NOATIME,
            "nodiratime" => flags |= libc::MS_NODIRATIME,
            "strictatime" => flags |= libc::MS_STRICTATIME,
            "lazytime" => flags |= libc::MS_LAZYTIME,
            "nosuid" => flags |= libc::MS_NOSUID,
            "nodev" => flags |= libc::MS_NODEV,
            "noexec" => flags |= libc::MS_NOEXEC,
            option if option.starts_with("x-") => {}
            option => data.push(option),
        }
    }
    (flags, data.join(","))
}

/// Mounts `source`, a block device or an image file, at `target`, made if it isn't there, as `mount -t fs_type -o options` does.
pub fn mount(source: &str, target: &Path, fs_type: &str, options: &str) -> Result<()> {
    fs::create_dir_all(target).with_context(|| format!("creating {}", target.display()))?;
    // A loop device for an image file, as mount makes one, held open until the mount has it.
    let image = Path::new(source);
    let looped = match image.is_absolute() && image.is_file() {
        true => Some(super::initrd::attach_loop(image).with_context(|| format!("making a loop device for {source}"))?),
        false => None,
    };
    let source = looped
        .as_ref()
        .and_then(|(device, _)| device.to_str())
        .unwrap_or(source);
    let (flags, data) = split_options(options);
    let (c_source, c_target) = (c_string(source.as_bytes())?, c_string(target.as_os_str().as_bytes())?);
    let (c_type, c_data) = (c_string(fs_type.as_bytes())?, c_string(data.as_bytes())?);
    let done = unsafe {
        libc::mount(
            c_source.as_ptr(),
            c_target.as_ptr(),
            c_type.as_ptr(),
            flags,
            c_data.as_ptr().cast(),
        )
    };
    if done != 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("mounting {source} ({fs_type}, {options}) at {}", target.display()));
    }
    Ok(())
}

/// Binds `source` over `target`, with the mounts under it when `recursive`.
pub fn bind(source: &Path, target: &Path, recursive: bool) -> Result<()> {
    let (c_source, c_target) = (
        c_string(source.as_os_str().as_bytes())?,
        c_string(target.as_os_str().as_bytes())?,
    );
    let flags = libc::MS_BIND | if recursive { libc::MS_REC } else { 0 };
    let done = unsafe {
        libc::mount(
            c_source.as_ptr(),
            c_target.as_ptr(),
            std::ptr::null(),
            flags,
            std::ptr::null(),
        )
    };
    if done != 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("binding {} over {}", source.display(), target.display()));
    }
    Ok(())
}

fn umount(point: &Path, flags: libc::c_int) -> io::Result<()> {
    let c_point = c_string(point.as_os_str().as_bytes())?;
    match unsafe { libc::umount2(c_point.as_ptr(), flags) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

/// Unmounts everything mounted at or under `dir`, the newest first, as `umount -R` does; each filesystem is flushed by its unmount.
pub fn unmount_under(dir: &Path) -> Result<()> {
    for mount in table().iter().rev().filter(|mount| mount.point.starts_with(dir)) {
        umount(&mount.point, 0).with_context(|| format!("unmounting {}", mount.point.display()))?;
    }
    Ok(())
}

/// Detaches everything mounted at or under `dir` at once, busy or not, as `umount -R -l` does.
pub fn detach_under(dir: &Path) {
    for mount in table().iter().rev().filter(|mount| mount.point.starts_with(dir)) {
        let _ = umount(&mount.point, libc::MNT_DETACH);
    }
}

/// What a block device holds, by its superblock.
#[derive(Debug, Clone, PartialEq)]
pub struct Probe {
    /// The type mount(2) takes: btrfs, ext4 (for ext2 and ext3 too), vfat, xfs or crypto_LUKS.
    pub fs_type: &'static str,
    /// A btrfs's UUID, which its devices share.
    pub uuid: Option<String>,
}

fn probe_bytes(read: &mut dyn FnMut(u64, &mut [u8]) -> bool) -> Option<Probe> {
    let found = |fs_type| Some(Probe { fs_type, uuid: None });
    let mut head = [0u8; 2048];
    if !read(0, &mut head) {
        return None;
    }
    let mut btrfs = [0u8; 0x48];
    if read(0x10000, &mut btrfs) && &btrfs[0x40..0x48] == b"_BHRfS_M" {
        let hex: String = btrfs[0x20..0x30].iter().map(|byte| format!("{byte:02x}")).collect();
        return Some(Probe {
            fs_type: "btrfs",
            uuid: Some(format!(
                "{}-{}-{}-{}-{}",
                &hex[..8],
                &hex[8..12],
                &hex[12..16],
                &hex[16..20],
                &hex[20..]
            )),
        });
    }
    if &head[..6] == b"LUKS\xba\xbe" {
        return found("crypto_LUKS");
    }
    if &head[..4] == b"XFSB" {
        return found("xfs");
    }
    if head[0x438..0x43a] == [0x53, 0xef] {
        return found("ext4");
    }
    if head[510..512] == [0x55, 0xaa] && (&head[82..87] == b"FAT32" || &head[54..57] == b"FAT") {
        return found("vfat");
    }
    None
}

/// What `device` holds, if it's a filesystem archix mounts or a LUKS volume.
pub fn probe(device: &Path) -> Option<Probe> {
    let mut file = fs::File::open(device).ok()?;
    probe_bytes(&mut |offset, buffer| file.seek(SeekFrom::Start(offset)).is_ok() && file.read_exact(buffer).is_ok())
}

/// Every block device there is, by its /dev path; device-mapper volumes by their /dev/mapper names.
pub fn block_devices() -> Vec<PathBuf> {
    let mut devices: Vec<PathBuf> = fs::read_dir("/sys/class/block")
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| {
            let mapped = fs::read_to_string(entry.path().join("dm/name")).ok();
            match mapped.as_deref().map(str::trim).filter(|name| !name.is_empty()) {
                Some(name) => Path::new("/dev/mapper").join(name),
                None => Path::new("/dev").join(entry.file_name()),
            }
        })
        .filter(|device| device.exists())
        .collect();
    devices.sort();
    devices
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = "\
22 1 0:20 / / rw,relatime shared:1 - ext4 /dev/vda2 rw
30 22 0:27 /@ /mnt/a\\040b rw,noatime shared:5 - btrfs /dev/loop0 rw,compress=zstd:3,subvolid=256,subvol=/@
31 30 0:27 /@archix /mnt/a\\040b/.archix rw,relatime shared:6 - btrfs /dev/loop0 rw,subvolid=257,subvol=/@archix
32 30 0:27 /@/var/rollback/pacman /mnt/a\\040b/var/lib/pacman rw,relatime - btrfs /dev/loop0 rw,subvolid=256,subvol=/@
";

    #[test]
    fn a_path_is_on_the_deepest_mount_above_it_with_its_subvolume_and_joined_options() {
        let table = parse(TABLE);
        let on = |path: &str| holding(&table, Path::new(path)).unwrap();
        assert_eq!(on("/etc/fstab").source, "/dev/vda2");
        let root = on("/mnt/a b/etc");
        assert_eq!((root.root.as_str(), root.fs_type.as_str()), ("/@", "btrfs"));
        assert_eq!(root.options, "rw,noatime,compress=zstd:3,subvolid=256,subvol=/@");
        assert_eq!(on("/mnt/a b/.archix/snapshots").root, "/@archix");
        assert_eq!(on("/mnt/a b/var/lib/pacman/local").root, "/@/var/rollback/pacman");
        assert_eq!(
            on("/mnt/a bc").point,
            Path::new("/"),
            "a name that only starts alike is elsewhere"
        );
    }

    #[test]
    fn mount_flags_are_split_from_the_filesystems_own_options() {
        assert_eq!(
            split_options("rw,noatime,compress=zstd:3,subvol=/@,x-systemd.automount,nofail"),
            (libc::MS_NOATIME, "compress=zstd:3,subvol=/@".to_string())
        );
        assert_eq!(
            split_options("ro,rescue=all"),
            (libc::MS_RDONLY, "rescue=all".to_string())
        );
        assert_eq!(split_options(""), (0, String::new()));
    }

    #[test]
    fn superblocks_tell_btrfs_ext_fat_and_luks_apart() {
        let probe_of = |writes: &[(usize, &[u8])]| {
            let mut disk = vec![0u8; 0x11000];
            for (at, bytes) in writes {
                disk[*at..*at + bytes.len()].copy_from_slice(bytes);
            }
            probe_bytes(&mut |offset, buffer| {
                let from = offset as usize;
                disk.get(from..from + buffer.len())
                    .map(|bytes| buffer.copy_from_slice(bytes))
                    .is_some()
            })
        };
        let fsid: Vec<u8> = (1..=16).collect();
        let btrfs = probe_of(&[(0x10040, b"_BHRfS_M"), (0x10020, &fsid)]).unwrap();
        assert_eq!(btrfs.fs_type, "btrfs");
        assert_eq!(btrfs.uuid.as_deref(), Some("01020304-0506-0708-090a-0b0c0d0e0f10"));
        assert_eq!(probe_of(&[(0x438, &[0x53, 0xef])]).unwrap().fs_type, "ext4");
        assert_eq!(
            probe_of(&[(510, &[0x55, 0xaa]), (82, b"FAT32   ")]).unwrap().fs_type,
            "vfat"
        );
        assert_eq!(probe_of(&[(0, b"LUKS\xba\xbe")]).unwrap().fs_type, "crypto_LUKS");
        assert_eq!(
            probe_of(&[(510, &[0x55, 0xaa])]),
            None,
            "a partition table alone is no filesystem"
        );
        assert_eq!(probe_of(&[]), None);
    }
}

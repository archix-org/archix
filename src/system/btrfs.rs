use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Result, bail};

const fn request(direction: u64, number: u64, size: usize) -> libc::Ioctl {
    ((direction << 30) | ((size as u64) << 16) | (0x94 << 8) | number) as libc::Ioctl
}

/// btrfs_ioctl_vol_args: a file descriptor and a name.
#[repr(C)]
struct VolArgs {
    fd: i64,
    name: [u8; 4088],
}

/// btrfs_ioctl_vol_args_v2, whose flags can make a snapshot read-only.
#[repr(C)]
struct VolArgsV2 {
    fd: i64,
    transid: u64,
    flags: u64,
    unused: [u64; 4],
    name: [u8; 4040],
}

/// btrfs_ioctl_get_subvol_info_args.
#[repr(C)]
struct SubvolInfo {
    treeid: u64,
    name: [u8; 256],
    parent_id: u64,
    dirid: u64,
    generation: u64,
    flags: u64,
    uuid: [u8; 16],
    rest: [u8; 192],
}

/// btrfs_ioctl_fs_info_args.
#[repr(C)]
struct FsInfo {
    max_id: u64,
    num_devices: u64,
    fsid: [u8; 16],
    rest: [u8; 992],
}

const SNAP_CREATE_V2: libc::Ioctl = request(1, 23, size_of::<VolArgsV2>());
const SNAP_DESTROY: libc::Ioctl = request(1, 15, size_of::<VolArgs>());
const SCAN_DEV: libc::Ioctl = request(1, 4, size_of::<VolArgs>());
const GET_SUBVOL_INFO: libc::Ioctl = request(2, 60, size_of::<SubvolInfo>());
const FS_INFO: libc::Ioctl = request(2, 31, size_of::<FsInfo>());
const SUBVOL_RDONLY: u64 = 1 << 1;
/// The inode number of every subvolume's root directory.
const SUBVOLUME_ROOT: u64 = 256;

/// The directory holding `path` opened, and `path`'s name in it as the ioctls take it.
fn parent_and_name<const N: usize>(path: &Path) -> io::Result<(File, [u8; N])> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "not a name in a directory");
    let name = path.file_name().ok_or_else(invalid)?.as_bytes();
    if name.len() >= N {
        return Err(invalid());
    }
    let mut bytes = [0u8; N];
    bytes[..name.len()].copy_from_slice(name);
    let parent = path.parent().filter(|dir| !dir.as_os_str().is_empty());
    Ok((File::open(parent.unwrap_or(Path::new(".")))?, bytes))
}

/// Snapshots the subvolume at `source` as `target`, as `btrfs subvolume snapshot [-r]` does, with the ioctl alone: a rollback must work on a root whose btrfs can't start.
pub fn snapshot(source: &Path, target: &Path, read_only: bool) -> io::Result<()> {
    let source = File::open(source)?;
    let (parent, name) = parent_and_name(target)?;
    let args = VolArgsV2 {
        fd: i64::from(source.as_raw_fd()),
        transid: 0,
        flags: if read_only { SUBVOL_RDONLY } else { 0 },
        unused: [0; 4],
        name,
    };
    match unsafe { libc::ioctl(parent.as_raw_fd(), SNAP_CREATE_V2, &args) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

/// Deletes the subvolume at `path`, as `btrfs subvolume delete` does.
pub fn delete_subvolume(path: &Path) -> io::Result<()> {
    let (parent, name) = parent_and_name(path)?;
    let args = VolArgs { fd: 0, name };
    match unsafe { libc::ioctl(parent.as_raw_fd(), SNAP_DESTROY, &args) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

fn uuid_text(bytes: &[u8; 16]) -> String {
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// The UUID of the subvolume whose root directory `path` is, as `btrfs subvolume show` prints it; none for the top level, which has none, or a path that isn't a subvolume's root.
pub fn subvolume_uuid(path: &Path) -> Option<String> {
    let dir = File::open(path).ok()?;
    if dir.metadata().ok()?.ino() != SUBVOLUME_ROOT {
        return None;
    }
    let mut info: SubvolInfo = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(dir.as_raw_fd(), GET_SUBVOL_INFO, &mut info) } != 0 || info.uuid == [0; 16] {
        return None;
    }
    Some(uuid_text(&info.uuid))
}

/// The UUID of the btrfs mounted at or above `path`, as blkid and fstab's UUID= name it.
pub fn filesystem_uuid(path: &Path) -> Option<String> {
    let dir = File::open(path).ok()?;
    let mut info: FsInfo = unsafe { std::mem::zeroed() };
    (unsafe { libc::ioctl(dir.as_raw_fd(), FS_INFO, &mut info) } == 0).then(|| uuid_text(&info.fsid))
}

/// Tells the kernel about `device`, one of a btrfs's devices, as `btrfs device scan` does, so a filesystem on several mounts.
pub fn scan_device(device: &Path) {
    let name = device.as_os_str().as_bytes();
    let Ok(control) = File::options().read(true).write(true).open("/dev/btrfs-control") else {
        return;
    };
    let mut args = VolArgs { fd: 0, name: [0; 4088] };
    if name.len() < args.name.len() {
        args.name[..name.len()].copy_from_slice(name);
        unsafe { libc::ioctl(control.as_raw_fd(), SCAN_DEV, &args) };
    }
}

/// What `btrfs filesystem usage -b` reports that matters for writing: space no chunk holds yet, and metadata's size and use.
#[derive(Debug, PartialEq)]
struct Usage {
    unallocated: u64,
    metadata_size: u64,
    metadata_used: u64,
}

const MIB: u64 = 1 << 20;
/// Below this much unallocated space, empty data chunks are handed back so metadata can still grow.
const LOW_UNALLOCATED: u64 = 1 << 30;

fn parse(report: &str) -> Option<Usage> {
    let number = |text: &str| text.split_whitespace().next()?.parse::<u64>().ok();
    let unallocated = report
        .lines()
        .find_map(|line| line.trim().strip_prefix("Device unallocated:"))
        .and_then(number)?;
    let metadata = report.lines().find(|line| line.starts_with("Metadata,"))?;
    let field = |name: &str| {
        let rest = metadata.split(name).nth(1)?;
        rest.split([',', ' ', '('])
            .find(|part| !part.is_empty())?
            .parse::<u64>()
            .ok()
    };
    Some(Usage {
        unallocated,
        metadata_size: field("Size:")?,
        metadata_used: field("Used:")?,
    })
}

/// The room still free to write on the btrfs at `mount`, in percent and bytes, as `btrfs filesystem usage -b` estimates it from unallocated space and room in the data chunks.
pub fn space(mount: &Path) -> Option<crate::snapshot::Space> {
    let output = Command::new("btrfs")
        .args(["filesystem", "usage", "-b"])
        .arg(mount)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| space_in(&String::from_utf8_lossy(&output.stdout)))
        .flatten()
}

fn space_in(report: &str) -> Option<crate::snapshot::Space> {
    let field = |name: &str| {
        report
            .lines()
            .find_map(|line| line.trim().strip_prefix(name))?
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()
    };
    let (size, free) = (field("Device size:")?, field("Free (estimated):")?);
    (size > 0).then(|| crate::snapshot::Space {
        percent: free as f64 * 100.0 / size as f64,
        bytes: free,
    })
}

/// Bytes no chunk holds yet on the btrfs at `mount`.
pub fn unallocated(mount: &Path) -> Option<u64> {
    usage(mount).map(|usage| usage.unallocated)
}

fn usage(mount: &Path) -> Option<Usage> {
    let output = Command::new("btrfs")
        .args(["filesystem", "usage", "-b"])
        .arg(mount)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| parse(&String::from_utf8_lossy(&output.stdout)))
        .flatten()
}

/// Whether metadata can't grow: almost no unallocated space for a new chunk, and the current ones almost full.
fn short_of_metadata(usage: &Usage) -> bool {
    usage.unallocated < 256 * MIB && usage.metadata_size.saturating_sub(usage.metadata_used) < 128 * MIB
}

/// btrfs can show free space and still fail writes: once data chunks fill the device, metadata can't grow. This hands empty data chunks back before a switch writes anything, and refuses one that would still fail halfway.
pub fn ensure_metadata_room(mount: &Path) -> Result<()> {
    if !crate::machine::is_btrfs(mount) {
        return Ok(());
    }
    let Some(before) = usage(mount) else {
        return Ok(());
    };
    if before.unallocated >= LOW_UNALLOCATED {
        return Ok(());
    }
    // Freed space counts only once committed, so a file deleted just now would still hold its chunk.
    if let Ok(dir) = File::open(mount) {
        unsafe { libc::syncfs(dir.as_raw_fd()) };
    }
    // Only completely empty data chunks move, so this is quick and touches no data.
    let _ = Command::new("btrfs")
        .args(["balance", "start", "-dusage=0"])
        .arg(mount)
        .output();
    let after = usage(mount).unwrap_or(before);
    if short_of_metadata(&after) {
        bail!(
            "btrfs on {} has room for data but almost none for metadata ({} MiB of metadata free, {} MiB unallocated), so writes would fail halfway; nothing was changed. Run `sudo btrfs balance start -dusage=10 {}` and try again",
            mount.display(),
            after.metadata_size.saturating_sub(after.metadata_used) / MIB,
            after.unallocated / MIB,
            mount.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ioctl_numbers_and_sizes_are_the_kernels() {
        assert_eq!(SNAP_CREATE_V2 as u32, 0x5000_9417);
        assert_eq!(SNAP_DESTROY as u32, 0x5000_940f);
        assert_eq!(SCAN_DEV as u32, 0x5000_9404);
        assert_eq!(GET_SUBVOL_INFO as u32, 0x81f8_943c);
        assert_eq!(FS_INFO as u32, 0x8400_941f);
        assert_eq!(
            uuid_text(&[0x1c, 0x0a, 0xff, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 0xab]),
            "1c0aff00-0102-0304-0506-0708090a0bab"
        );
    }

    // From the VM run that failed: data chunks filled the device after a full-disk test, metadata 93% used.
    const STUCK: &str = "Overall:
    Device size:		       16548626432
    Device allocated:		       16547577856
    Device unallocated:		           1048576
    Used:			        6755629056

Data,single: Size:15993929728, Used:6254284800 (39.10%)
   /dev/loop2	15993929728

Metadata,DUP: Size:268435456, Used:250662912 (93.38%)
   /dev/loop2	 536870912
";

    #[test]
    fn free_space_is_the_estimate_against_the_device_size() {
        let report =
            "Overall:\n    Device size:\t\t  1073741824\n    Free (estimated):\t\t   268435456\t(min: 134217728)\n";
        assert_eq!(
            space_in(report),
            Some(crate::snapshot::Space {
                percent: 25.0,
                bytes: 268435456
            })
        );
        assert_eq!(space_in(STUCK), None, "no estimate, no number");
    }

    #[test]
    fn a_device_full_of_data_chunks_is_short_of_metadata() {
        let usage = parse(STUCK).unwrap();
        assert_eq!(
            usage,
            Usage {
                unallocated: 1_048_576,
                metadata_size: 268_435_456,
                metadata_used: 250_662_912
            }
        );
        assert!(short_of_metadata(&usage));
        let reclaimed = Usage {
            unallocated: 8 << 30,
            ..usage
        };
        assert!(!short_of_metadata(&reclaimed));
    }
}

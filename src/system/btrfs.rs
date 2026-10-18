use std::path::Path;
use std::process::Command;

use anyhow::{Result, bail};

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
    if !crate::capture::is_btrfs(mount) {
        return Ok(());
    }
    let Some(before) = usage(mount) else {
        return Ok(());
    };
    if before.unallocated >= LOW_UNALLOCATED {
        return Ok(());
    }
    // Freed space counts only once committed, so a file deleted just now would still hold its chunk.
    if let Ok(dir) = std::fs::File::open(mount) {
        unsafe { libc::syncfs(std::os::fd::AsRawFd::as_raw_fd(&dir)) };
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

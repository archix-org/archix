//! Where secrets could reach a disk unencrypted: the root filesystem the identity is on, and swap, live or in fstab; and which retired identities a kept generation still needs.

use std::fs;
use std::path::{Path, PathBuf};

use super::crypto;

/// Whether the block device at `dev` (any path that resolves to one) is dm-crypt, a device-mapper stack whose every leg is, or zram, which lives in memory.
pub fn device_encrypted(dev: &Path) -> bool {
    let Ok(real) = fs::canonicalize(dev) else {
        return false;
    };
    let Some(name) = real.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return false;
    };
    encrypted_block(&name, 0)
}

fn encrypted_block(name: &str, depth: usize) -> bool {
    if name.starts_with("zram") {
        return true;
    }
    if depth > 8 || !name.starts_with("dm-") {
        return false;
    }
    let sys = Path::new("/sys/block").join(name);
    if fs::read_to_string(sys.join("dm/uuid")).is_ok_and(|uuid| uuid.starts_with("CRYPT-")) {
        return true;
    }
    let slaves: Vec<String> = fs::read_dir(sys.join("slaves"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    !slaves.is_empty() && slaves.iter().all(|slave| encrypted_block(slave, depth + 1))
}

/// The source device of the filesystem that holds `path`, from this namespace's mount table.
fn backing_device(path: &Path) -> Option<PathBuf> {
    let info = fs::read_to_string("/proc/self/mountinfo").ok()?;
    let mut best: Option<(usize, PathBuf)> = None;
    for line in info.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        let (Some(point), Some(dash)) = (fields.get(4), fields.iter().position(|f| *f == "-")) else {
            continue;
        };
        let point = point.replace("\\040", " ");
        let Some(source) = fields.get(dash + 2) else { continue };
        if path.starts_with(&point) && best.as_ref().is_none_or(|(len, _)| point.len() >= *len) {
            best = Some((point.len(), PathBuf::from(source)));
        }
    }
    best.map(|(_, source)| source)
}

/// Whether the root filesystem sits on dm-crypt.
pub fn root_encrypted() -> bool {
    backing_device(Path::new("/")).is_some_and(|dev| device_encrypted(&dev))
}

/// A swap known to the running system or the fstab at `root`, and whether it's encrypted.
#[derive(Debug, Clone, PartialEq)]
pub struct Swap {
    pub name: String,
    pub encrypted: bool,
}

fn fstab_device(spec: &str) -> PathBuf {
    for (prefix, dir) in [
        ("UUID=", "by-uuid"),
        ("LABEL=", "by-label"),
        ("PARTUUID=", "by-partuuid"),
        ("PARTLABEL=", "by-partlabel"),
    ] {
        if let Some(value) = spec.strip_prefix(prefix) {
            return Path::new("/dev/disk").join(dir).join(value.trim_matches('"'));
        }
    }
    PathBuf::from(spec)
}

/// Every active swap and every swap the fstab at `root` names, which may not be on yet this early in a boot.
pub fn swaps(root: &Path) -> Vec<Swap> {
    let crypttab = fs::read_to_string(root.join("etc/crypttab")).unwrap_or_default();
    let crypt_names: Vec<&str> = crypttab
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    let mut found: Vec<Swap> = Vec::new();
    let add = |found: &mut Vec<Swap>, name: String, encrypted: bool| {
        if !found.iter().any(|swap| swap.name == name) {
            found.push(Swap { name, encrypted });
        }
    };
    let proc_swaps = fs::read_to_string("/proc/swaps").unwrap_or_default();
    for line in proc_swaps.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (Some(name), Some(kind)) = (fields.first(), fields.get(1)) else {
            continue;
        };
        let name = name.replace("\\040", " ");
        let encrypted = match *kind {
            "file" => backing_device(Path::new(&name)).is_some_and(|dev| device_encrypted(&dev)),
            _ => device_encrypted(Path::new(&name)),
        };
        add(&mut found, name, encrypted);
    }
    let fstab = fs::read_to_string(root.join("etc/fstab")).unwrap_or_default();
    for line in fstab.lines().filter(|line| !line.trim_start().starts_with('#')) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.get(2) != Some(&"swap") {
            continue;
        }
        let spec = fields[0];
        let device = fstab_device(spec);
        let by_crypttab = device
            .strip_prefix("/dev/mapper")
            .ok()
            .and_then(|name| name.to_str())
            .is_some_and(|name| crypt_names.contains(&name));
        let encrypted = by_crypttab
            || device_encrypted(&device)
            || (!spec.starts_with("/dev/")
                && backing_device(Path::new(spec)).is_some_and(|dev| device_encrypted(&dev)));
        if let Ok(real) = fs::canonicalize(&device)
            && found
                .iter()
                .any(|swap| fs::canonicalize(&swap.name).ok().as_ref() == Some(&real))
        {
            continue;
        }
        add(&mut found, spec.to_string(), encrypted);
    }
    found
}

/// The swaps that aren't encrypted.
pub fn unencrypted_swaps(root: &Path) -> Vec<String> {
    swaps(root)
        .into_iter()
        .filter(|swap| !swap.encrypted)
        .map(|swap| swap.name)
        .collect()
}

/// Deletes the retired identities no kept ciphertext needs any more: one that opens a ciphertext the current identity doesn't stays; returns the recipients of those deleted.
pub fn collect_retired(state: &Path, kept: &[Vec<u8>]) -> Vec<String> {
    let current: Vec<crypto::Identity> = crypto::machine(state).into_iter().collect();
    let mut dropped = Vec::new();
    for identity in crypto::retired(state) {
        let needed = kept.iter().any(|ciphertext| {
            crypto::opener(ciphertext, std::slice::from_ref(&identity)).is_ok()
                && crypto::opener(ciphertext, &current).is_err()
        });
        if !needed && fs::remove_file(&identity.source).is_ok() {
            dropped.push(identity.recipient);
        }
    }
    dropped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_retired_identity_goes_once_no_kept_ciphertext_needs_it() {
        let state = tempfile::tempdir().unwrap();
        let (old, _) = crypto::init(state.path(), false).unwrap();
        let only_old = crypto::encrypt(std::slice::from_ref(&old), b"old-value-123456").unwrap();
        crypto::init(state.path(), true).unwrap();
        let new = crypto::machine(state.path()).unwrap().recipient;
        let both = crypto::encrypt(&[old.clone(), new.clone()], b"value-123456789").unwrap();
        assert!(
            collect_retired(state.path(), &[only_old.clone(), both.clone()]).is_empty(),
            "a deployment still needs it"
        );
        assert_eq!(crypto::retired(state.path()).len(), 1);
        assert_eq!(
            collect_retired(state.path(), &[both]),
            [old],
            "the current identity opens everything kept"
        );
        assert!(crypto::retired(state.path()).is_empty());
    }

    #[test]
    fn fstab_swaps_name_their_devices_and_a_crypttab_mapping_counts_as_encrypted() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("etc")).unwrap();
        fs::write(
            root.path().join("etc/fstab"),
            "UUID=abcd / ext4 rw 0 1\nUUID=no-such-uuid none swap defaults 0 0\n/dev/mapper/cswap none swap defaults 0 0\n",
        )
        .unwrap();
        fs::write(
            root.path().join("etc/crypttab"),
            "cswap /dev/sda3 /dev/urandom swap,cipher=aes-xts-plain64\n",
        )
        .unwrap();
        let found = swaps(root.path());
        let named = |name: &str| found.iter().find(|swap| swap.name == name).cloned();
        assert_eq!(named("UUID=no-such-uuid").map(|s| s.encrypted), Some(false));
        assert_eq!(named("/dev/mapper/cswap").map(|s| s.encrypted), Some(true));
        assert!(unencrypted_swaps(root.path()).contains(&"UUID=no-such-uuid".to_string()));
    }
}

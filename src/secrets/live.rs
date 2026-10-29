//! The values this boot decrypted, read back from the ramfs into locked memory to find copies of them on disk: the /etc merge won't carry a file that holds one into the pending deployment, and `archix status` names it.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use super::memory::{MAX_SECRET, SecretBuf, contains};
use super::scan::MIN_SCANNED;
use super::{Kind, Manifest, RUN_DIR};

/// The largest file `archix status` reads under /etc, which keeps it quick; profiles and configs that hold a key are far smaller.
const MAX_FILE: u64 = 1 << 20;
/// The most entries under /etc `archix status` looks at.
const MAX_ENTRIES: usize = 50_000;

/// Values by secret name, each in a SecretBuf of its own.
#[derive(Default)]
pub struct Values(Vec<(String, SecretBuf)>);

impl Values {
    pub fn new(values: Vec<(String, SecretBuf)>) -> Values {
        Values(
            values
                .into_iter()
                .filter(|(_, value)| value.len() >= MIN_SCANNED)
                .collect(),
        )
    }

    /// The names of the secrets whose value is in `bytes`, joined for a message.
    pub fn in_bytes(&self, bytes: &[u8]) -> Option<String> {
        let names: Vec<&str> = self
            .0
            .iter()
            .filter(|(_, value)| contains(bytes, value.as_slice()))
            .map(|(name, _)| name.as_str())
            .collect();
        (!names.is_empty()).then(|| names.join(", "))
    }
}

/// The booted deployment's file secrets as its ramfs holds them; None when it declares some but the ramfs holds none of them (before the boot's decrypt step, or in the rescue entry), so nothing can be compared.
pub fn read(root: &Path) -> Result<Option<Values>> {
    let Some(manifest) = Manifest::read(root) else {
        return Ok(Some(Values::default()));
    };
    let names: Vec<&String> = manifest
        .secrets
        .iter()
        .filter(|(_, entry)| entry.kind == Kind::File && !entry.absent)
        .map(|(name, _)| name)
        .collect();
    if names.is_empty() {
        return Ok(Some(Values::default()));
    }
    let dir = root.join(RUN_DIR.trim_start_matches('/'));
    let (mut values, mut found) = (Vec::new(), false);
    for name in names {
        let path = dir.join(name);
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        found = true;
        super::memory::harden(false)?;
        values.push((name.clone(), SecretBuf::read_all(file, MAX_SECRET, name)?));
    }
    Ok(found.then(|| Values::new(values)))
}

/// Each regular file under the /etc at `etc` that holds a value, as ("/etc/…", secret names), but `skip`'s paths (relative to /etc), pacman's keyring and files over MAX_FILE.
pub fn scan_etc(etc: &Path, values: &Values, skip: &BTreeSet<String>) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut stack = vec![String::new()];
    let mut seen = 0;
    while let Some(rel) = stack.pop() {
        let Ok(entries) = fs::read_dir(etc.join(&rel)) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > MAX_ENTRIES {
                return found;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let child = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            if skip.contains(&child) || child == "pacman.d/gnupg" {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(child);
            } else if meta.is_file()
                && meta.len() <= MAX_FILE
                && let Ok(bytes) = fs::read(entry.path())
                && let Some(names) = values.in_bytes(&bytes)
            {
                found.push((format!("/etc/{child}"), names));
            }
        }
    }
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_is_found_in_etc_but_where_skipped_and_short_ones_arent_looked_for() {
        let dir = tempfile::tempdir().unwrap();
        let etc = dir.path();
        fs::create_dir_all(etc.join("NetworkManager/system-connections")).unwrap();
        fs::create_dir_all(etc.join("pacman.d/gnupg")).unwrap();
        fs::create_dir_all(etc.join("wireguard")).unwrap();
        fs::write(
            etc.join("NetworkManager/system-connections/wg0.nmconnection"),
            "private-key=0123456789abcdef-key\n",
        )
        .unwrap();
        fs::write(etc.join("pacman.d/gnupg/x"), "0123456789abcdef-key").unwrap();
        fs::write(etc.join("wireguard/wg0.conf"), "0123456789abcdef-key").unwrap();
        fs::write(etc.join("short"), "1234 and pin-1234").unwrap();
        let values = Values::new(vec![
            ("wg0".into(), SecretBuf::from_slice(b"0123456789abcdef-key").unwrap()),
            ("pin".into(), SecretBuf::from_slice(b"pin-1234").unwrap()),
            ("tiny".into(), SecretBuf::from_slice(b"1234").unwrap()),
        ]);
        let skip: BTreeSet<String> = ["wireguard/wg0.conf".to_string()].into();
        assert_eq!(
            scan_etc(etc, &values, &skip),
            [
                (
                    "/etc/NetworkManager/system-connections/wg0.nmconnection".to_string(),
                    "wg0".to_string()
                ),
                ("/etc/short".to_string(), "pin".to_string()),
            ]
        );
        assert_eq!(values.in_bytes(b"x 1234 x"), None, "under 8 bytes isn't looked for");
    }

    #[test]
    fn values_are_read_from_the_ramfs_for_the_booted_file_secrets_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(
            read(root)
                .unwrap()
                .is_some_and(|v| v.in_bytes(b"anything at all").is_none())
        );
        let entry = |kind: &str| -> super::super::Entry {
            serde_json::from_value(serde_json::json!({
                "file": "x.age", "sha256": "00", "kind": kind, "owner": "root", "group": "root", "mode": 256
            }))
            .unwrap()
        };
        let manifest = Manifest {
            secrets: [
                ("wg0".to_string(), entry("file")),
                ("pw".to_string(), entry("crypt_hash")),
            ]
            .into(),
            ..Manifest::default()
        };
        let at = root.join(super::super::MANIFEST.trim_start_matches('/'));
        fs::create_dir_all(at.parent().unwrap()).unwrap();
        fs::write(&at, super::super::manifest_bytes(&manifest)).unwrap();
        assert!(
            read(root).unwrap().is_none(),
            "declared but not decrypted: nothing to compare"
        );
        let run = root.join(RUN_DIR.trim_start_matches('/'));
        fs::create_dir_all(&run).unwrap();
        fs::write(run.join("wg0"), "0123456789abcdef-key").unwrap();
        fs::write(run.join("pw"), "$6$salt$hash-of-a-password").unwrap();
        let values = read(root).unwrap().unwrap();
        assert_eq!(values.in_bytes(b"k=0123456789abcdef-key").as_deref(), Some("wg0"));
        assert_eq!(
            values.in_bytes(b"$6$salt$hash-of-a-password"),
            None,
            "a crypt hash belongs in shadow"
        );
    }
}

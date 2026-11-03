//! Hibernation is refused while the config's decrypted secrets would go to an unencrypted swap.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// A systemd-sleep drop-in under /run, so it's gone at the next boot whatever happens; "zz-" sorts it after other drop-ins, which it overrides.
pub fn drop_in(root: &Path) -> PathBuf {
    root.join("run/systemd/sleep.conf.d/zz-archix-secrets.conf")
}

/// The unencrypted swaps hibernating would write the decrypted secrets to, unless the config allows hibernation anyway (secrets.allow_hibernation).
pub fn secrets_at_risk(root: &Path) -> Vec<String> {
    match crate::secrets::Manifest::read(root) {
        Some(manifest) if !manifest.secrets.is_empty() && !manifest.allow_hibernation => {
            crate::secrets::storage::unencrypted_swaps(root)
        }
        _ => Vec::new(),
    }
}

/// What a boot or status says when hibernation is off for the secrets.
pub fn secrets_note(swaps: &[String]) -> String {
    format!(
        "hibernation is off while secrets are declared and swap isn't encrypted ({}): it would write every decrypted secret to it; encrypt the swap, or set secrets.allow_hibernation = true",
        swaps.join(", ")
    )
}

/// Refuses hibernate, hybrid-sleep and suspend-then-hibernate while the secrets would go to an unencrypted swap, and allows them again once that's over; suspend stays allowed.
pub fn sync(root: &Path) -> Result<()> {
    let path = drop_in(root);
    let swaps = secrets_at_risk(root);
    if swaps.is_empty() {
        return match fs::remove_file(&path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("removing {}", path.display()))
            }
            _ => Ok(()),
        };
    }
    let text = format!(
        "# archix: {}\n[Sleep]\nAllowHibernation=no\nAllowSuspendThenHibernate=no\nAllowHybridSleep=no\n",
        secrets_note(&swaps)
    );
    if fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
        let dir = path.parent().context("the drop-in has a directory")?;
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, &text).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hibernation_is_refused_while_the_secrets_would_go_to_an_unencrypted_swap() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("etc")).unwrap();
        fs::write(root.join("etc/fstab"), "UUID=archix-test-swap none swap defaults 0 0\n").unwrap();
        sync(root).unwrap();
        assert!(!drop_in(root).exists(), "no secrets, no refusal");
        let manifest_path = root.join(crate::secrets::MANIFEST.trim_start_matches('/'));
        fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
        let entry: crate::secrets::Entry = serde_json::from_value(serde_json::json!({
            "file": "s.age", "sha256": "1", "kind": "file", "owner": "root", "group": "root", "mode": 256
        }))
        .unwrap();
        let mut manifest = crate::secrets::Manifest {
            secrets: [("wg0".to_string(), entry)].into(),
            ..Default::default()
        };
        fs::write(&manifest_path, crate::secrets::manifest_bytes(&manifest)).unwrap();
        sync(root).unwrap();
        let text = fs::read_to_string(drop_in(root)).unwrap();
        assert!(
            text.contains("AllowHibernation=no")
                && !text.contains("AllowSuspend=no")
                && text.contains("UUID=archix-test-swap")
                && text.contains("allow_hibernation"),
            "{text}"
        );
        manifest.allow_hibernation = true;
        fs::write(&manifest_path, crate::secrets::manifest_bytes(&manifest)).unwrap();
        sync(root).unwrap();
        assert!(!drop_in(root).exists(), "the override lets it hibernate");
    }
}

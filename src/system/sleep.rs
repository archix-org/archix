//! Hibernation is refused while a committed deployment waits for the next boot, since resuming would start its kernel and initramfs on an image the running deployment's kernel wrote.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// A systemd-sleep drop-in under /run, so it's gone at the next boot whatever happens; "zz-" sorts it after other drop-ins, which it overrides.
pub fn drop_in(root: &Path) -> PathBuf {
    root.join("run/systemd/sleep.conf.d/zz-archix-pending-deployment.conf")
}

/// The deployment committed but not booted yet, which makes hibernating unsafe.
pub fn pending(root: &Path) -> Option<u32> {
    let current = crate::deploy::Store::open(root).ok()?.current().ok()??;
    (super::boot::booted_deployment(root) != Some(current)).then_some(current)
}

/// Refuses hibernate, hybrid-sleep and suspend-then-hibernate while a deployment is pending and allows them again once none is; suspend stays allowed. Returns the pending deployment.
pub fn sync(root: &Path) -> Result<Option<u32>> {
    let path = drop_in(root);
    let Some(number) = pending(root) else {
        match fs::remove_file(&path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("removing {}", path.display()));
            }
            _ => return Ok(None),
        }
    };
    let text = format!(
        "# archix: deployment {number} boots at the next restart; resuming from hibernation would start its kernel on the running deployment's image, so reboot first\n\
         [Sleep]\nAllowHibernation=no\nAllowSuspendThenHibernate=no\nAllowHybridSleep=no\n"
    );
    if fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
        let dir = path.parent().context("the drop-in has a directory")?;
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, &text).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(Some(number))
}

/// What a switch or rollback says when hibernation is off until the next boot.
pub fn note(number: u32) -> String {
    format!(
        "hibernation is off until you reboot into deployment {number}: resuming would start its kernel on the running system's memory image; suspend still works"
    )
}

/// Syncs the drop-in after a commit and says so; a failure is a warning, as the deployment is committed already.
pub fn after_commit(root: &Path) {
    match sync(root) {
        Ok(Some(number)) => println!("{}", note(number)),
        Ok(None) => {}
        Err(e) => eprintln!(
            "warning: couldn't turn hibernation off until the next boot: {e:#}; don't hibernate before rebooting"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with_current(root: &Path, current: u32) {
        for n in 1..=current {
            for tree in crate::deploy::TREES {
                fs::create_dir_all(root.join("archix/deploy").join(n.to_string()).join(tree)).unwrap();
            }
        }
        let link = root.join("archix/current");
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(format!("deploy/{current}"), link).unwrap();
    }

    fn boot_into(root: &Path, number: u32) {
        fs::create_dir_all(root.join("run/archix")).unwrap();
        fs::write(root.join("run/archix/deployment"), format!("{number}\n")).unwrap();
    }

    #[test]
    fn hibernation_is_refused_only_while_a_committed_deployment_waits_for_the_next_boot() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        store_with_current(root, 1);
        boot_into(root, 1);
        assert_eq!(sync(root).unwrap(), None);
        assert!(!drop_in(root).exists());
        store_with_current(root, 2);
        assert_eq!(sync(root).unwrap(), Some(2));
        let text = fs::read_to_string(drop_in(root)).unwrap();
        for key in [
            "AllowHibernation=no",
            "AllowSuspendThenHibernate=no",
            "AllowHybridSleep=no",
        ] {
            assert!(text.contains(key), "{text}");
        }
        assert!(!text.contains("AllowSuspend=no"), "suspend stays allowed: {text}");
        boot_into(root, 2);
        assert_eq!(sync(root).unwrap(), None);
        assert!(
            !drop_in(root).exists(),
            "booting the deployment allows hibernation again"
        );
        store_with_current(root, 1);
        assert_eq!(
            sync(root).unwrap(),
            Some(1),
            "a rollback away from the booted deployment is pending too"
        );
    }
}

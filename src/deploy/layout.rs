use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use super::commit::rename_noreplace;
use super::view::mount_points;
use super::{Backend, Store};
use crate::system::deploy_boot::TREES;
use crate::testhooks::mark;

const OVERLAYFS_MAGIC: i64 = 0x794c_7630;

/// Whether the system under `root` is laid out as deployments: it has a current one.
pub fn laid_out(root: &Path) -> bool {
    root.join("archix/current").symlink_metadata().is_ok()
}

fn mounted(root: &Path) -> Vec<PathBuf> {
    fs::read("/proc/self/mountinfo")
        .map(|info| mount_points(&info))
        .unwrap_or_default()
        .into_iter()
        .filter(|point| point.starts_with(root))
        .collect()
}

/// Why the system under `root` can't be laid out as deployments that boot from the ESP at `esp`, if it can't.
pub fn unsupported(root: &Path, esp: &Path) -> Option<String> {
    let mounts = mounted(root);
    let is_mount = |rel: &str| mounts.iter().any(|point| *point == root.join(rel));
    if !esp.is_dir() {
        return Some(format!(
            "the ESP at {} doesn't exist; set boot.esp to where it's mounted",
            esp.display()
        ));
    }
    let esp = fs::canonicalize(esp).unwrap_or_else(|_| esp.to_path_buf());
    let boot = fs::canonicalize(root.join("boot")).unwrap_or_else(|_| root.join("boot"));
    if esp.starts_with(&boot) {
        return Some(format!(
            "the ESP is mounted at {}, in /boot, which each deployment keeps its own copy of; mount it at /efi instead (in /etc/fstab, and boot.esp = \"/efi\"), then switch again",
            esp.display()
        ));
    }
    for (tree, why) in [
        ("boot", "each deployment keeps its own /boot"),
        ("usr", "each deployment keeps its own /usr"),
        ("opt", "each deployment keeps its own /opt"),
        ("archix", "the initramfs finds deployments only on the root filesystem"),
    ] {
        if is_mount(tree) {
            return Some(format!(
                "/{tree} is a separate filesystem, and {why} on the root filesystem; archix can't lay this system out as deployments"
            ));
        }
    }
    if let Some(why) = separate_var(root) {
        return Some(why);
    }
    let Ok(c_root) = CString::new(root.as_os_str().as_bytes()) else {
        return Some(format!("{} has a NUL byte", root.display()));
    };
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_root.as_ptr(), &mut stat) } == 0 && stat.f_type as i64 == OVERLAYFS_MAGIC {
        return Some("the root filesystem is overlayfs, as in a container, which can't hold deployments".to_string());
    }
    let static_binary = root.join(crate::integration::STATIC_BINARY.trim_start_matches('/'));
    if !static_binary.is_file() {
        return Some(format!(
            "{} is missing, and the initramfs needs it to mount deployments; install the archix package",
            crate::integration::STATIC_BINARY
        ));
    }
    None
}

/// Why the root at `root` can't boot deployments' pacman databases: /var, /var/lib or /var/lib/pacman is its own filesystem, mounted after the initramfs over what it bound.
pub fn separate_var(root: &Path) -> Option<String> {
    let mounts = mounted(root);
    ["var", "var/lib", "var/lib/pacman"]
        .into_iter()
        .find(|rel| mounts.contains(&root.join(rel)))
        .map(|rel| {
            format!(
                "/{rel} is a separate filesystem, which at boot would cover the pacman database each deployment keeps on the root filesystem; archix can't use deployments on this system"
            )
        })
}

fn cp(args: &[&str], from: &Path, to: &Path) -> Result<()> {
    let output = Command::new("cp")
        .args(args)
        .arg("--no-target-directory")
        .arg(from)
        .arg(to)
        .output()
        .context("running cp")?;
    if !output.status.success() {
        bail!(
            "copying {} to {} failed: {}",
            from.display(),
            to.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Lays the live system under `root` out as deployment 1 and makes it current: hardlinked (btrfs: reflinked) copies of usr, opt and boot, real copies of etc, the pacman database and DKMS state, so the live trees stay whole until `bind_live`; the boot entry is the caller's to write.
pub fn lay_out(root: &Path) -> Result<Store> {
    let archix = root.join("archix");
    fs::create_dir_all(archix.join("deploy")).with_context(|| format!("creating {}", archix.display()))?;
    let store = Store::open(root)?;
    if store.current()?.is_some() {
        bail!("{} is laid out already", root.display());
    }
    let lock = store.lock()?;
    // A layout cut off before `current` was written boots nothing, so it starts over.
    for number in store.deployments()? {
        store.remove_tree(&store.path(number))?;
    }
    store.remove_leftovers()?;
    let staging = store.staging_path(1);
    let (shared, own): (&[&str], &[&str]) = match store.backend() {
        Backend::Snapshot => {
            let output = Command::new("btrfs")
                .args(["-q", "subvolume", "create"])
                .arg(&staging)
                .output()
                .context("running btrfs")?;
            if !output.status.success() {
                bail!(
                    "btrfs couldn't create {}: {}",
                    staging.display(),
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            (&["-a", "--reflink=always"], &["-a", "--reflink=auto"])
        }
        Backend::Overlay => {
            fs::create_dir(&staging).with_context(|| format!("creating {}", staging.display()))?;
            (&["-al"], &["-a"])
        }
    };
    share_usr_local(root, store.backend())?;
    for (tree, at) in TREES {
        let from = root.join(at);
        let to = staging.join(tree);
        if from.is_dir() {
            let mut how = own.to_vec();
            if ["usr", "opt", "boot"].contains(tree) {
                // A filesystem mounted at /usr/local stays where fstab mounts it, and hardlinks can't cross into it.
                how = [shared, &["--one-file-system"]].concat();
            }
            cp(&how, &from, &to)?;
        } else {
            fs::create_dir(&to).with_context(|| format!("creating {}", to.display()))?;
        }
    }
    empty_usr_local(&staging.join("usr"), &staging.join("pacman"))?;
    fs::create_dir(staging.join("var-upper"))
        .with_context(|| format!("creating {}", staging.join("var-upper").display()))?;
    mark("layout-copied");
    crate::durable::sync_filesystems_checked(&[&archix])?;
    rename_noreplace(&staging, &store.path(1))?;
    crate::durable::sync_dir(&store.path(1))?;
    mark("layout-committed");
    store.point_current(1)?;
    drop(lock);
    Ok(store)
}

/// Copies the live /usr/local into the shared /var/usrlocal, which boots bind over each deployment's own: hardlinks on ext4, reflinks on btrfs, entries already there kept, so a layout cut off partway starts over without losing any; a filesystem mounted at /usr/local stays where fstab mounts it, over the bind.
fn share_usr_local(root: &Path, backend: Backend) -> Result<()> {
    let from = root.join("usr/local");
    let to = crate::system::usr_local::dir(root);
    if !from.is_dir() || mounted(root).contains(&from) {
        if !to.is_dir() {
            fs::create_dir(&to).with_context(|| format!("creating {}", to.display()))?;
            fs::set_permissions(&to, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .with_context(|| format!("chmod {}", to.display()))?;
        }
        return Ok(());
    }
    let how: &[&str] = match backend {
        Backend::Snapshot => &["-a", "--reflink=auto", "--update=none"],
        Backend::Overlay => &["-al", "--update=none"],
    };
    cp(how, &from, &to)
}

/// Leaves a staged deployment's usr/local with nothing but the filesystem package's skeleton from pacman database `local_db`: the boot binds the shared /var/usrlocal over it.
fn empty_usr_local(usr: &Path, local_db: &Path) -> Result<()> {
    let local = usr.join("local");
    if !local.is_dir() {
        fs::create_dir(&local).with_context(|| format!("creating {}", local.display()))?;
        return fs::set_permissions(&local, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .with_context(|| format!("chmod {}", local.display()));
    }
    crate::system::usr_local::strip_to_skeleton(&local, &crate::system::usr_local::skeleton(local_db))
}

/// Binds deployment `number`'s trees over the live root, and the shared /var/usrlocal over its /usr/local, as its boot would, and records it as the running one, so the system works on the deployment's files from now on.
pub fn bind_live(root: &Path, store: &Store, number: u32) -> Result<()> {
    crate::system::deploy_boot::bind_trees(root, &store.path(number))
        .with_context(|| format!("mounting deployment {number} over the live system"))?;
    crate::system::deploy_boot::bind_usr_local(root).context("mounting /var/usrlocal over the live /usr/local")?;
    let record = root.join(crate::system::boot::DEPLOYMENT_RECORD.trim_start_matches('/'));
    if let Some(dir) = record.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    fs::write(&record, format!("{number}\n")).with_context(|| format!("writing {}", record.display()))
}

/// Deletes what the live root's own usr, etc, opt, boot, pacman database and DKMS directories held before the first layout, now hidden under a deployment's binds, by running `archix` (`prune-hidden`) in a private mount namespace; nothing is touched unless every tree is bound from a deployment.
pub fn prune_hidden(root: &Path, archix: &Path) -> Result<()> {
    let mut command = Command::new(archix);
    command.arg("prune-hidden").arg(root);
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut command, || {
            if libc::unshare(libc::CLONE_NEWNS) != 0 {
                return Err(io::Error::last_os_error());
            }
            let flags = libc::MS_REC | libc::MS_PRIVATE;
            if libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                flags,
                std::ptr::null(),
            ) != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = command.output().context("running archix prune-hidden")?;
    if !output.status.success() {
        bail!(
            "removing the trees from before the first layout failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn identity(path: &Path) -> Option<(u64, u64)> {
    fs::metadata(path).ok().map(|meta| (meta.dev(), meta.ino()))
}

/// The work of `prune_hidden`, run in a private mount namespace: a non-recursive bind of the root shows the directories under the binds.
pub fn prune_hidden_here(root: &Path) -> Result<usize> {
    let store = &Store::open(root)?;
    let trees: Vec<(u64, u64)> = store
        .deployments()?
        .into_iter()
        .flat_map(|n| {
            TREES
                .iter()
                .filter_map(move |(tree, _)| identity(&store.path(n).join(tree)))
        })
        .collect();
    for (_, at) in TREES {
        if !identity(&root.join(at)).is_some_and(|id| trees.contains(&id)) {
            bail!(
                "{} isn't bound from a deployment, so nothing is pruned",
                root.join(at).display()
            );
        }
    }
    let plain = root.join("archix/.prune");
    let _ = fs::remove_dir(&plain);
    fs::create_dir(&plain).with_context(|| format!("creating {}", plain.display()))?;
    let c = |path: &Path| CString::new(path.as_os_str().as_bytes()).context("path has a NUL byte");
    let (c_root, c_plain) = (c(root)?, c(&plain)?);
    let bound = unsafe {
        libc::mount(
            c_root.as_ptr(),
            c_plain.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    };
    if bound != 0 {
        return Err(io::Error::last_os_error()).with_context(|| format!("binding {}", root.display()));
    }
    let mut removed = 0;
    let result = (|| -> Result<()> {
        for (_, at) in TREES {
            let hidden = plain.join(at);
            match identity(&hidden) {
                Some(id) if !trees.contains(&id) => {}
                _ => continue,
            }
            for entry in fs::read_dir(&hidden).with_context(|| format!("reading {}", hidden.display()))? {
                super::stage::remove_any(&entry.with_context(|| format!("reading {}", hidden.display()))?.path())?;
                removed += 1;
            }
        }
        Ok(())
    })();
    unsafe { libc::umount2(c_plain.as_ptr(), libc::MNT_DETACH) };
    let _ = fs::remove_dir(&plain);
    result.map(|()| removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_root_with_its_esp_in_boot_or_without_the_static_binary_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for sub in ["boot/efi", "efi", "usr/lib/archix"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        let why = unsupported(root, &root.join("boot/efi")).unwrap();
        assert!(why.contains("in /boot"), "{why}");
        let why = unsupported(root, &root.join("boot")).unwrap();
        assert!(why.contains("in /boot"), "{why}");
        let why = unsupported(root, &root.join("nowhere")).unwrap();
        assert!(why.contains("doesn't exist"), "{why}");
        let why = unsupported(root, &root.join("efi")).unwrap();
        assert!(why.contains("archix-static"), "{why}");
        fs::write(root.join("usr/lib/archix/archix-static"), "").unwrap();
        assert_eq!(unsupported(root, &root.join("efi")), None);
    }
}

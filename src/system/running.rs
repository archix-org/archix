//! The running kernel's modules, which pacman deletes when a switch replaces that kernel's package; a copy keeps them loadable until the reboot.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::packages::Packages;
use crate::pacman::LocalPkg;
use crate::paths::Paths;

/// Copies the tree at `from` to `to`, by hardlinks where the two share a mount, else by reflink or in full (/var on a filesystem of its own); leaves nothing at `to` on failure.
fn clone_tree(from: &Path, to: &Path) -> bool {
    for flags in [&["-al"][..], &["-a", "--reflink=auto"][..]] {
        let _ = fs::remove_dir_all(to);
        let copied = Command::new("cp").args(flags).arg(from).arg(to).status();
        if copied.is_ok_and(|status| status.success()) {
            return true;
        }
    }
    let _ = fs::remove_dir_all(to);
    false
}

pub struct Running {
    modules: PathBuf,
    /// The running kernel's release, when the root is the one it runs.
    release: Option<String>,
}

impl Running {
    pub fn new(root: &Path) -> Running {
        let release = (root == Path::new("/"))
            .then(|| fs::read_to_string("/proc/sys/kernel/osrelease").ok())
            .flatten()
            .map(|release| release.trim().to_string());
        Running {
            modules: PathBuf::from(super::boot::MODULES),
            release,
        }
    }

    fn dir(&self) -> Option<PathBuf> {
        self.release.as_ref().map(|release| self.modules.join(release))
    }

    fn package(&self) -> Option<String> {
        let text = fs::read_to_string(self.dir()?.join("pkgbase")).ok()?;
        Some(text.trim().to_string())
    }

    /// On a root laid out for rollbacks, /var/lib/archix is a bind of this directory, and a hardlink can't cross a mount even within one filesystem.
    fn copy(paths: &Paths, release: &str) -> PathBuf {
        Paths::on_root(&paths.root).state.join("kernels").join(release)
    }

    /// Before the transaction: drops copies kept for kernels no longer running, and hardlinks the running kernel's modules aside when `packages` replace or remove its package.
    pub fn keep(&self, paths: &Paths, installed: &BTreeMap<String, LocalPkg>, packages: &Packages) {
        let Some(release) = &self.release else { return };
        self.drop_stale(paths, release);
        let (Some(dir), Some(package)) = (self.dir(), self.package()) else {
            return;
        };
        let Some(have) = installed.get(&package) else { return };
        let changes =
            packages.remove.contains(&package) || packages.versions.get(&package).is_some_and(|v| *v != have.version);
        let copy = Running::copy(paths, release);
        if !changes || copy.exists() {
            return;
        }
        if !self.take_copy(paths, release, &dir) {
            eprintln!(
                "warning: couldn't keep a copy of the running kernel's modules; modules it hasn't loaded yet won't load until you reboot"
            );
        }
    }

    /// Drops the copies of kernels other than `release`, and the modules `restore` put back for them unless their package owns that release again.
    fn drop_stale(&self, paths: &Paths, release: &str) {
        let Some(kernels) = Running::copy(paths, release).parent().map(Path::to_path_buf) else {
            return;
        };
        for entry in fs::read_dir(kernels).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == release {
                continue;
            }
            let _ = fs::remove_dir_all(entry.path());
            let dir = self.modules.join(&name);
            let owner = fs::read_to_string(dir.join("pkgbase"))
                .ok()
                .and_then(|base| super::boot::owned_version(&paths.local_db(), base.trim()));
            if dir.join("pkgbase").exists() && owner.as_deref() != Some(name.as_str()) {
                let _ = fs::remove_dir_all(&dir);
            }
        }
    }

    /// The copy appears whole or not at all, as its presence is what says the running kernel was replaced.
    fn take_copy(&self, paths: &Paths, release: &str, dir: &Path) -> bool {
        let copy = Running::copy(paths, release);
        let _ = fs::create_dir_all(copy.parent().unwrap_or(&copy));
        let tmp = copy.with_file_name(format!(".{release}.new"));
        clone_tree(dir, &tmp) && crate::durable::rename(&tmp, &copy).is_ok()
    }

    /// After the transaction: puts the running kernel's modules back if pacman removed them.
    pub fn restore(&self, paths: &Paths) {
        let (Some(release), Some(dir)) = (&self.release, self.dir()) else {
            return;
        };
        let copy = Running::copy(paths, release);
        if dir.join("modules.dep").exists() || !copy.exists() {
            return;
        }
        let tmp = self.modules.join(format!(".{release}.restoring"));
        let _ = fs::remove_dir_all(&tmp);
        let restored = clone_tree(&copy, &tmp) && crate::durable::rename(&tmp, &dir).is_ok();
        match restored {
            true => eprintln!(":: kept the running kernel's modules, so they still load until you reboot"),
            false => eprintln!(
                "warning: couldn't put the running kernel's modules back; modules it hasn't loaded yet won't load until you reboot"
            ),
        }
    }

    /// Whether a switch since this boot replaced or removed the running kernel's package, so only a reboot runs the new one.
    pub fn replaced(&self, paths: &Paths) -> bool {
        self.release
            .as_ref()
            .is_some_and(|release| Running::copy(paths, release).exists())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running(root: &Path, release: &str) -> Running {
        Running {
            modules: root.join("usr/lib/modules"),
            release: Some(release.to_string()),
        }
    }

    fn kernel_dir(root: &Path, release: &str, base: &str) -> PathBuf {
        let dir = root.join("usr/lib/modules").join(release);
        fs::create_dir_all(dir.join("kernel")).unwrap();
        fs::write(dir.join("pkgbase"), format!("{base}\n")).unwrap();
        fs::write(dir.join("modules.dep"), "").unwrap();
        fs::write(dir.join("kernel/a.ko"), "module").unwrap();
        dir
    }

    fn owns(paths: &Paths, base: &str, release: &str) {
        let entry = paths.local_db().join(format!("{base}-1.0-1"));
        fs::create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("files"),
            format!("%FILES%\nusr/lib/modules/{release}/vmlinuz\n"),
        )
        .unwrap();
    }

    // /var/lib/archix is a bind of /var/rollback/archix there, and a hardlink can't cross a mount even within one filesystem.
    #[test]
    fn a_root_laid_out_for_rollbacks_keeps_the_copy_on_the_roots_own_mount() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        assert_eq!(Running::copy(&paths, "6.1"), paths.state.join("kernels/6.1"));
        fs::create_dir_all(root.path().join("var/rollback/archix")).unwrap();
        assert_eq!(
            Running::copy(&paths, "6.1"),
            root.path().join("var/rollback/archix/kernels/6.1")
        );
    }

    #[test]
    fn a_copy_that_fails_leaves_nothing_a_later_restore_would_take_for_the_modules() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        let kernel = running(root.path(), "6.1");
        assert!(!kernel.take_copy(&paths, "6.1", &root.path().join("gone")));
        assert!(!Running::copy(&paths, "6.1").exists());
        assert!(!kernel.replaced(&paths));
    }

    #[test]
    fn a_kept_kernel_comes_back_after_pacman_removes_it() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        let dir = kernel_dir(root.path(), "6.1", "linux");
        let kernel = running(root.path(), "6.1");
        assert!(kernel.take_copy(&paths, "6.1", &dir));
        fs::remove_dir_all(&dir).unwrap();
        kernel.restore(&paths);
        assert_eq!(fs::read_to_string(dir.join("kernel/a.ko")).unwrap(), "module");
        assert!(kernel.replaced(&paths));
    }

    #[test]
    fn once_another_kernel_runs_a_kept_kernels_copy_and_modules_go_unless_its_package_owns_them() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        let stale = kernel_dir(root.path(), "6.1", "linux");
        let reinstalled = kernel_dir(root.path(), "6.2", "linux-lts");
        let current = kernel_dir(root.path(), "6.3", "linux");
        owns(&paths, "linux", "6.3");
        owns(&paths, "linux-lts", "6.2");
        for (release, dir) in [("6.1", &stale), ("6.2", &reinstalled)] {
            assert!(running(root.path(), release).take_copy(&paths, release, dir));
        }
        running(root.path(), "6.3").drop_stale(&paths, "6.3");
        assert!(!stale.exists(), "nothing owns the kept kernel's modules any more");
        assert!(reinstalled.exists(), "a package owns these again");
        assert!(current.exists());
        assert!(!Running::copy(&paths, "6.1").exists() && !Running::copy(&paths, "6.2").exists());
    }
}

//! The running kernel's modules, which pacman deletes when a switch replaces that kernel's package; a copy keeps them loadable until the reboot.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::packages::Packages;
use crate::pacman::LocalPkg;
use crate::paths::Paths;

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

    fn copy(paths: &Paths, release: &str) -> PathBuf {
        paths.state.join("kernels").join(release)
    }

    /// Before the transaction: drops copies kept for kernels no longer running, and hardlinks the running kernel's modules aside when `packages` replace or remove its package.
    pub fn keep(&self, paths: &Paths, installed: &BTreeMap<String, LocalPkg>, packages: &Packages) {
        let Some(release) = &self.release else { return };
        for entry in fs::read_dir(paths.state.join("kernels"))
            .into_iter()
            .flatten()
            .flatten()
        {
            if entry.file_name().to_string_lossy() != release.as_str() {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
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
        let _ = fs::create_dir_all(copy.parent().unwrap_or(&copy));
        let copied = Command::new("cp").arg("-al").arg(&dir).arg(&copy).status();
        if !copied.is_ok_and(|status| status.success()) {
            eprintln!(
                "warning: couldn't keep a copy of the running kernel's modules; modules it hasn't loaded yet won't load until you reboot"
            );
        }
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
        let restored = Command::new("cp")
            .arg("-al")
            .arg(&copy)
            .arg(&tmp)
            .status()
            .is_ok_and(|status| status.success())
            && fs::rename(&tmp, &dir).is_ok();
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

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;

use super::boot::{MODULES, preserve_from, restore_into};
use crate::journal::Transaction;
use crate::pacman::LocalPkg;
use crate::paths::Paths;

/// The running kernel's modules directory, which pacman deletes when it replaces that kernel's package.
pub struct Running {
    modules: PathBuf,
    release: Option<String>,
}

impl Running {
    pub fn new() -> Running {
        let release = fs::read_to_string("/proc/sys/kernel/osrelease")
            .ok()
            .map(|r| r.trim().to_string());
        Running {
            modules: PathBuf::from(MODULES),
            release,
        }
    }

    fn dir(&self) -> Option<PathBuf> {
        self.release.as_ref().map(|release| self.modules.join(release))
    }

    /// Keeps a hardlinked copy of the running kernel's modules when `tx` replaces or removes its package.
    pub fn keep(
        &self,
        paths: &Paths,
        tx: &Transaction,
        installed: &std::collections::BTreeMap<String, LocalPkg>,
    ) -> Result<()> {
        let (Some(release), Some(dir)) = (&self.release, self.dir()) else {
            return Ok(());
        };
        let Ok(package) = fs::read_to_string(dir.join("pkgbase")) else {
            return Ok(());
        };
        let package = package.trim();
        let changes = installed
            .get(package)
            .is_some_and(|have| tx.versions.get(package) != Some(&have.version));
        if changes {
            preserve_from(&self.modules, paths, release)?;
        }
        Ok(())
    }

    /// Puts the running kernel's modules back if pacman removed them, and remembers to clean them up after a reboot; true if it did.
    pub fn restore(&self, paths: &Paths) -> Result<bool> {
        let Some(release) = &self.release else {
            return Ok(false);
        };
        if !restore_into(&self.modules, paths, release)? {
            return Ok(false);
        }
        let mut restored = restored(paths);
        restored.insert(release.clone());
        save(paths, &restored)?;
        Ok(true)
    }

    /// Whether the running kernel's package was replaced or removed, so only a reboot finishes the switch.
    pub fn replaced(&self) -> bool {
        let Some(dir) = self.dir() else { return false };
        dir.join("pkgbase").is_file() && !owned(&dir.join("pkgbase"))
    }

    /// Deletes module directories archix put back for a kernel that's no longer running, unless a package owns them again.
    pub fn clean_up(&self, paths: &Paths) -> Result<()> {
        let mut restored = restored(paths);
        let before = restored.len();
        restored.retain(|release| {
            let dir = self.modules.join(release);
            if owned(&dir.join("pkgbase")) {
                return false;
            }
            if self.release.as_ref() == Some(release) {
                return true;
            }
            let _ = fs::remove_dir_all(&dir);
            false
        });
        if restored.len() != before {
            save(paths, &restored)?;
        }
        Ok(())
    }
}

impl Default for Running {
    fn default() -> Self {
        Running::new()
    }
}

fn list(paths: &Paths) -> PathBuf {
    paths.state.join("restored-modules")
}

fn restored(paths: &Paths) -> BTreeSet<String> {
    fs::read_to_string(list(paths))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn save(paths: &Paths, restored: &BTreeSet<String>) -> Result<()> {
    if restored.is_empty() {
        let _ = fs::remove_file(list(paths));
        return Ok(());
    }
    fs::create_dir_all(&paths.state)?;
    let text: String = restored.iter().map(|r| format!("{r}\n")).collect();
    fs::write(list(paths), text)?;
    Ok(())
}

fn owned(path: &Path) -> bool {
    Command::new("pacman")
        .arg("-Qqo")
        .arg(path)
        .output()
        .is_ok_and(|o| o.status.success())
}

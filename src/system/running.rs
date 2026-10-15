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
            let mut kept = kept(paths);
            kept.insert(release.clone());
            save(paths, &kept)?;
        }
        Ok(())
    }

    /// Puts the running kernel's modules back if pacman removed them; true if it did.
    pub fn restore(&self, paths: &Paths) -> Result<bool> {
        let Some(release) = &self.release else {
            return Ok(false);
        };
        restore_into(&self.modules, paths, release)
    }

    /// Whether the running kernel's package was replaced or removed, so only a reboot finishes the switch.
    pub fn replaced(&self) -> bool {
        let Some(dir) = self.dir() else { return false };
        dir.join("pkgbase").is_file() && !owned(&dir.join("pkgbase"))
    }

    /// For kernels archix kept that are no longer running: deletes the modules directory it put back, unless a package owns it again, and its copy, unless a boot entry uses it.
    pub fn clean_up(&self, paths: &Paths) -> Result<()> {
        let mut kept = kept(paths);
        let before = kept.len();
        let booted = boot_entry_kernels(paths);
        kept.retain(|release| {
            if self.release.as_ref() == Some(release) {
                return true;
            }
            let dir = self.modules.join(release);
            if dir.exists() && !owned(&dir.join("pkgbase")) {
                let _ = fs::remove_dir_all(&dir);
            }
            if !booted.contains(release) {
                let _ = fs::remove_dir_all(paths.state.join("kernels").join(release));
            }
            false
        });
        if kept.len() != before {
            save(paths, &kept)?;
        }
        Ok(())
    }
}

impl Default for Running {
    fn default() -> Self {
        Running::new()
    }
}

/// Kernels whose modules archix kept while they were running.
fn list(paths: &Paths) -> PathBuf {
    paths.state.join("kept-modules")
}

/// The kernel versions boot entries were built with, whose kept modules they still need.
fn boot_entry_kernels(paths: &Paths) -> BTreeSet<String> {
    fs::read_dir(paths.state.join("boot"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".kernel"))
        .filter_map(|entry| fs::read_to_string(entry.path()).ok())
        .map(|release| release.trim().to_string())
        .collect()
}

fn kept(paths: &Paths) -> BTreeSet<String> {
    fs::read_to_string(list(paths))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn save(paths: &Paths, kept: &BTreeSet<String>) -> Result<()> {
    if kept.is_empty() {
        let _ = fs::remove_file(list(paths));
        return Ok(());
    }
    fs::create_dir_all(&paths.state)?;
    let text: String = kept.iter().map(|r| format!("{r}\n")).collect();
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

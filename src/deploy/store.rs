use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

const BTRFS_MAGIC: u64 = 0x9123_683e;
/// Every btrfs subvolume's top directory has this inode number.
const SUBVOLUME_INODE: u64 = 256;
const STAGING_PREFIX: &str = ".staging-";
const CURRENT_TMP: &str = ".current.tmp";

/// How a deployment is staged from the current one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// btrfs: each deployment is a subvolume, staged as a snapshot.
    Snapshot,
    /// Anything else: copies of etc, pacman and dkms, overlays flattened into hardlinks for the rest.
    Overlay,
}

impl Backend {
    pub fn detect(path: &Path) -> Result<Backend> {
        let c_path = CString::new(path.as_os_str().as_bytes())?;
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(c_path.as_ptr(), &mut stat) } != 0 {
            return Err(io::Error::last_os_error()).with_context(|| format!("statfs {}", path.display()));
        }
        Ok(if stat.f_type as u64 == BTRFS_MAGIC {
            Backend::Snapshot
        } else {
            Backend::Overlay
        })
    }
}

/// The deployments under `<root>/archix`.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
    dir: PathBuf,
    backend: Backend,
}

impl Store {
    /// The store of the system whose real root, holding archix/, var/ and the mountpoints, is `root`.
    pub fn open(root: &Path) -> Result<Store> {
        let dir = root.join("archix");
        let backend = Backend::detect(&dir)?;
        Ok(Store {
            root: root.to_path_buf(),
            dir,
            backend,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    pub fn deploy_dir(&self) -> PathBuf {
        self.dir.join("deploy")
    }

    pub fn path(&self, number: u32) -> PathBuf {
        self.deploy_dir().join(number.to_string())
    }

    pub fn staging_path(&self, number: u32) -> PathBuf {
        self.deploy_dir().join(format!("{STAGING_PREFIX}{number}"))
    }

    /// Committed deployments, oldest first.
    pub fn deployments(&self) -> Result<Vec<u32>> {
        let mut numbers: Vec<u32> = self
            .names()?
            .iter()
            .filter_map(|name| committed_number(name))
            .filter(|n| self.path(*n).is_dir())
            .collect();
        numbers.sort_unstable();
        Ok(numbers)
    }

    /// The deployment `current` names, which boots when no entry picks another.
    pub fn current(&self) -> Result<Option<u32>> {
        match fs::read_link(self.dir.join("current")) {
            Ok(target) => match parse_current(&target) {
                Some(n) => Ok(Some(n)),
                None => bail!(
                    "{} points to {}, not deploy/<number>",
                    self.dir.join("current").display(),
                    target.display()
                ),
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.dir.join("current").display())),
        }
    }

    pub fn next_number(&self) -> Result<u32> {
        Ok(next_number(self.names()?.iter().map(String::as_str)))
    }

    /// Deletes staging trees and a half-made `current` link a crash left behind, none of which was ever committed; returns their names.
    pub fn remove_leftovers(&self) -> Result<Vec<String>> {
        let mut removed = Vec::new();
        for name in self.names()? {
            if name.starts_with(STAGING_PREFIX) {
                self.remove_tree(&self.deploy_dir().join(&name))?;
                removed.push(name);
            }
        }
        let tmp = self.dir.join(CURRENT_TMP);
        if tmp.symlink_metadata().is_ok() {
            fs::remove_file(&tmp).with_context(|| format!("removing {}", tmp.display()))?;
            removed.push(CURRENT_TMP.to_string());
        }
        Ok(removed)
    }

    /// Deletes a committed deployment; never the current one.
    pub fn delete(&self, number: u32) -> Result<()> {
        if self.current()? == Some(number) {
            bail!("deployment {number} is current; it can't be deleted");
        }
        let path = self.path(number);
        if path.symlink_metadata().is_err() {
            return Ok(());
        }
        self.remove_tree(&path)
    }

    /// Points `current` at a committed deployment with an atomic rename, flushed to disk.
    pub fn point_current(&self, number: u32) -> Result<()> {
        if !self.deployments()?.contains(&number) {
            bail!("deployment {number} isn't committed");
        }
        let tmp = self.dir.join(CURRENT_TMP);
        let _ = fs::remove_file(&tmp);
        std::os::unix::fs::symlink(format!("deploy/{number}"), &tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        let current = self.dir.join("current");
        fs::rename(&tmp, &current).with_context(|| format!("replacing {}", current.display()))?;
        crate::durable::sync_dir(&current)
    }

    fn names(&self) -> Result<Vec<String>> {
        let dir = self.deploy_dir();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
        };
        Ok(entries
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect())
    }

    fn remove_tree(&self, path: &Path) -> Result<()> {
        let subvolume = self.backend == Backend::Snapshot
            && fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir() && meta.ino() == SUBVOLUME_INODE);
        if subvolume {
            let output = Command::new("btrfs")
                .args(["subvolume", "delete"])
                .arg(path)
                .output()
                .context("running btrfs")?;
            if !output.status.success() {
                bail!(
                    "btrfs could not delete {}: {}",
                    path.display(),
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            return Ok(());
        }
        fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))
    }
}

fn committed_number(name: &str) -> Option<u32> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) || name.starts_with('0') {
        return None;
    }
    name.parse().ok()
}

/// The deployment a `current` link target names: deploy/<N>, relative.
pub fn parse_current(target: &Path) -> Option<u32> {
    let name = target.to_str()?.strip_prefix("deploy/")?;
    committed_number(name)
}

/// One past the highest committed or staged number among `names`, so a leftover staging tree's number isn't handed out while it's still there.
pub fn next_number<'a>(names: impl Iterator<Item = &'a str>) -> u32 {
    names
        .filter_map(|name| committed_number(name.strip_prefix(STAGING_PREFIX).unwrap_or(name)))
        .max()
        .unwrap_or(0)
        + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &Path) -> Store {
        Store {
            root: dir.to_path_buf(),
            dir: dir.join("archix"),
            backend: Backend::Overlay,
        }
    }

    #[test]
    fn next_number_counts_committed_and_staged_trees_and_ignores_the_rest() {
        assert_eq!(next_number([].into_iter()), 1);
        assert_eq!(next_number(["1", "2", "7", "notes", "07x", ""].into_iter()), 8);
        assert_eq!(next_number(["3", ".staging-9"].into_iter()), 10);
        assert_eq!(next_number(["3", ".staging-x", "010"].into_iter()), 4);
    }

    #[test]
    fn current_must_be_a_relative_link_into_deploy() {
        assert_eq!(parse_current(Path::new("deploy/12")), Some(12));
        assert_eq!(parse_current(Path::new("/archix/deploy/12")), None);
        assert_eq!(parse_current(Path::new("deploy/.staging-12")), None);
        assert_eq!(parse_current(Path::new("deploy/12/usr")), None);
    }

    #[test]
    fn leftovers_go_and_committed_deployments_stay() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        for name in ["1", "2", ".staging-3", ".staging-4"] {
            fs::create_dir_all(store.deploy_dir().join(name).join("usr/lib")).unwrap();
        }
        fs::write(store.deploy_dir().join(".staging-3/usr/lib/file"), "x").unwrap();
        std::os::unix::fs::symlink("deploy/2", store.dir().join("current")).unwrap();
        std::os::unix::fs::symlink("deploy/3", store.dir().join(".current.tmp")).unwrap();
        let mut removed = store.remove_leftovers().unwrap();
        removed.sort();
        assert_eq!(removed, vec![".current.tmp", ".staging-3", ".staging-4"]);
        assert_eq!(store.deployments().unwrap(), vec![1, 2]);
        assert_eq!(store.current().unwrap(), Some(2));
        assert_eq!(store.next_number().unwrap(), 3);
        assert!(store.remove_leftovers().unwrap().is_empty());
    }

    #[test]
    fn current_moves_only_to_committed_deployments_and_is_never_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        for name in ["1", "2", ".staging-3"] {
            fs::create_dir_all(store.deploy_dir().join(name)).unwrap();
        }
        assert_eq!(store.current().unwrap(), None);
        store.point_current(2).unwrap();
        assert_eq!(
            fs::read_link(store.dir().join("current")).unwrap(),
            Path::new("deploy/2")
        );
        assert!(store.point_current(3).is_err());
        assert!(store.delete(2).is_err());
        store.delete(1).unwrap();
        assert_eq!(store.deployments().unwrap(), vec![2]);
        store.delete(1).unwrap();
    }
}

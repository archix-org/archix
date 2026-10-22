use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use super::view::VIEW_DIR;
use super::{Keep, TREES};
use crate::testhooks::mark;

const BTRFS_MAGIC: u64 = 0x9123_683e;
/// Every btrfs subvolume's top directory has this inode number.
const SUBVOLUME_INODE: u64 = 256;
pub(super) const STAGING_PREFIX: &str = ".staging-";
const DELETING_PREFIX: &str = ".deleting-";
const CURRENT_TMP: &str = ".current.tmp";
const LOCK: &str = ".lock";
const CHOSEN: &str = "chosen";

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

    /// The staging root that builds a committed deployment's boot entry or an AUR package, never committed.
    pub fn scratch_path(&self) -> PathBuf {
        self.deploy_dir().join(format!("{STAGING_PREFIX}scratch"))
    }

    /// Committed deployments that are complete, oldest first.
    pub fn deployments(&self) -> Result<Vec<u32>> {
        let mut numbers: Vec<u32> = self
            .names()?
            .iter()
            .filter_map(|name| committed_number(name))
            .filter(|n| incomplete(&self.path(*n)).is_none())
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

    /// The deployment the last switch or rollback chose, which gc keeps whatever entry booted since.
    pub fn chosen(&self) -> Result<Option<u32>> {
        match fs::read_to_string(self.dir.join(CHOSEN)) {
            Ok(text) => Ok(text.trim().parse().ok()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.dir.join(CHOSEN).display())),
        }
    }

    /// Records that a switch or rollback chose deployment `number`.
    pub fn choose(&self, number: u32) -> Result<()> {
        crate::durable::write(&self.dir.join(CHOSEN), format!("{number}\n").as_bytes())
    }

    pub fn next_number(&self) -> Result<u32> {
        Ok(next_number(self.names()?.iter().map(String::as_str)))
    }

    /// Takes the store's lock, which staging holds until its commit, so two switches never share or sweep each other's staging trees.
    pub fn lock(&self) -> Result<StoreLock> {
        let path = self.dir.join(LOCK);
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                bail!(
                    "another archix process is staging a deployment in {}",
                    self.dir.display()
                );
            }
            return Err(err).with_context(|| format!("locking {}", path.display()));
        }
        Ok(StoreLock { _file: file })
    }

    /// Deletes staging trees, deletions and a half-made `current` link a crash left behind, none of which is a committed deployment; returns their names.
    pub fn remove_leftovers(&self) -> Result<Vec<String>> {
        let mut removed = Vec::new();
        for name in self.names()? {
            if name.starts_with(STAGING_PREFIX) || name.starts_with(DELETING_PREFIX) {
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

    /// Deletes a committed deployment, never the current, booted or boot-default one; it's renamed out of sight first, so a crash leaves a leftover, not a torn deployment.
    pub fn delete(&self, number: u32, pins: &Keep) -> Result<()> {
        if self.current()? == Some(number) || pins.current == Some(number) {
            bail!("deployment {number} is current; it can't be deleted");
        }
        if pins.booted == Some(number) {
            bail!("deployment {number} is the one running; it can't be deleted");
        }
        if pins.boot_default == Some(number) {
            bail!("deployment {number} is the boot default; it can't be deleted");
        }
        if pins.fallbacks.contains(&number) {
            bail!("deployment {number} is what the fallback UKI boots; it can't be deleted");
        }
        if pins.chosen == Some(number) {
            bail!("deployment {number} is the one the last switch or rollback chose; it can't be deleted");
        }
        let path = self.path(number);
        if path.symlink_metadata().is_err() {
            return Ok(());
        }
        let hidden = self.deploy_dir().join(format!("{DELETING_PREFIX}{number}"));
        if hidden.symlink_metadata().is_ok() {
            self.remove_tree(&hidden)?;
        }
        fs::rename(&path, &hidden).with_context(|| format!("renaming {} to {}", path.display(), hidden.display()))?;
        crate::durable::sync_dir(&hidden)?;
        mark("deploy-delete-renamed");
        if self.is_subvolume(&hidden) {
            return self.remove_tree(&hidden);
        }
        let mut entries: Vec<PathBuf> = fs::read_dir(&hidden)
            .with_context(|| format!("reading {}", hidden.display()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<io::Result<_>>()?;
        entries.sort();
        for (i, entry) in entries.iter().enumerate() {
            super::stage::remove_any(entry)?;
            if i == 0 {
                mark("deploy-delete-partway");
            }
        }
        fs::remove_dir(&hidden).with_context(|| format!("removing {}", hidden.display()))
    }

    /// Renames deployment `number`, whose commit failed and which `current` doesn't name, to a leftover the next staging deletes.
    pub fn abandon(&self, number: u32) -> Result<()> {
        if self.current()? == Some(number) {
            bail!("deployment {number} is current; it can't be taken back");
        }
        let path = self.path(number);
        let hidden = self.deploy_dir().join(format!("{DELETING_PREFIX}{number}"));
        if hidden.symlink_metadata().is_ok() {
            self.remove_tree(&hidden)?;
        }
        fs::rename(&path, &hidden).with_context(|| format!("renaming {} to {}", path.display(), hidden.display()))?;
        crate::durable::sync_dir(&hidden)?;
        if let Err(e) = self.remove_tree(&hidden) {
            eprintln!(
                "warning: couldn't remove {}: {e:#}; the next switch does",
                hidden.display()
            );
        }
        Ok(())
    }

    /// Points `current` at a complete committed deployment with an atomic rename, flushed to disk.
    pub fn point_current(&self, number: u32) -> Result<()> {
        if let Some(why) = incomplete(&self.path(number)) {
            bail!("deployment {number} can't be current: {why}");
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

    fn is_subvolume(&self, path: &Path) -> bool {
        self.backend == Backend::Snapshot
            && fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir() && meta.ino() == SUBVOLUME_INODE)
    }

    pub(crate) fn remove_tree(&self, path: &Path) -> Result<()> {
        if self.is_subvolume(path) {
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

/// Held while a deployment is staged and committed; released when dropped or when the process dies.
#[derive(Debug)]
pub struct StoreLock {
    _file: fs::File,
}

/// Why `dir` isn't a complete deployment: a tree missing or staging scratch left in it.
pub fn incomplete(dir: &Path) -> Option<String> {
    for tree in TREES {
        if !dir.join(tree).is_dir() {
            return Some(format!("{} has no {tree}/", dir.display()));
        }
    }
    for scratch in [
        ".upper",
        ".work",
        VIEW_DIR,
        super::stage::VAR_LOWER,
        super::stage::VAR_BASE,
    ] {
        if dir.join(scratch).symlink_metadata().is_ok() {
            return Some(format!("{} still has {scratch}", dir.display()));
        }
    }
    None
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

/// One past the highest committed, staged or deleted number among `names`, so a leftover's number isn't handed out while it's still there.
pub fn next_number<'a>(names: impl Iterator<Item = &'a str>) -> u32 {
    names
        .filter_map(|name| {
            let bare = [STAGING_PREFIX, DELETING_PREFIX]
                .iter()
                .find_map(|prefix| name.strip_prefix(prefix))
                .unwrap_or(name);
            committed_number(bare)
        })
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

    fn deployment(store: &Store, name: &str) {
        for tree in TREES {
            fs::create_dir_all(store.deploy_dir().join(name).join(tree)).unwrap();
        }
    }

    #[test]
    fn next_number_counts_committed_staged_and_deleted_trees_and_ignores_the_rest() {
        assert_eq!(next_number([].into_iter()), 1);
        assert_eq!(next_number(["1", "2", "7", "notes", "07x", ""].into_iter()), 8);
        assert_eq!(next_number(["3", ".staging-9"].into_iter()), 10);
        assert_eq!(next_number(["3", ".deleting-6"].into_iter()), 7);
        assert_eq!(next_number(["3", ".staging-x", "010"].into_iter()), 4);
    }

    #[test]
    fn only_complete_deployments_count_or_become_current_and_a_half_deleted_one_is_swept() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        for name in ["1", "2", "3", "4", ".deleting-5"] {
            deployment(&store, name);
        }
        fs::remove_dir(store.path(2).join("etc")).unwrap();
        fs::create_dir(store.path(3).join(".upper")).unwrap();
        fs::remove_dir_all(store.deploy_dir().join(".deleting-5/usr")).unwrap();
        assert_eq!(store.deployments().unwrap(), vec![1, 4]);
        assert!(store.point_current(2).is_err());
        assert!(store.point_current(3).is_err());
        assert!(store.point_current(5).is_err());
        store.point_current(4).unwrap();
        assert_eq!(store.remove_leftovers().unwrap(), vec![".deleting-5"]);
        assert_eq!(store.next_number().unwrap(), 5);
    }

    #[test]
    fn a_deletion_renames_the_deployment_out_of_sight_and_spares_current_booted_and_default() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        for name in ["1", "2", "3", "4", "5"] {
            deployment(&store, name);
        }
        store.point_current(5).unwrap();
        let pins = Keep {
            keep: 1,
            booted: Some(3),
            current: Some(5),
            boot_default: Some(4),
            fallbacks: [1].into(),
            ..Keep::default()
        };
        for pinned in [1, 3, 4, 5] {
            assert!(store.delete(pinned, &pins).is_err(), "{pinned}");
        }
        store.delete(2, &pins).unwrap();
        assert_eq!(store.deployments().unwrap(), vec![1, 3, 4, 5]);
        let mut names: Vec<String> = store.names().unwrap();
        names.sort();
        assert_eq!(names, vec!["1", "3", "4", "5"]);
        store.delete(2, &pins).unwrap();
    }

    #[test]
    fn a_second_lock_on_the_store_is_refused_until_the_first_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        fs::create_dir_all(store.dir()).unwrap();
        let held = store.lock().unwrap();
        assert!(store.lock().is_err());
        drop(held);
        store.lock().unwrap();
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
            deployment(&store, name);
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
            deployment(&store, name);
        }
        assert_eq!(store.current().unwrap(), None);
        store.point_current(2).unwrap();
        assert_eq!(
            fs::read_link(store.dir().join("current")).unwrap(),
            Path::new("deploy/2")
        );
        assert!(store.point_current(3).is_err());
        assert!(store.delete(2, &Keep::default()).is_err());
        store.delete(1, &Keep::default()).unwrap();
        assert_eq!(store.deployments().unwrap(), vec![2]);
        store.delete(1, &Keep::default()).unwrap();
    }
}

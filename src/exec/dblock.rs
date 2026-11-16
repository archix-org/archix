//! SCOPE 2.2 step 3: archix takes pacman's own db.lck with O_EXCL, as libalpm does, and both transactions run with ALPM_TRANS_FLAG_NOLOCK under it.
use std::fs::{self, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::Refused;

/// The held /var/lib/pacman/db.lck; dropping it unlinks it.
#[derive(Debug)]
pub struct DbLock {
    path: PathBuf,
    held: bool,
}

impl DbLock {
    /// Creates `dbpath`/db.lck exclusively; refuses when it exists, as pacman would.
    pub fn take(dbpath: &Path) -> Result<DbLock> {
        let path = dbpath.join("db.lck");
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o000)
            .custom_flags(libc::O_CLOEXEC)
            .open(&path)
        {
            Ok(_) => Ok(DbLock { path, held: true }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(Refused(stale_instruction(&path)).into()),
            Err(e) => Err(e).with_context(|| format!("creating {}", path.display())),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Unlinks the lock.
    pub fn release(mut self) -> Result<()> {
        self.held = false;
        fs::remove_file(&self.path).with_context(|| format!("removing {}", self.path.display()))
    }
}

impl Drop for DbLock {
    fn drop(&mut self) {
        if self.held {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Holds the plan DBPath's archix.lock while the file lives, so no concurrent diff refetches the dbs the switch installs from.
pub fn hold_plan_dbpath(dir: &Path) -> Result<fs::File> {
    use std::os::fd::AsRawFd;
    let path = dir.join("archix.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("locking {}", path.display()));
    }
    Ok(file)
}

/// Refuses when the plan's sync dbs moved since the fingerprint was taken.
pub fn recheck_sync_dbs(plan_dbpath: &Path, planned: &std::collections::BTreeMap<String, String>) -> Result<()> {
    let repos: Vec<String> = planned.keys().cloned().collect();
    let now = crate::plan::fingerprint::hash_sync_dbs(plan_dbpath, &repos);
    let moved: Vec<&String> = repos.iter().filter(|r| now.get(*r) != planned.get(*r)).collect();
    if !moved.is_empty() {
        return Err(Refused(format!(
            "the sync dbs of {} changed since the plan was made; run the switch again for a new plan; nothing was changed",
            moved.iter().map(|r| r.as_str()).collect::<Vec<_>>().join(", ")
        ))
        .into());
    }
    Ok(())
}

/// SCOPE 5's one exact instruction for a db.lck nothing may hold.
pub fn stale_instruction(path: &Path) -> String {
    format!(
        "{p} exists, so pacman, an AUR helper or another switch is using the package database; if none is running (`pgrep -x pacman` prints nothing), remove it with `rm {p}` and switch again; nothing was changed",
        p = path.display()
    )
}

/// Refuses when the local db moved since `planned` (the plan fingerprint's local_db hash) was taken.
pub fn recheck(dbpath: &Path, planned: &str) -> Result<()> {
    let now = crate::plan::fingerprint::hash_local_db(dbpath)?;
    if now != planned {
        return Err(Refused(
            "the local package db changed since the plan was made (pacman or an AUR helper ran meanwhile); run the switch again for a new plan; nothing was changed".into(),
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alpm::testdb::{TestRoot, pkg};

    #[test]
    fn the_lock_is_exclusive_and_released_on_release_or_drop() {
        let dir = tempfile::tempdir().unwrap();
        let lock = DbLock::take(dir.path()).unwrap();
        assert!(dir.path().join("db.lck").exists());
        let second = DbLock::take(dir.path()).unwrap_err();
        assert!(second.downcast_ref::<Refused>().is_some(), "{second:#}");
        assert!(format!("{second}").contains(&format!("rm {}", dir.path().join("db.lck").display())));
        lock.release().unwrap();
        assert!(!dir.path().join("db.lck").exists());
        {
            let _held = DbLock::take(dir.path()).unwrap();
        }
        assert!(!dir.path().join("db.lck").exists(), "dropping a held lock unlinks it");
    }

    #[test]
    fn a_lock_pacman_holds_refuses_and_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("db.lck"), "").unwrap();
        assert!(DbLock::take(dir.path()).is_err());
        assert!(
            dir.path().join("db.lck").exists(),
            "someone else's lock is never removed"
        );
    }

    #[test]
    fn libalpm_refuses_a_locking_transaction_while_archix_holds_the_lock() {
        let t = TestRoot::new();
        t.install(&[pkg("a", "1-1")]);
        let conf = t.conf(&[]);
        let lock = DbLock::take(&t.real_dbpath()).unwrap();
        let handle = crate::alpm::open(&conf, &t.real_dbpath()).unwrap();
        let err = handle.trans_init(alpm::TransFlag::NONE).unwrap_err();
        assert_eq!(err, alpm::Error::HandleLock);
        handle.trans_init(alpm::TransFlag::NO_LOCK).unwrap();
        drop(handle);
        lock.release().unwrap();
    }

    #[test]
    fn a_moved_local_db_refuses_the_recheck() {
        let t = TestRoot::new();
        t.install(&[pkg("a", "1-1")]);
        let planned = crate::plan::fingerprint::hash_local_db(&t.real_dbpath()).unwrap();
        recheck(&t.real_dbpath(), &planned).unwrap();
        t.install(&[pkg("b", "1-1")]);
        let err = recheck(&t.real_dbpath(), &planned).unwrap_err();
        assert!(err.downcast_ref::<Refused>().is_some());
        assert!(format!("{err}").contains("changed since the plan"), "{err}");
    }
}

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result};

/// Writes via a temp file, fsync and rename, so a power cut leaves either the old or the new contents, never a torn file.
pub fn write(path: &Path, contents: &[u8]) -> Result<()> {
    write_mode(path, contents, None)
}

/// Like `write`, for a program: it gets mode 0755 before the rename, so it never appears without it.
pub fn write_executable(path: &Path, contents: &[u8]) -> Result<()> {
    write_mode(path, contents, Some(0o755))
}

/// Like `write`, for a key only root may read: mode 0600 before anything is written.
pub fn write_secret(path: &Path, contents: &[u8]) -> Result<()> {
    write_mode(path, contents, Some(0o600))
}

fn write_mode(path: &Path, contents: &[u8], mode: Option<u32>) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    removing_on_error(&tmp, || {
        let mut file = File::create(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
        if let Some(mode) = mode {
            file.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        file.write_all(contents)
            .with_context(|| format!("writing {}", tmp.display()))?;
        if !crate::testhooks::broken("data-unsynced") {
            file.sync_all().with_context(|| format!("writing {}", tmp.display()))?;
        }
        fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
    })?;
    sync_renamed(path)
}

/// Flushes a file renamed into place and its directory; on FAT the new name's entry says empty until the file itself is synced again.
pub fn sync_renamed(path: &Path) -> Result<()> {
    if crate::testhooks::broken("renamed-unsynced") {
        return sync_dir(path);
    }
    File::open(path)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("writing {}", path.display()))?;
    sync_dir(path)
}

/// Runs `write`, deleting the temporary file it was making if it fails, so a full disk isn't left fuller.
fn removing_on_error(tmp: &Path, write: impl FnOnce() -> Result<()>) -> Result<()> {
    let result = write();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

/// Copies a file like `write` does, streaming it instead of holding it in memory.
pub fn copy(from: &Path, to: &Path) -> Result<()> {
    if let Some(dir) = to.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut tmp_name = to.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = to.with_file_name(tmp_name);
    removing_on_error(&tmp, || {
        fs::copy(from, &tmp).with_context(|| format!("copying {} to {}", from.display(), tmp.display()))?;
        if !crate::testhooks::broken("data-unsynced") {
            File::open(&tmp)?
                .sync_all()
                .with_context(|| format!("writing {}", tmp.display()))?;
        }
        fs::rename(&tmp, to).with_context(|| format!("writing {}", to.display()))
    })?;
    sync_renamed(to)
}

/// Flushes the directory entry for `path`, which a rename or new symlink only changes in memory.
pub fn sync_dir(path: &Path) -> Result<()> {
    if let Some(dir) = path.parent() {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// Flushes each filesystem holding one of `paths` once, with syncfs and an fsync of the path, which on FAT is what flushes the disk's cache: a global sync also waits on unrelated disks, and a stalled one hangs the switch.
pub fn sync_filesystems<P: AsRef<Path>>(paths: &[P]) {
    let mut seen = Vec::new();
    for path in paths {
        let Ok(dir) = File::open(path.as_ref()) else {
            continue;
        };
        let Ok(meta) = dir.metadata() else { continue };
        if !seen.contains(&meta.dev()) {
            seen.push(meta.dev());
            unsafe { libc::syncfs(std::os::fd::AsRawFd::as_raw_fd(&dir)) };
            let _ = dir.sync_all();
        }
    }
}

/// Like `sync_filesystems`, for a commit: every path must open and every flush succeed, so nothing is renamed after a write the disk lost.
pub fn sync_filesystems_checked<P: AsRef<Path>>(paths: &[P]) -> Result<()> {
    let mut seen = Vec::new();
    for path in paths {
        let path = path.as_ref();
        let dir = File::open(path).with_context(|| format!("opening {} to flush it", path.display()))?;
        let dev = dir
            .metadata()
            .with_context(|| format!("stat {}", path.display()))?
            .dev();
        if seen.contains(&dev) {
            continue;
        }
        seen.push(dev);
        if unsafe { libc::syncfs(std::os::fd::AsRawFd::as_raw_fd(&dir)) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("flushing the filesystem holding {}", path.display()));
        }
        // FAT has no sync_fs, so syncfs writes its buffers but never flushes the disk's cache; its fsync does.
        dir.sync_all()
            .with_context(|| format!("flushing the disk holding {}", path.display()))?;
    }
    Ok(())
}

/// Writes a file in the user's config like `write`, keeping its owner and mode, or giving a new one its directory's owner and 0644, so a run under sudo leaves nothing root-owned there.
pub fn write_config(path: &Path, contents: &[u8]) -> Result<()> {
    let before = fs::symlink_metadata(path)
        .ok()
        .filter(|meta| meta.is_file())
        .map(|meta| (meta.uid(), meta.gid(), meta.mode() & 0o7777));
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    create_config_dirs(dir)?;
    let (uid, gid, mode) = match before {
        Some(owner) => owner,
        None => {
            let dir = fs::metadata(dir)?;
            (dir.uid(), dir.gid(), 0o644)
        }
    };
    let (tmp, mut file) = new_file_beside(path)?;
    removing_on_error(&tmp, || {
        file.write_all(contents)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::os::unix::fs::fchown(&file, Some(uid), Some(gid)).with_context(|| format!("chown {}", tmp.display()))?;
        file.set_permissions(fs::Permissions::from_mode(mode))
            .with_context(|| format!("chmod {}", tmp.display()))?;
        file.sync_all().with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
    })?;
    sync_dir(path)
}

/// A new file beside `path` at a random name, created exclusively so a link someone planted in a user's config directory is never followed.
fn new_file_beside(path: &Path) -> Result<(std::path::PathBuf, File)> {
    use std::os::unix::fs::OpenOptionsExt;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    loop {
        let mut bytes = [0u8; 8];
        if unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), 0) } != bytes.len() as isize {
            return Err(std::io::Error::last_os_error()).context("getrandom");
        }
        let suffix: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let tmp = path.with_file_name(format!(".{name}.{suffix}.tmp"));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)
        {
            Ok(file) => return Ok((tmp, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("writing {}", tmp.display())),
        }
    }
}

/// Creates `dir` and its missing parents owned like the nearest one that exists, 0755 whatever the umask; owners are set without following links.
pub fn create_config_dirs(dir: &Path) -> Result<()> {
    let Some(existing) = dir.ancestors().find(|a| a.exists()) else {
        return Ok(());
    };
    let owner = fs::metadata(existing)?;
    let missing: Vec<&Path> = dir.ancestors().take_while(|a| !a.exists()).collect();
    for created in missing.into_iter().rev() {
        use std::os::unix::fs::OpenOptionsExt;
        fs::create_dir(created).with_context(|| format!("creating {}", created.display()))?;
        let opened = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(created)
            .with_context(|| format!("opening {}", created.display()))?;
        std::os::unix::fs::fchown(&opened, Some(owner.uid()), Some(owner.gid()))
            .with_context(|| format!("chown {}", created.display()))?;
        opened
            .set_permissions(fs::Permissions::from_mode(0o755))
            .with_context(|| format!("chmod {}", created.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_write_or_copy_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("inside"), "x").unwrap();
        assert!(write(&target, b"new").is_err());
        assert!(!dir.path().join("target.tmp").exists(), "write");
        fs::write(dir.path().join("source"), "s").unwrap();
        assert!(copy(&dir.path().join("source"), &target).is_err());
        assert!(!dir.path().join("target.tmp").exists(), "copy");
    }

    #[test]
    fn a_config_write_follows_no_link_its_directorys_owner_planted() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, "secret").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o600)).unwrap();
        let config = dir.path().join("config");
        fs::create_dir(&config).unwrap();
        std::os::unix::fs::symlink(&victim, config.join("archix.lock.tmp")).unwrap();
        write_config(&config.join("archix.lock"), b"lock").unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "secret");
        assert_eq!(fs::metadata(&victim).unwrap().mode() & 0o777, 0o600);
        assert_eq!(fs::read_to_string(config.join("archix.lock")).unwrap(), "lock");
        assert_eq!(fs::metadata(config.join("archix.lock")).unwrap().mode() & 0o777, 0o644);
        write_config(&config.join("hosts/box/installed.lua"), b"x").unwrap();
        assert_eq!(fs::metadata(config.join("hosts")).unwrap().mode() & 0o777, 0o755);
        let mut names: Vec<_> = fs::read_dir(&config)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["archix.lock", "archix.lock.tmp", "hosts"],
            "no temporary file is left"
        );
    }

    #[test]
    fn a_checked_flush_fails_on_a_path_it_cannot_open() {
        let dir = tempfile::tempdir().unwrap();
        sync_filesystems_checked(&[dir.path()]).unwrap();
        let err = sync_filesystems_checked(&[dir.path(), &dir.path().join("missing")]).unwrap_err();
        assert!(format!("{err:#}").contains("missing"), "{err:#}");
    }
}

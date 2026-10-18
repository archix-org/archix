use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result};

/// Writes via a temp file, fsync and rename, so a power cut leaves either the old or the new contents, never a torn file.
pub fn write(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    let mut file = File::create(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
    file.write_all(contents)?;
    file.sync_all()?;
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    sync_dir(path)
}

/// Flushes the directory entry for `path`, which a rename or new symlink only changes in memory.
pub fn sync_dir(path: &Path) -> Result<()> {
    if let Some(dir) = path.parent() {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// Writes a file in the user's config like `write`, keeping its owner and mode, or giving a new one its directory's owner and 0644, so a run under sudo leaves nothing root-owned there.
pub fn write_config(path: &Path, contents: &[u8]) -> Result<()> {
    let before = fs::metadata(path)
        .ok()
        .map(|meta| (meta.uid(), meta.gid(), meta.mode() & 0o7777));
    if let Some(dir) = path.parent() {
        create_config_dirs(dir)?;
    }
    write(path, contents)?;
    let (uid, gid, mode) = match before {
        Some(owner) => owner,
        None => {
            let dir = fs::metadata(path.parent().unwrap_or(Path::new("/")))?;
            (dir.uid(), dir.gid(), 0o644)
        }
    };
    std::os::unix::fs::chown(path, Some(uid), Some(gid)).with_context(|| format!("chown {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).with_context(|| format!("chmod {}", path.display()))
}

/// Creates `dir` and its missing parents owned like the nearest one that exists, 0755 whatever the umask.
pub fn create_config_dirs(dir: &Path) -> Result<()> {
    let Some(existing) = dir.ancestors().find(|a| a.exists()) else {
        return Ok(());
    };
    let owner = fs::metadata(existing)?;
    let missing: Vec<&Path> = dir.ancestors().take_while(|a| !a.exists()).collect();
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    for created in missing {
        std::os::unix::fs::chown(created, Some(owner.uid()), Some(owner.gid()))
            .with_context(|| format!("chown {}", created.display()))?;
        fs::set_permissions(created, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("chmod {}", created.display()))?;
    }
    Ok(())
}

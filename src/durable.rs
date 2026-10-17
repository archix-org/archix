use std::fs::{self, File};
use std::io::Write;
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

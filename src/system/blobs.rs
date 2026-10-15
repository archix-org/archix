use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::paths::Paths;

/// File contents by sha256, shared by every generation that writes them.
fn dir(paths: &Paths) -> PathBuf {
    paths.state.join("blobs")
}

pub fn store(paths: &Paths, blobs: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    let dir = dir(paths);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    for (sha256, content) in blobs {
        let path = dir.join(sha256);
        if path.exists() {
            continue;
        }
        let tmp = dir.join(format!(".{sha256}.{}", std::process::id()));
        fs::write(&tmp, content)?;
        fs::rename(&tmp, &path).with_context(|| format!("storing {}", path.display()))?;
    }
    Ok(())
}

pub fn read(paths: &Paths, sha256: &str) -> Result<Vec<u8>> {
    let path = dir(paths).join(sha256);
    fs::read(&path).with_context(|| format!("reading {}", path.display()))
}

/// Deletes blobs no kept generation writes; returns how many and their size.
pub fn collect(paths: &Paths, keep: &BTreeSet<&str>) -> Result<(usize, u64)> {
    let (mut count, mut bytes) = (0, 0);
    for entry in fs::read_dir(dir(paths)).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !keep.contains(name.as_str()) {
            bytes += entry.metadata().map_or(0, |m| m.len());
            fs::remove_file(entry.path())?;
            count += 1;
        }
    }
    Ok((count, bytes))
}

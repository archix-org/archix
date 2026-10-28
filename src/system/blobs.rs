use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::archive::Archive;
use crate::paths::Paths;
use crate::system::Rendered;

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
        fs::write(&tmp, content).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &path).with_context(|| format!("storing {}", path.display()))?;
    }
    Ok(())
}

pub fn read(paths: &Paths, sha256: &str) -> Result<Vec<u8>> {
    let path = dir(paths).join(sha256);
    fs::read(&path).with_context(|| format!("reading {}", path.display()))
}

pub fn path(paths: &Paths, sha256: &str) -> PathBuf {
    dir(paths).join(sha256)
}

/// Puts every file a rendered system writes into the store: rendered contents, matching files already on disk, then downloads.
pub fn ensure(paths: &Paths, archive: &Archive, rendered: &Rendered) -> Result<()> {
    store(paths, &rendered.blobs)?;
    let dir = dir(paths);
    for (target, file) in &rendered.state.files {
        let path = dir.join(&file.sha256);
        if file.link.is_some() || path.exists() || !Path::new(target).is_file() {
            continue;
        }
        if crate::fetch::sha256(Path::new(target))? == file.sha256 {
            let tmp = dir.join(format!(".{}.{}", file.sha256, std::process::id()));
            fs::copy(target, &tmp).with_context(|| format!("copying {target} to {}", tmp.display()))?;
            fs::rename(&tmp, &path).with_context(|| format!("storing {}", path.display()))?;
        }
    }
    for (sha256, url) in &rendered.fetches {
        let path = dir.join(sha256);
        if path.exists() {
            continue;
        }
        let tmp = dir.join(format!(".{sha256}.fetch"));
        eprintln!("downloading {url}");
        if !archive.fetch(url, &tmp)? {
            bail!("{url} doesn't exist");
        }
        let got = crate::fetch::sha256(&tmp)?;
        if got != *sha256 {
            let _ = fs::remove_file(&tmp);
            bail!("{url} has sha256 {got}, but the config says {sha256}");
        }
        fs::rename(&tmp, &path).with_context(|| format!("storing {}", path.display()))?;
    }
    Ok(())
}

/// Deletes blobs no kept generation writes; returns how many and their size.
pub fn collect(paths: &Paths, keep: &BTreeSet<&str>) -> Result<(usize, u64)> {
    let (mut count, mut bytes) = (0, 0);
    for entry in fs::read_dir(dir(paths)).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !keep.contains(name.as_str()) {
            bytes += entry.metadata().map_or(0, |m| m.len());
            fs::remove_file(entry.path()).with_context(|| format!("removing {}", entry.path().display()))?;
            count += 1;
        }
    }
    Ok((count, bytes))
}

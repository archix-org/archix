use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::generations::Store;
use crate::paths::Paths;

/// What archix 0.1.0-dogfood recorded before a switch rewrote archix.lock or installed.lua; later versions write the config only when a switch commits, and only put back what an old record names.
#[derive(Serialize, Deserialize)]
struct Record {
    files: Vec<Saved>,
    generation: Option<u32>,
}

/// A file's text, owner and mode, or None for text if it didn't exist.
#[derive(Serialize, Deserialize)]
struct Saved {
    path: PathBuf,
    text: Option<String>,
    owner: Option<(u32, u32, u32)>,
}

fn record_path(paths: &Paths) -> PathBuf {
    paths.state.join("pending-config.json")
}

pub fn exists(paths: &Paths) -> bool {
    record_path(paths).exists()
}

/// Puts the remembered files back unless a new generation became current since, and forgets them; returns what it put back. An open journal leaves them to `archix recover`.
pub fn settle(paths: &Paths) -> Result<Vec<PathBuf>> {
    let path = record_path(paths);
    let Ok(bytes) = fs::read(&path) else {
        return Ok(Vec::new());
    };
    if paths.journal().exists() {
        return Ok(Vec::new());
    }
    let mut restored = Vec::new();
    if let Ok(record) = serde_json::from_slice::<Record>(&bytes)
        && Store::new(paths).current()? == record.generation
    {
        for saved in &record.files {
            let file = &saved.path;
            if fs::read_to_string(file).ok() == saved.text {
                continue;
            }
            match &saved.text {
                Some(text) => {
                    crate::durable::write(file, text.as_bytes())
                        .with_context(|| format!("putting back {}", file.display()))?;
                    if let Some((uid, gid, mode)) = saved.owner {
                        std::os::unix::fs::chown(file, Some(uid), Some(gid))?;
                        fs::set_permissions(file, fs::Permissions::from_mode(mode))?;
                    }
                }
                None => fs::remove_file(file).with_context(|| format!("removing {}", file.display()))?,
            }
            restored.push(file.clone());
        }
    }
    fs::remove_file(&path)?;
    crate::durable::sync_dir(&paths.state)?;
    Ok(restored)
}

/// The note for files `settle` put back, as they were before `what`.
pub fn describe(restored: &[PathBuf], what: &str) -> Option<String> {
    let names: Vec<String> = restored.iter().map(|p| p.display().to_string()).collect();
    (!names.is_empty()).then(|| format!("put back {} as they were before {what}", names.join(" and ")))
}

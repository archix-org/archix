use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::generations::Store;
use crate::paths::Paths;

/// Config files a switch may rewrite (archix.lock, installed.lua), as they were before it, and the generation that was current then.
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

/// Remembers `files` before a switch may rewrite them; dropping the guard puts them back unless a new generation became current.
pub fn begin(paths: &Paths, files: &[PathBuf]) -> Result<Guard> {
    settle(paths)?;
    let record = Record {
        files: files
            .iter()
            .map(|file| Saved {
                path: file.clone(),
                text: fs::read_to_string(file).ok(),
                owner: fs::metadata(file)
                    .ok()
                    .map(|meta| (meta.uid(), meta.gid(), meta.mode() & 0o7777)),
            })
            .collect(),
        generation: Store::new(paths).current()?,
    };
    fs::create_dir_all(&paths.state)?;
    crate::durable::write(&record_path(paths), &serde_json::to_vec(&record)?)?;
    Ok(Guard { paths: paths.clone() })
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

pub struct Guard {
    paths: Paths,
}

impl Drop for Guard {
    fn drop(&mut self) {
        match settle(&self.paths) {
            Ok(restored) => {
                if let Some(note) = describe(&restored, "this command") {
                    eprintln!("{note}");
                }
            }
            Err(e) => eprintln!("warning: couldn't put the config back: {e:#}"),
        }
    }
}

/// The files `archix switch`, `install` and `remove` may rewrite under `config_dir`.
pub fn config_files(config_dir: &Path, host: &str, installed_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut files = vec![
        config_dir.join(crate::lock::LOCK_FILE),
        crate::installed::default_dir(config_dir, host).join(crate::installed::FILE),
    ];
    if let Some(dir) = installed_dir {
        files.push(dir.join(crate::installed::FILE));
    }
    files
}

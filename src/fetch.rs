use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::archive::Archive;

const PARALLEL: usize = 5;

/// A package file to have on disk before a transaction starts.
#[derive(Debug, Clone)]
pub struct Wanted {
    pub name: String,
    pub file: String,
    /// Tried in order; the first one that exists wins.
    pub urls: Vec<String>,
    /// None for rollbacks to versions the lock never recorded; pacman's signature check still applies.
    pub sha256: Option<String>,
}

/// Makes every wanted file present in `dir`, reusing earlier downloads and pacman's cache, and verifies checksums.
pub fn fetch_all(archive: &Archive, dir: &Path, also_look_in: &[PathBuf], wanted: &[Wanted]) -> Result<Vec<PathBuf>> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let next = AtomicUsize::new(0);
    let errors = Mutex::new(Vec::new());
    thread::scope(|scope| {
        for _ in 0..PARALLEL.min(wanted.len()) {
            scope.spawn(|| {
                while let Some(item) = wanted.get(next.fetch_add(1, Ordering::Relaxed)) {
                    if let Err(e) = fetch_one(archive, dir, also_look_in, item) {
                        errors.lock().unwrap().push(format!("{}: {e:#}", item.name));
                    }
                }
            });
        }
    });
    let errors = errors.into_inner().unwrap();
    if !errors.is_empty() {
        bail!("could not fetch {} packages:\n  {}", errors.len(), errors.join("\n  "));
    }
    Ok(wanted.iter().map(|item| dir.join(&item.file)).collect())
}

fn fetch_one(archive: &Archive, dir: &Path, also_look_in: &[PathBuf], item: &Wanted) -> Result<()> {
    let dest = dir.join(&item.file);
    if dest.is_file() && matches(&dest, item)? {
        return Ok(());
    }
    for other in also_look_in {
        let candidate = other.join(&item.file);
        if candidate.is_file() && matches(&candidate, item)? {
            fs::copy(&candidate, &dest).with_context(|| format!("copying {}", candidate.display()))?;
            let sig = other.join(format!("{}.sig", item.file));
            if sig.is_file() {
                fs::copy(&sig, dir.join(format!("{}.sig", item.file)))?;
            }
            return Ok(());
        }
    }
    for url in &item.urls {
        eprintln!("downloading {}", item.file);
        if !archive.fetch(url, &dest)? {
            continue;
        }
        if !matches(&dest, item)? {
            fs::remove_file(&dest)?;
            bail!("{url} does not match the sha256 in the lock");
        }
        archive.fetch(&format!("{url}.sig"), &dir.join(format!("{}.sig", item.file)))?;
        return Ok(());
    }
    bail!("{} is not available from {}", item.file, item.urls.join(" or "))
}

fn matches(path: &Path, item: &Wanted) -> Result<bool> {
    let Some(expected) = &item.sha256 else { return Ok(true) };
    Ok(sha256(path)? == *expected)
}

pub fn sha256_bytes(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut buf = vec![0; 1 << 16];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_of_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x");
        fs::write(&path, "abc").unwrap();
        assert_eq!(
            sha256(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn reuses_a_matching_copy_from_another_cache() {
        let dir = tempfile::tempdir().unwrap();
        let (ours, pacman) = (dir.path().join("ours"), dir.path().join("pacman"));
        fs::create_dir_all(&pacman).unwrap();
        fs::write(pacman.join("a-1-1-x86_64.pkg.tar.zst"), "abc").unwrap();
        let wanted = Wanted {
            name: "a".into(),
            file: "a-1-1-x86_64.pkg.tar.zst".into(),
            urls: vec!["http://127.0.0.1:9/unreachable".into()],
            sha256: Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into()),
        };
        let archive = Archive::new("http://127.0.0.1:9");
        let files = fetch_all(&archive, &ours, &[pacman], &[wanted]).unwrap();
        assert_eq!(fs::read_to_string(&files[0]).unwrap(), "abc");
    }
}

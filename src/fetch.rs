use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    let full = AtomicBool::new(false);
    let errors = Mutex::new(Vec::new());
    thread::scope(|scope| {
        for _ in 0..PARALLEL.min(wanted.len()) {
            scope.spawn(|| {
                while let Some(item) = wanted.get(next.fetch_add(1, Ordering::Relaxed)) {
                    if full.load(Ordering::Relaxed) {
                        break;
                    }
                    if let Err(e) = fetch_one(archive, dir, also_look_in, item) {
                        full.fetch_or(is_disk_full(&e), Ordering::Relaxed);
                        errors.lock().unwrap().push(format!("{}: {e:#}", item.name));
                    }
                }
            });
        }
    });
    let errors = errors.into_inner().unwrap();
    if full.into_inner() {
        bail!(
            "{} ran out of space while downloading packages; nothing was changed. Free some space (`archix gc`, `pacman -Sc`) and try again",
            dir.display()
        );
    }
    if !errors.is_empty() {
        bail!("could not fetch {} packages:\n  {}", errors.len(), errors.join("\n  "));
    }
    Ok(wanted.iter().map(|item| dir.join(&item.file)).collect())
}

pub(crate) fn is_disk_full(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::StorageFull)
    })
}

/// Copies through a temporary name, so a copy cut short by a full disk never sits under the real name, where a file without a pinned sha256 would pass as fetched.
fn copy_whole(from: &Path, to: &Path) -> Result<()> {
    let part = to.with_extension(format!("part.{}", std::process::id()));
    let copied = fs::copy(from, &part).and_then(|_| File::open(&part)?.sync_all());
    if let Err(e) = copied {
        let _ = fs::remove_file(&part);
        return Err(e).with_context(|| format!("copying {}", from.display()));
    }
    fs::rename(&part, to).with_context(|| format!("renaming {}", part.display()))
}

/// A hard link to a file another cache holds, since pacman never rewrites a cached package in place, or a copy when they're on different filesystems.
fn link_or_copy(from: &Path, to: &Path) -> Result<()> {
    let part = to.with_extension(format!("part.{}", std::process::id()));
    let _ = fs::remove_file(&part);
    if fs::hard_link(from, &part).is_ok() {
        return fs::rename(&part, to).with_context(|| format!("renaming {}", part.display()));
    }
    copy_whole(from, to)
}

fn fetch_one(archive: &Archive, dir: &Path, also_look_in: &[PathBuf], item: &Wanted) -> Result<()> {
    let dest = dir.join(&item.file);
    if dest.is_file() && matches(&dest, item)? {
        return Ok(());
    }
    for other in also_look_in {
        let candidate = other.join(&item.file);
        if candidate.is_file() && matches(&candidate, item)? {
            link_or_copy(&candidate, &dest)?;
            let sig = other.join(format!("{}.sig", item.file));
            if sig.is_file() {
                link_or_copy(&sig, &dir.join(format!("{}.sig", item.file)))?;
            }
            return Ok(());
        }
    }
    let mut mismatched = Vec::new();
    let caches = archive.cache_urls(&item.file);
    let cached = caches.len();
    let urls: Vec<String> = caches.into_iter().chain(item.urls.iter().cloned()).collect();
    for (index, url) in urls.iter().enumerate() {
        eprintln!("downloading {}", item.file);
        match archive.fetch(url, &dest) {
            Ok(true) => {}
            Ok(false) => continue,
            // A package cache that is down or misbehaving only costs its speedup; the archive is still there.
            Err(e) if index < cached && !is_disk_full(&e) => {
                eprintln!("warning: skipping the package cache at {url}: {e:#}");
                continue;
            }
            Err(e) => return Err(e),
        }
        // Another source may still hold the right file, such as the archive's per-package copy when a dated repo's file differs from its database.
        if !matches(&dest, item)? {
            fs::remove_file(&dest)?;
            mismatched.push(url.as_str());
            continue;
        }
        archive.fetch(&format!("{url}.sig"), &dir.join(format!("{}.sig", item.file)))?;
        return Ok(());
    }
    if !mismatched.is_empty() {
        bail!(
            "{} doesn't match the sha256 the lock took from the repo database at {}; the archive holds a different file than its own database lists",
            item.file,
            mismatched.join(" or ")
        );
    }
    bail!("{} is not available from {}", item.file, urls.join(" or "))
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
    fn a_full_disk_is_told_apart_from_other_failures() {
        let full = anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::StorageFull)).context("downloading x");
        assert!(is_disk_full(&full));
        let refused =
            anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::ConnectionRefused)).context("downloading x");
        assert!(!is_disk_full(&refused));
    }

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
        let files = fetch_all(&archive, &ours, std::slice::from_ref(&pacman), &[wanted]).unwrap();
        assert_eq!(fs::read_to_string(&files[0]).unwrap(), "abc");
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            fs::metadata(&files[0]).unwrap().ino(),
            fs::metadata(pacman.join("a-1-1-x86_64.pkg.tar.zst")).unwrap().ino(),
            "a file pacman's cache already holds is linked, not stored twice"
        );
    }
}

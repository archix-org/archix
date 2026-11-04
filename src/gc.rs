use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::generations::{Store, kept, now};
use crate::paths::Paths;

#[derive(Debug, Default, PartialEq)]
pub struct Report {
    pub generations: Vec<u32>,
    pub files: usize,
    pub bytes: u64,
    pub snapshots: usize,
    /// Retired secrets identities no kept generation needs any more, by recipient.
    pub retired: Vec<String>,
}

/// The package files archix's own cache keeps: every one an official repo doesn't serve from the Arch Linux Archive, for each kept generation, and every one of the newest two, so a revert to yesterday works offline.
pub fn cached_files<'a>(
    kept: &[(u32, &'a std::collections::BTreeMap<String, crate::lock::Locked>)],
) -> BTreeSet<&'a str> {
    let mut newest: Vec<u32> = kept.iter().map(|(number, _)| *number).collect();
    newest.sort_unstable();
    let recent: BTreeSet<u32> = newest.iter().rev().take(RECENT).copied().collect();
    kept.iter()
        .flat_map(|(number, packages)| {
            let all = recent.contains(number);
            packages
                .values()
                .filter(move |pkg| all || pkg.is_built() || !crate::repos::is_official(&pkg.repo))
        })
        .map(|pkg| pkg.file.as_str())
        .collect()
}

/// How many of the newest generations keep every package file, official ones included.
pub const RECENT: usize = 2;

/// How a package file got into archix's own cache, cheapest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kept {
    /// There already, or not in pacman's cache to take.
    Nothing,
    /// Shares its blocks with pacman's copy, which can go on its own.
    Reflinked,
    /// The same inode as pacman's copy, where the filesystem has no reflinks; paccache only unlinks its name.
    Linked,
    /// A full copy, as pacman's cache is on another filesystem.
    Copied,
}

/// Takes `file` from pacman's cache into archix's own without duplicating it where it can: a reflink, else a hard link on the same filesystem, else a copy; paccache never touches archix's cache.
pub fn keep_copy(paths: &Paths, file: &str) -> Result<Kept> {
    let target = paths.packages().join(file);
    let source = paths.pacman_cache.join(file);
    if target.is_file() || !source.is_file() {
        return Ok(Kept::Nothing);
    }
    fs::create_dir_all(paths.packages())?;
    let temp = paths.packages().join(format!(".{file}.part"));
    let _ = fs::remove_file(&temp);
    let cp = |mode: &str| {
        std::process::Command::new("cp")
            .arg(mode)
            .arg(&source)
            .arg(&temp)
            .stderr(std::process::Stdio::null())
            .status()
            .context("running cp")
            .map(|status| status.success())
    };
    let how = if cp("--reflink=always")? {
        Kept::Reflinked
    } else if fs::hard_link(&source, &temp).is_ok() {
        Kept::Linked
    } else if cp("--reflink=never")? {
        Kept::Copied
    } else {
        let _ = fs::remove_file(&temp);
        anyhow::bail!(
            "copying {} into {} failed",
            source.display(),
            paths.packages().display()
        );
    };
    crate::durable::rename(&temp, &target)?;
    Ok(how)
}

/// Moves the package copies a switch kept in `from`, the cache directory, before @archix was mounted, into `to`, @archix's, keeping any `to` has already; returns how many moved. Renames where they can, else copies, as they're on other subvolumes.
pub fn adopt_cache(from: &Path, to: &Path) -> Result<usize> {
    let Ok(entries) = fs::read_dir(from) else {
        return Ok(0);
    };
    fs::create_dir_all(to)?;
    let mut moved = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let text = name.to_string_lossy();
        let (source, target) = (entry.path(), to.join(&name));
        if !text.ends_with(".pkg.tar.zst") && !text.ends_with(".pkg.tar.zst.sig")
            || text.starts_with('.')
            || target.exists()
        {
            continue;
        }
        if crate::durable::rename(&source, &target).is_err() {
            crate::durable::copy(&source, &target)?;
        }
        moved += 1;
    }
    fs::remove_dir_all(from).with_context(|| format!("removing {}", from.display()))?;
    Ok(moved)
}

/// The bytes archix's own package cache holds on disk, each hard-linked file once; reflinked blocks count too, as this cache alone can pin them.
pub fn cache_size(paths: &Paths) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let mut seen = BTreeSet::new();
    fs::read_dir(paths.packages())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .filter(|meta| meta.is_file() && seen.insert((meta.dev(), meta.ino())))
        .map(|meta| meta.blocks() * 512)
        .sum()
}

/// Under the free-space floor: drops the cached packages the Arch Linux Archive can serve again that `current`, the current lock, doesn't name, the first thing a low disk gives up after snapshots; returns the bytes freed.
pub fn drop_refetchable(
    paths: &Paths,
    current: &std::collections::BTreeMap<String, crate::lock::Locked>,
    older: &[&std::collections::BTreeMap<String, crate::lock::Locked>],
) -> Result<u64> {
    let keep: BTreeSet<&str> = current.values().map(|pkg| pkg.file.as_str()).collect();
    let refetchable: BTreeSet<&str> = older
        .iter()
        .flat_map(|packages| packages.values())
        .filter(|pkg| crate::repos::is_official(&pkg.repo) && !pkg.is_built())
        .map(|pkg| pkg.file.as_str())
        .filter(|file| !keep.contains(file))
        .collect();
    let mut freed = 0;
    for file in refetchable {
        let path = paths.packages().join(file);
        if let Ok(meta) = fs::metadata(&path) {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            freed += meta.len();
        }
    }
    Ok(freed)
}

/// Drops generation records outside the generation limits that no snapshot holds, then cached files no kept generation or snapshot needs.
pub fn collect(paths: &Paths, snapshot_cache: &Path) -> Result<Report> {
    let store = Store::new(paths);
    let generations = store.list()?;
    let current = store.current()?;
    let Some(settings) = current
        .and_then(|n| generations.iter().find(|g| g.number == n))
        .map(|g| g.settings.clone())
    else {
        return Ok(Report::default());
    };
    let mut report = Report::default();
    let snapshots = crate::snapshot::Store::open(&paths.root).list().unwrap_or_default();
    let mut keep = kept(&generations, current, settings.keep, settings.keep_days, now());
    keep.extend(snapshots.iter().filter_map(|record| record.generation));
    for generation in &generations {
        if !keep.contains(&generation.number) {
            store.delete(generation.number)?;
            report.generations.push(generation.number);
        }
    }

    let kept_generations: Vec<_> = generations.iter().filter(|g| keep.contains(&g.number)).collect();
    let mut locks: Vec<_> = kept_generations.iter().map(|g| (g.number, &g.packages)).collect();
    locks.extend(snapshots.iter().map(|record| (0, &record.packages)));
    let files = cached_files(&locks);
    for dir in [paths.packages(), paths.cache.join("verified")] {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let package = name.strip_suffix(".sig").unwrap_or(&name);
            if !files.contains(package) {
                report.bytes += entry.metadata().map_or(0, |m| m.len());
                fs::remove_file(entry.path()).with_context(|| format!("removing {}", entry.path().display()))?;
                report.files += 1;
            }
        }
    }

    let blobs: BTreeSet<&str> = kept_generations
        .iter()
        .flat_map(|g| g.system.files.values())
        .map(|file| file.sha256.as_str())
        .collect();
    let (blob_count, blob_bytes) = crate::system::blobs::collect(paths, &blobs)?;
    report.files += blob_count;
    report.bytes += blob_bytes;
    let ciphertexts: Vec<Vec<u8>> = kept_generations
        .iter()
        .filter_map(|g| g.system.secrets.as_ref())
        .flat_map(|manifest| manifest.secrets.values())
        .filter_map(|entry| fs::read(crate::system::blobs::path(paths, &entry.sha256)).ok())
        .collect();
    report.retired = crate::secrets::storage::collect_retired(&paths.state, &ciphertexts);

    let dates: BTreeSet<String> = kept_generations.iter().map(|g| g.date.to_string()).collect();
    // `archix lock` doesn't take archix's lock, so one resolving right now may be reading a snapshot no generation uses yet.
    let fresh = |entry: &fs::DirEntry| {
        entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|modified| modified.elapsed().unwrap_or_default() < std::time::Duration::from_secs(86_400))
    };
    if let Ok(entries) = fs::read_dir(snapshot_cache.join("snapshots")) {
        for entry in entries.flatten() {
            if !dates.contains(entry.file_name().to_string_lossy().as_ref()) && !fresh(&entry) {
                fs::remove_dir_all(entry.path()).with_context(|| format!("removing {}", entry.path().display()))?;
                report.snapshots += 1;
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::Locked;
    use std::collections::BTreeMap;

    fn generation(number: u32, packages: &[(&str, &str)]) -> (u32, BTreeMap<String, Locked>) {
        let mut g = (number, BTreeMap::new());
        for (name, repo) in packages {
            g.1.insert(
                name.to_string(),
                Locked {
                    version: number.to_string(),
                    repo: repo.to_string(),
                    file: format!("{name}-{number}.pkg.tar.zst"),
                    ..Default::default()
                },
            );
        }
        g
    }

    fn cache_paths(dir: &Path) -> Paths {
        let mut paths = Paths::under(dir);
        paths.cache = dir.join("cache");
        paths.pacman_cache = dir.join("pacman");
        fs::create_dir_all(&paths.pacman_cache).unwrap();
        paths
    }

    #[test]
    fn copies_kept_before_archix_had_its_subvolume_move_into_it() {
        let dir = tempfile::tempdir().unwrap();
        let (from, to) = (dir.path().join("cache/pkg"), dir.path().join("archix/packages"));
        fs::create_dir_all(&from).unwrap();
        fs::create_dir_all(&to).unwrap();
        fs::write(from.join("htop-1.pkg.tar.zst"), "old").unwrap();
        fs::write(from.join("jq-1.pkg.tar.zst"), "old").unwrap();
        fs::write(to.join("jq-1.pkg.tar.zst"), "new").unwrap();
        fs::write(from.join(".vim-1.pkg.tar.zst.part"), "torn").unwrap();
        assert_eq!(adopt_cache(&from, &to).unwrap(), 1);
        assert_eq!(fs::read_to_string(to.join("htop-1.pkg.tar.zst")).unwrap(), "old");
        assert_eq!(
            fs::read_to_string(to.join("jq-1.pkg.tar.zst")).unwrap(),
            "new",
            "a copy there already stays"
        );
        assert!(
            !to.join(".vim-1.pkg.tar.zst.part").exists(),
            "a torn copy isn't adopted"
        );
        assert!(!from.exists(), "the old cache goes, so its blocks aren't pinned twice");
        assert_eq!(adopt_cache(&from, &to).unwrap(), 0, "once is enough");
    }

    #[test]
    fn a_package_kept_without_reflinks_is_a_hard_link_not_a_second_copy() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let paths = cache_paths(dir.path());
        fs::write(paths.pacman_cache.join("vim-1.pkg.tar.zst"), vec![7; 8192]).unwrap();
        let how = keep_copy(&paths, "vim-1.pkg.tar.zst").unwrap();
        let (pacman, kept) = (
            fs::metadata(paths.pacman_cache.join("vim-1.pkg.tar.zst")).unwrap(),
            fs::metadata(paths.packages().join("vim-1.pkg.tar.zst")).unwrap(),
        );
        match how {
            Kept::Linked => assert_eq!(pacman.ino(), kept.ino()),
            Kept::Reflinked => assert_ne!(pacman.ino(), kept.ino()),
            other => panic!("kept as {other:?} on one filesystem"),
        }
        assert_eq!(keep_copy(&paths, "vim-1.pkg.tar.zst").unwrap(), Kept::Nothing);
        assert_eq!(keep_copy(&paths, "absent.pkg.tar.zst").unwrap(), Kept::Nothing);
        fs::remove_file(paths.pacman_cache.join("vim-1.pkg.tar.zst")).unwrap();
        assert_eq!(
            fs::read(paths.packages().join("vim-1.pkg.tar.zst")).unwrap().len(),
            8192
        );
    }

    #[test]
    fn the_cache_counts_a_hard_linked_file_once_by_its_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let paths = cache_paths(dir.path());
        fs::create_dir_all(paths.packages()).unwrap();
        fs::write(paths.packages().join("a"), vec![1; 65536]).unwrap();
        fs::hard_link(paths.packages().join("a"), paths.packages().join("b")).unwrap();
        let size = cache_size(&paths);
        assert!((65536..2 * 65536).contains(&size), "{size}");
    }

    #[test]
    fn a_low_disk_drops_cached_official_packages_of_older_locks_but_never_the_current_locks_or_unarchived_ones() {
        let dir = tempfile::tempdir().unwrap();
        let paths = cache_paths(dir.path());
        fs::create_dir_all(paths.packages()).unwrap();
        let older = generation(1, &[("vim", "extra"), ("cachy", "cachyos-v3"), ("nano", "core")]).1;
        let mut current = generation(2, &[("bash", "core")]).1;
        current.insert("nano".to_string(), older["nano"].clone());
        for file in [
            "vim-1.pkg.tar.zst",
            "cachy-1.pkg.tar.zst",
            "nano-1.pkg.tar.zst",
            "bash-2.pkg.tar.zst",
        ] {
            fs::write(paths.packages().join(file), vec![1; 100]).unwrap();
        }
        assert_eq!(drop_refetchable(&paths, &current, &[&older]).unwrap(), 100);
        let mut left: Vec<String> = fs::read_dir(paths.packages())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            ["bash-2.pkg.tar.zst", "cachy-1.pkg.tar.zst", "nano-1.pkg.tar.zst"]
        );
    }

    #[test]
    fn the_cache_keeps_every_package_of_the_newest_two_and_only_the_unarchived_ones_of_older_generations() {
        let generations = [
            generation(1, &[("vim", "extra"), ("cachy", "cachyos-v3")]),
            generation(2, &[("vim", "extra")]),
            generation(3, &[("vim", "core"), ("cachy", "cachyos-v3")]),
        ];
        let kept: Vec<_> = generations.iter().map(|(n, p)| (*n, p)).collect();
        let files = cached_files(&kept);
        assert_eq!(
            files,
            BTreeSet::from([
                "cachy-1.pkg.tar.zst",
                "vim-2.pkg.tar.zst",
                "vim-3.pkg.tar.zst",
                "cachy-3.pkg.tar.zst"
            ])
        );
    }
}

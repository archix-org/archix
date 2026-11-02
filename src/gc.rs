use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::deploy::Keep;
use crate::generations::{Store, kept, now};
use crate::paths::Paths;

#[derive(Debug, Default, PartialEq)]
pub struct Report {
    pub deployments: Vec<u32>,
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

/// Copies `file` from pacman's cache into archix's own, as a reflink where the filesystem has them, unless it's there; paccache never touches archix's cache.
pub fn keep_copy(paths: &Paths, file: &str) -> Result<bool> {
    let target = paths.packages().join(file);
    let source = paths.pacman_cache.join(file);
    if target.is_file() || !source.is_file() {
        return Ok(false);
    }
    fs::create_dir_all(paths.packages())?;
    let temp = paths.packages().join(format!(".{file}.part"));
    let status = std::process::Command::new("cp")
        .arg("--reflink=auto")
        .arg(&source)
        .arg(&temp)
        .status()
        .context("running cp")?;
    if !status.success() {
        let _ = fs::remove_file(&temp);
        anyhow::bail!(
            "copying {} into {} failed",
            source.display(),
            paths.packages().display()
        );
    }
    fs::rename(&temp, &target)?;
    Ok(true)
}

/// The bytes archix's own package cache holds.
pub fn cache_size(paths: &Paths) -> u64 {
    fs::read_dir(paths.packages())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .filter(|meta| meta.is_file())
        .map(|meta| meta.len())
        .sum()
}

/// Drops the deployments `deployments.keep` leaves out (never the booted, current or boot-default one) with their boot entries, then generation records outside the generation limits that have no deployment, then cached files no kept generation needs.
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
    let mut deployed = BTreeSet::new();
    if crate::deploy::layout::laid_out(&paths.root) {
        let deployments = crate::deploy::Store::open(&paths.root)?;
        let _lock = deployments.lock()?;
        let esp = settings.boot.as_ref().map(|boot| PathBuf::from(&boot.esp));
        let keep = settings.deployments_keep.max(1) as usize;
        if let Some(esp) = &esp {
            crate::system::boot::require_esp(esp)?;
        }
        let pins = match &esp {
            Some(esp) => Keep::read(keep, &deployments, esp)?,
            None => Keep {
                keep,
                booted: crate::system::boot::booted_deployment(&paths.root),
                current: deployments.current()?,
                chosen: deployments.chosen()?,
                ..Keep::default()
            },
        };
        for number in pins.to_delete(&deployments.deployments()?) {
            // The entries go first, so no entry is ever left naming a deployment that's gone.
            if let Some(esp) = &esp {
                crate::system::boot::drop_entries(paths, esp, number)?;
            }
            deployments.delete(number, &pins)?;
            let _ = fs::remove_file(crate::system::hooks::pending_path(&paths.state, number));
            crate::flatpak::drop_pending(&paths.state, Some(number));
            report.deployments.push(number);
        }
        if let Some(esp) = &esp {
            crate::system::boot::prune_records(paths, esp)?;
        }
        deployed.extend(deployments.deployments()?);
    }
    let mut keep = kept(&generations, current, settings.keep, settings.keep_days, now());
    // The running system came from the booted generation; it's what a broken switch falls back to.
    let booted = crate::system::boot::booted_deployment(&paths.root);
    keep.extend(booted.filter(|number| generations.iter().any(|g| g.number == *number)));
    keep.extend(deployed);
    for generation in &generations {
        if !keep.contains(&generation.number) {
            store.delete(generation.number)?;
            report.generations.push(generation.number);
        }
    }

    let kept_generations: Vec<_> = generations.iter().filter(|g| keep.contains(&g.number)).collect();
    let locks: Vec<_> = kept_generations.iter().map(|g| (g.number, &g.packages)).collect();
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

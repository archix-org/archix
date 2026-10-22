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
    let files: BTreeSet<&str> = kept_generations
        .iter()
        .flat_map(|g| g.packages.values())
        .map(|pkg| pkg.file.as_str())
        .collect();
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

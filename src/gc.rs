use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::generations::{Store, kept, now};
use crate::paths::Paths;

#[derive(Debug, Default, PartialEq)]
pub struct Report {
    pub generations: Vec<u32>,
    pub files: usize,
    pub bytes: u64,
    pub snapshots: usize,
}

/// Deletes generations outside the current generation's limits, then cached files no kept generation needs.
pub fn collect(paths: &Paths, snapshot_cache: &Path) -> Result<Report> {
    if paths.journal().exists() {
        bail!("a switch is unfinished; run `archix recover` before collecting garbage");
    }
    let store = Store::new(paths);
    let generations = store.list()?;
    let current = store.current()?;
    let Some(settings) = current
        .and_then(|n| generations.iter().find(|g| g.number == n))
        .map(|g| g.settings.clone())
    else {
        return Ok(Report::default());
    };
    let keep = kept(&generations, current, settings.keep, settings.keep_days, now());
    let mut report = Report::default();
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
    if let Ok(entries) = fs::read_dir(paths.packages()) {
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

    let dates: BTreeSet<String> = kept_generations.iter().map(|g| g.date.to_string()).collect();
    if let Ok(entries) = fs::read_dir(snapshot_cache.join("snapshots")) {
        for entry in entries.flatten() {
            if !dates.contains(entry.file_name().to_string_lossy().as_ref()) {
                fs::remove_dir_all(entry.path()).with_context(|| format!("removing {}", entry.path().display()))?;
                report.snapshots += 1;
            }
        }
    }
    Ok(report)
}

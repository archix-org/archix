use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Result, bail};

use crate::archive::Archive;
use crate::aur::git::AurGit;
use crate::aur::srcinfo::{self, SrcInfo};
use crate::date::Date;
use crate::fetch::{Wanted, fetch_all, sha256};
use crate::lock::{ARCHIVE_REPO, AUR_REPO, HostLock, Locked};
use crate::pacman::{LocalPkg, read_sync_db, vercmp};
use crate::paths::Paths;
use crate::repos::Repo;
use crate::resolve::{Aur, resolve};

/// Installed packages at a newer version than the lock, the drift a manual `pacman -Syu` or `yay` leaves.
pub fn newer_than_lock<'a>(lock: &HostLock, installed: &'a BTreeMap<String, LocalPkg>) -> Vec<&'a LocalPkg> {
    lock.packages
        .iter()
        .filter_map(|(name, locked)| {
            installed
                .get(name)
                .filter(|pkg| vercmp(&pkg.version, &locked.version) == Ordering::Greater)
        })
        .collect()
}

/// Which installed versions [`fit`] pins when they differ from the snapshot's.
#[derive(Clone, Copy, PartialEq)]
pub enum Fit {
    /// Only newer ones: what `pacman -Syu` left ahead of the lock.
    Newer,
    /// Any version that differs, for taking over a system exactly as it is.
    Exact,
}

/// A new lock matching what's installed: the best-fitting snapshot date, plus pins for anything still newer.
pub fn adopt(
    archive: &Archive,
    cache: &Path,
    paths: &Paths,
    lock: &HostLock,
    installed: &BTreeMap<String, LocalPkg>,
    repos: &[Repo],
    aur: Option<&Aur>,
) -> Result<HostLock> {
    let newer = newer_than_lock(lock, installed);
    if newer.is_empty() {
        return Ok(lock.clone());
    }
    let updated_at = newer.iter().filter_map(|pkg| pkg.installed_at).max();
    let day = updated_at.map_or_else(Date::today_utc, Date::from_unix);
    let mut candidates = dates_around(archive, day)?;
    candidates.insert(lock.date);
    let candidates = Candidates { dates: candidates, day };
    fit(
        archive,
        cache,
        paths,
        &lock.declared,
        &candidates,
        Some(lock),
        installed,
        repos,
        aur,
        Fit::Newer,
    )
}

/// The snapshot dates the day before, of and after `day` that the archive already has.
pub fn dates_around(archive: &Archive, day: Date) -> Result<BTreeSet<Date>> {
    let latest = archive.latest()?;
    Ok([day.previous(), day, day.next()]
        .into_iter()
        .filter(|date| *date <= latest)
        .collect())
}

/// How many installed packages are at exactly the version the snapshot of `repos` on `date` carries.
pub fn snapshot_score(
    archive: &Archive,
    cache: &Path,
    date: Date,
    repos: &[String],
    installed: &BTreeMap<String, LocalPkg>,
) -> Result<usize> {
    let dir = archive.snapshot_db(cache, date, repos)?;
    let mut score = 0;
    for repo in repos {
        let db = read_sync_db(&dir.join("sync").join(format!("{repo}.db")))?;
        score += db
            .values()
            .filter(|pkg| {
                installed
                    .get(&pkg.name)
                    .is_some_and(|local| local.version == pkg.version)
            })
            .count();
    }
    Ok(score)
}

/// Walks the days around `start` toward the snapshot matching the most installed versions; a partial upgrade can't drag it off.
pub fn best_snapshot(
    archive: &Archive,
    cache: &Path,
    start: Date,
    latest: Date,
    repos: &[String],
    installed: &BTreeMap<String, LocalPkg>,
) -> Result<Date> {
    const PATIENCE: usize = 3;
    const MAX_DAYS: usize = 60;
    let start = start.min(latest);
    let mut best = (start, snapshot_score(archive, cache, start, repos, installed)?);
    for forward in [false, true] {
        let mut date = start;
        let mut worse = 0;
        for _ in 0..MAX_DAYS {
            date = if forward { date.next() } else { date.previous() };
            if date > latest {
                break;
            }
            let score = snapshot_score(archive, cache, date, repos, installed).unwrap_or(0);
            if score > best.1 {
                best = (date, score);
                worse = 0;
            } else {
                worse += 1;
                if worse >= PATIENCE {
                    break;
                }
            }
        }
    }
    Ok(best.0)
}

pub struct Candidates {
    pub dates: BTreeSet<Date>,
    /// Only for the error message when none of them resolves.
    pub day: Date,
}

/// Resolves `declared` at each candidate date, keeps the one most installed versions match and pins the rest per `mode`.
#[allow(clippy::too_many_arguments)]
pub fn fit(
    archive: &Archive,
    cache: &Path,
    paths: &Paths,
    declared: &[String],
    candidates: &Candidates,
    previous: Option<&HostLock>,
    installed: &BTreeMap<String, LocalPkg>,
    repos: &[Repo],
    aur: Option<&Aur>,
    mode: Fit,
) -> Result<HostLock> {
    let mut best: Option<(usize, HostLock)> = None;
    for date in &candidates.dates {
        let mut resolved = match resolve(archive, cache, *date, repos, declared, aur) {
            Ok(resolved) => resolved,
            Err(e) => {
                eprintln!("skipping snapshot {date}: {e:#}");
                continue;
            }
        };
        if let Some(previous) = previous {
            resolved.keep_pins(previous);
        }
        let score = resolved
            .packages
            .iter()
            .filter(|(name, locked)| installed.get(*name).is_some_and(|pkg| pkg.version == locked.version))
            .count();
        if best.as_ref().is_none_or(|(top, _)| score >= *top) {
            best = Some((score, resolved));
        }
    }
    let Some((_, mut adopted)) = best else {
        bail!("no snapshot around {} resolves the declared packages", candidates.day)
    };

    let differs = |installed: &str, locked: &str| match mode {
        Fit::Newer => vercmp(installed, locked) == Ordering::Greater,
        Fit::Exact => vercmp(installed, locked) != Ordering::Equal,
    };
    let mut pins = Vec::new();
    let mut aur_bases = BTreeMap::new();
    for (name, locked) in &adopted.packages {
        let Some(pkg) = installed.get(name) else { continue };
        if !differs(&pkg.version, &locked.version) {
            continue;
        }
        match &locked.base {
            Some(base) if locked.repo == AUR_REPO => {
                aur_bases.insert(base.clone(), pkg.version.clone());
            }
            _ => pins.push((name.clone(), pkg.clone(), locked.clone())),
        }
    }
    if let Some(aur) = aur {
        for (base, version) in aur_bases {
            match commit_for_version(aur.git, &base, &version)? {
                Some((commit, info)) => {
                    for (name, locked) in adopted.packages.iter_mut() {
                        if locked.base.as_deref() == Some(base.as_str())
                            && let Some(pkg) = info.package(name)
                        {
                            locked.version = info.version.clone();
                            locked.file = info.file(pkg);
                            locked.commit = Some(commit.clone());
                        }
                    }
                }
                None => eprintln!(
                    "warning: no {base} commit in the AUR builds {version}; switch will rebuild the locked one"
                ),
            }
        }
    }
    for (name, pkg, locked) in pins {
        let file = format!("{name}-{}-{}.pkg.tar.zst", pkg.version, pkg.arch);
        let mut urls = repos
            .iter()
            .find(|repo| repo.name == locked.repo)
            .map(|repo| repo.urls(&file))
            .unwrap_or_default();
        urls.push(archive.package_url(&name, &file));
        let wanted = Wanted {
            name: name.clone(),
            urls,
            file,
            sha256: None,
        };
        let fetched = fetch_all(
            archive,
            &paths.packages(),
            std::slice::from_ref(&paths.pacman_cache),
            std::slice::from_ref(&wanted),
        );
        let file = match (fetched, mode) {
            (Ok(mut files), _) => files.remove(0),
            (Err(e), Fit::Newer) => return Err(e),
            (Err(e), Fit::Exact) => {
                eprintln!(
                    "warning: {e:#}\n  the lock keeps {name} {} from the snapshot",
                    locked.version
                );
                continue;
            }
        };
        let pinned = Locked {
            version: pkg.version.clone(),
            repo: ARCHIVE_REPO.to_string(),
            file: file
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default(),
            sha256: sha256(&file)?,
            explicit: locked.explicit,
            ..Default::default()
        };
        adopted.pin(&name, pinned);
    }
    Ok(adopted)
}

/// The newest commit in a base's history whose .SRCINFO builds exactly `version`.
pub fn commit_for_version(git: &AurGit, base: &str, version: &str) -> Result<Option<(String, SrcInfo)>> {
    git.sync(base)?;
    for commit in git.history(base)?.into_iter().take(500) {
        let Ok(text) = git.show(base, &commit, ".SRCINFO") else {
            continue;
        };
        if let Ok(info) = srcinfo::parse(&text)
            && info.version == version
        {
            return Ok(Some((commit, info)));
        }
    }
    Ok(None)
}

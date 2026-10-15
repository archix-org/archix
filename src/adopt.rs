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
use crate::pacman::{LocalPkg, vercmp};
use crate::paths::Paths;
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

/// A new lock matching what's installed: the best-fitting snapshot date, plus pins for anything still newer.
pub fn adopt(
    archive: &Archive,
    cache: &Path,
    paths: &Paths,
    lock: &HostLock,
    installed: &BTreeMap<String, LocalPkg>,
    aur: Option<&Aur>,
) -> Result<HostLock> {
    let newer = newer_than_lock(lock, installed);
    if newer.is_empty() {
        return Ok(lock.clone());
    }
    let updated_at = newer.iter().filter_map(|pkg| pkg.installed_at).max();
    let day = updated_at.map_or_else(Date::today_utc, Date::from_unix);
    let latest = archive.latest()?;
    let candidates: BTreeSet<Date> = [lock.date, day.previous(), day, day.next()]
        .into_iter()
        .filter(|date| *date <= latest)
        .collect();

    let mut best: Option<(usize, HostLock)> = None;
    for date in candidates {
        let mut resolved = match resolve(archive, cache, date, &lock.repos, &lock.declared, aur) {
            Ok(resolved) => resolved,
            Err(e) => {
                eprintln!("skipping snapshot {date}: {e:#}");
                continue;
            }
        };
        resolved.keep_pins(lock);
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
        bail!("no snapshot around {day} resolves the declared packages")
    };

    let mut pins = Vec::new();
    let mut aur_bases = BTreeMap::new();
    for (name, locked) in &adopted.packages {
        let Some(pkg) = installed.get(name) else { continue };
        if vercmp(&pkg.version, &locked.version) != Ordering::Greater {
            continue;
        }
        match &locked.base {
            Some(base) if locked.repo == AUR_REPO => {
                aur_bases.insert(base.clone(), pkg.version.clone());
            }
            _ => pins.push((name.clone(), pkg.clone(), locked.explicit)),
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
    let wanted: Vec<Wanted> = pins
        .iter()
        .map(|(name, pkg, _)| {
            let file = format!("{name}-{}-{}.pkg.tar.zst", pkg.version, pkg.arch);
            Wanted {
                name: name.clone(),
                urls: vec![archive.package_url(name, &file)],
                file,
                sha256: None,
            }
        })
        .collect();
    let files = fetch_all(
        archive,
        &paths.packages(),
        std::slice::from_ref(&paths.pacman_cache),
        &wanted,
    )?;
    for ((name, pkg, explicit), file) in pins.into_iter().zip(files) {
        let locked = Locked {
            version: pkg.version.clone(),
            repo: ARCHIVE_REPO.to_string(),
            file: file
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default(),
            sha256: sha256(&file)?,
            explicit,
            ..Default::default()
        };
        adopted.pin(&name, locked);
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

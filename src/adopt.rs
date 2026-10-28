use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Result, bail};

use crate::archive::Archive;
use crate::aur::git::AurGit;
use crate::aur::sources::{Kind, floating};
use crate::aur::srcinfo::{self, SrcInfo};
use crate::date::Date;
use crate::fetch::{Wanted, fetch_all, sha256};
use crate::lock::{ARCHIVE_REPO, AUR_REPO, HostLock, Locked};
use crate::pacman::{LocalPkg, read_sync_db, vercmp};
use crate::paths::Paths;
use crate::repos::Repo;
use crate::resolve::{Aur, resolve};

/// Installed packages at a newer version than the lock, the drift a manual `pacman -Syu` or `yay` leaves; a held one isn't adopted but goes back to its hold.
pub fn newer_than_lock<'a>(lock: &HostLock, installed: &'a BTreeMap<String, LocalPkg>) -> Vec<&'a LocalPkg> {
    lock.packages
        .iter()
        .filter(|(name, _)| !holding(lock, name))
        .filter_map(|(name, locked)| {
            installed
                .get(name)
                .filter(|pkg| vercmp(&pkg.version, &locked.version) == Ordering::Greater)
        })
        .collect()
}

/// Whether the lock keeps `name` at a version or commit by the config's `holds`, rather than only above a floor.
fn holding(lock: &HostLock, name: &str) -> bool {
    lock.held.get(name).is_some_and(|held| !held.hold.starts_with(">="))
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
        &lock.prefer,
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
    prefer: &[String],
    candidates: &Candidates,
    previous: Option<&HostLock>,
    installed: &BTreeMap<String, LocalPkg>,
    repos: &[Repo],
    aur: Option<&Aur>,
    mode: Fit,
) -> Result<HostLock> {
    let holds = previous.map(crate::holds::from_lock).transpose()?.unwrap_or_default();
    let mut best: Option<(usize, HostLock)> = None;
    for date in &candidates.dates {
        let mut resolved = match resolve(archive, cache, *date, repos, declared, prefer, aur, &holds) {
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
    let mut moved = false;
    if let Some(aur) = aur {
        let mut aur_bases = BTreeMap::new();
        for (name, locked) in &adopted.packages {
            if let (Some(pkg), Some(base)) = (installed.get(name), &locked.base)
                && locked.repo == AUR_REPO
                && !holding(&adopted, name)
                && differs(&pkg.version, &locked.version)
            {
                aur_bases.insert(base.clone(), pkg.version.clone());
            }
        }
        for (base, version) in aur_bases {
            match commit_for_version(aur.git, &base, &version)? {
                Some((commit, _)) => {
                    aur.commits.borrow_mut().insert(base, commit);
                    moved = true;
                }
                None if vcs_commit(&version).is_some() => {}
                None => eprintln!(
                    "warning: no {base} commit in the AUR builds {version}; switch will rebuild the locked one"
                ),
            }
        }
    }
    if moved {
        adopted = resolve(archive, cache, adopted.date, repos, declared, prefer, aur, &holds)?;
        if let Some(previous) = previous {
            adopted.keep_pins(previous);
        }
    }
    if let Some(aur) = aur {
        for (name, locked) in adopted.packages.iter_mut() {
            let Some(pkg) = installed.get(name) else { continue };
            let (Some(base), Some(commit)) = (locked.base.clone(), locked.commit.clone()) else {
                continue;
            };
            if locked.repo != AUR_REPO || !differs(&pkg.version, &locked.version) || adopted.held.contains_key(name) {
                continue;
            }
            let Some(upstream) = vcs_commit(&pkg.version) else {
                continue;
            };
            let info = aur
                .git
                .show(&base, &commit, ".SRCINFO")
                .and_then(|text| srcinfo::parse(&text));
            match info.ok().and_then(|info| vcs_sources(&info, upstream)) {
                Some(sources) => {
                    eprintln!(
                        "{name}: locked at {upstream}, the upstream commit its installed {} was built from",
                        pkg.version
                    );
                    locked.sources = sources;
                    locked.version = pkg.version.clone();
                    locked.file = format!("{name}-{}-{}.pkg.tar.zst", pkg.version, pkg.arch);
                }
                None => eprintln!(
                    "warning: {name} {} names upstream commit {upstream}, but its recipe has no single git source to pin there; switch will rebuild it from the newest code",
                    pkg.version
                ),
            }
        }
    }
    let mut pins = Vec::new();
    for (name, locked) in &adopted.packages {
        let Some(pkg) = installed.get(name) else { continue };
        if !locked.is_built() && differs(&pkg.version, &locked.version) && !holding(&adopted, name) {
            pins.push((name.clone(), pkg.clone(), locked.clone()));
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
            size: None,
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

/// The upstream commit a VCS package's version names, such as 1a2b3c4 in 0.4.0.r12.g1a2b3c4-1, as the pkgver functions of -git recipes write it.
pub fn vcs_commit(version: &str) -> Option<&str> {
    let pkgver = version.rsplit_once('-').map_or(version, |(pkgver, _)| pkgver);
    let pkgver = pkgver.split_once(':').map_or(pkgver, |(_, pkgver)| pkgver);
    pkgver
        .split(['.', '+', '_'])
        .filter_map(|part| part.strip_prefix('g'))
        .rfind(|hash| (7..=40).contains(&hash.len()) && hash.chars().all(|c| c.is_ascii_hexdigit()))
}

/// The pinned sources that build a recipe's one git source at `commit`, None when it doesn't have exactly one.
pub fn vcs_sources(info: &SrcInfo, commit: &str) -> Option<BTreeMap<String, String>> {
    let git: Vec<_> = floating(info)
        .into_iter()
        .filter(|source| matches!(source.kind, Kind::Git { .. }))
        .collect();
    match &git[..] {
        [one] => Some(BTreeMap::from([(one.entry.clone(), commit.to_string())])),
        _ => None,
    }
}

/// The newest commit whose .SRCINFO says `version`, skipping one whose PKGBUILD had moved on before its .SRCINFO caught up.
pub fn commit_for_version(git: &AurGit, base: &str, version: &str) -> Result<Option<(String, SrcInfo)>> {
    commit_where(git, base, |found| found == version)
}

/// The newest commit whose .SRCINFO's version `wanted` accepts, as [`commit_for_version`] picks it.
pub fn commit_where(git: &AurGit, base: &str, wanted: impl Fn(&str) -> bool) -> Result<Option<(String, SrcInfo)>> {
    git.sync(base)?;
    let mut newer: Option<(String, String)> = None;
    for commit in git.history(base)?.into_iter().take(500) {
        let Some(info) = git
            .show(base, &commit, ".SRCINFO")
            .ok()
            .and_then(|text| srcinfo::parse(&text).ok())
        else {
            newer = None;
            continue;
        };
        let pkgbuild = git.show(base, &commit, "PKGBUILD").unwrap_or_default();
        let stale = newer
            .as_ref()
            .is_some_and(|(next_version, next_pkgbuild)| !wanted(next_version) && *next_pkgbuild == pkgbuild);
        if wanted(&info.version) && !stale {
            return Ok(Some((commit, info)));
        }
        newer = Some((info.version.clone(), pkgbuild));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn commit(repo: &Path, pkgbuild_version: &str, srcinfo_version: &str, message: &str) {
        std::fs::write(
            repo.join("PKGBUILD"),
            format!("pkgname=demo\npkgver={pkgbuild_version}\npkgrel=1\narch=(any)\n"),
        )
        .unwrap();
        std::fs::write(
            repo.join(".SRCINFO"),
            format!("pkgbase = demo\n\tpkgver = {srcinfo_version}\n\tpkgrel = 1\n\tarch = any\n\npkgname = demo\n"),
        )
        .unwrap();
        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                    .arg("-C")
                    .arg(repo)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            )
        };
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", message]);
    }

    #[test]
    fn a_vcs_package_is_pinned_at_the_commit_its_version_names() {
        assert_eq!(vcs_commit("0.4.0.r12.g1a2b3c4-1"), Some("1a2b3c4"));
        assert_eq!(vcs_commit("2:r1234+g0123456789ab-2"), Some("0123456789ab"));
        assert_eq!(vcs_commit("1.2.3-1"), None);
        assert_eq!(vcs_commit("r12.gcc-1"), None, "too short for a commit");
        let info = srcinfo::parse(
            "pkgbase = tool-git\n\tpkgver = 0.4.0.r20.g5d6e7f8\n\tpkgrel = 1\n\tarch = x86_64\n\
             \tsource = tool::git+https://example.org/tool.git\n\tsource = fix.patch\n\tsha256sums = SKIP\n\tsha256sums = abc\n\npkgname = tool-git\n",
        )
        .unwrap();
        assert_eq!(
            vcs_sources(&info, "1a2b3c4"),
            Some(BTreeMap::from([(
                "tool::git+https://example.org/tool.git".to_string(),
                "1a2b3c4".to_string()
            )]))
        );
        let two = srcinfo::parse(
            "pkgbase = two-git\n\tpkgver = 1\n\tpkgrel = 1\n\tarch = any\n\
             \tsource = git+https://example.org/a.git\n\tsource = git+https://example.org/b.git\n\tsha256sums = SKIP\n\tsha256sums = SKIP\n\npkgname = two-git\n",
        )
        .unwrap();
        assert_eq!(
            vcs_sources(&two, "1a2b3c4"),
            None,
            "which clone the hash is from is unknown"
        );
    }

    #[test]
    fn skips_a_commit_whose_pkgbuild_ran_ahead_of_its_srcinfo() {
        let dir = tempfile::tempdir().unwrap();
        let upstream = dir.path().join("aur");
        let repo = upstream.join("demo.git");
        std::fs::create_dir_all(&repo).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .arg(&repo)
                .status()
                .unwrap()
                .success()
        );
        commit(&repo, "1.0", "1.0", "1.0");
        commit(&repo, "2.0", "1.0", "bump the PKGBUILD, forget .SRCINFO");
        commit(&repo, "2.0", "2.0", "up .SRCINFO");

        let git = AurGit::new(&format!("file://{}", upstream.display()), &dir.path().join("cache"));
        let (found, info) = commit_for_version(&git, "demo", "1.0-1").unwrap().unwrap();
        assert_eq!(info.version, "1.0-1");
        assert_eq!(
            git.show("demo", &found, "PKGBUILD").unwrap().lines().nth(1),
            Some("pkgver=1.0"),
            "the commit that really builds 1.0, not the one with a stale .SRCINFO"
        );
        let (_, newest) = commit_for_version(&git, "demo", "2.0-1").unwrap().unwrap();
        assert_eq!(newest.version, "2.0-1");
    }
}

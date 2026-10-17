use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use archix::adopt::{adopt, newer_than_lock};
use archix::apply;
use archix::archive::{ARCHIVE_URL, Archive};
use archix::aur::build::Builder;
use archix::aur::index::Index;
use archix::aur::install::Installer;
use archix::aur::review::review;
use archix::installed::{self, FILE};
use archix::journal::{Journal, Kind, Transaction};
use archix::lock::{AUR_REPO, LOCK_FILE, Lockfile};
use archix::pacman::{read_local_db, read_sync_db};
use archix::paths::Paths;
use archix::plan::{Change, plan};
use archix::resolve::{resolve, snapshot_conf};

use super::lock::{current_lock, save_lock};
use super::{
    Snapshot, Target, aur_git, aur_source, check_pacman_free, confirm, count, declared, recipes, refuse_if_unfinished,
    require_root, transaction_for, user_cache,
};

/// An edit to an installed.lua that can be undone if the switch after it doesn't go through.
pub struct InstalledEdit {
    path: PathBuf,
    previous: Option<String>,
}

impl InstalledEdit {
    pub fn restore(&self) {
        let _ = match &self.previous {
            Some(text) => fs::write(&self.path, text),
            None => fs::remove_file(&self.path),
        };
    }
}

/// Adds (or removes) packages in the host's installed.lua, or the one in `path`, before the config is evaluated.
pub fn edit_installed(
    config_dir: &Path,
    host: &str,
    path: Option<&Path>,
    add: &[String],
    remove: &[String],
) -> Result<InstalledEdit> {
    let dir = match path {
        Some(dir) => {
            let module = installed::module_name(config_dir, dir)?;
            let default = installed::default_dir(config_dir, host);
            if dir.canonicalize().ok() != default.canonicalize().ok() {
                eprintln!("note: import {module:?} from a module this host loads, or archix won't see it");
            }
            dir.to_path_buf()
        }
        None => installed::default_dir(config_dir, host),
    };
    let file = dir.join(FILE);
    let previous = fs::read_to_string(&file).ok();
    let mut packages = installed::read(&file)?;
    for name in add {
        if !packages.contains(name) {
            packages.push(name.clone());
        }
    }
    for name in remove {
        if !packages.contains(name) {
            bail!(
                "{name} isn't in {}; it's declared elsewhere in the config, so remove it there",
                file.display()
            );
        }
        packages.retain(|p| p != name);
    }
    packages.sort();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    fs::write(&file, installed::render(&packages)).with_context(|| format!("writing {}", file.display()))?;
    Ok(InstalledEdit { path: file, previous })
}

/// Installs packages without declaring them; the next switch removes them unless they get declared.
pub fn install_temporarily(target: &Target, names: &[String], noconfirm: bool) -> Result<()> {
    require_root("install --temp")?;
    let paths = Paths::system();
    refuse_if_unfinished(&paths)?;
    check_pacman_free(&paths)?;
    let declared = declared(target.config)?;
    let (lock, _) = current_lock(target, &declared, Snapshot::Locked, false, true, false)?;
    let archive = Archive::new(&declared.archive_url).with_caches(&declared.package_caches);
    let git = aur_git(&declared)?;
    let recipes = recipes(target, &declared)?;
    let aur = aur_source(&git, &declared, &archive, Some(&lock), false, &recipes);
    let cache = user_cache()?;
    let temp = resolve(
        &archive,
        &cache,
        lock.date,
        &declared.repos,
        names,
        &declared.prefer,
        Some(&aur),
    )?;
    review(&git, Some(&lock), &temp, declared.aur_review && !noconfirm, &confirm)?;

    let installed = read_local_db(&paths.local_db())?;
    let changes: Vec<Change> = plan(&temp.packages, &installed)
        .into_iter()
        .filter(|change| matches!(change, Change::Install { .. }))
        .collect();
    if changes.is_empty() {
        println!("already installed: {}", names.join(" "));
        return Ok(());
    }
    for change in &changes {
        println!("{change}");
    }
    if !noconfirm
        && !confirm(&format!(
            "install {} without declaring them?",
            count(changes.len(), "package", "packages")
        ))?
    {
        bail!("aborted; nothing was changed");
    }
    let aur_names: Vec<String> = changes
        .iter()
        .filter_map(|change| match change {
            Change::Install { name, .. } if temp.packages[name].is_built() => Some(name.clone()),
            _ => None,
        })
        .collect();
    let built = if aur_names.is_empty() {
        BTreeMap::new()
    } else {
        let builder = Builder {
            paths: &paths,
            archive: &archive,
            git: &git,
            snapshot_cache: &cache,
            skip_check: &declared.skip_check,
        };
        Installer::new(&builder, &temp, &declared.build_on_host, Some(&recipes))?.build(&aur_names)?
    };
    let full = transaction_for(
        &archive,
        &paths,
        temp.date,
        &temp.packages,
        &changes,
        &built,
        &declared.repos,
    )?;
    let new: BTreeSet<&String> = changes
        .iter()
        .filter_map(|change| match change {
            Change::Install { name, .. } => Some(name),
            _ => None,
        })
        .collect();
    let transaction = Transaction {
        reinstall: false,
        at_boot: false,
        system: None,
        boot: None,
        install: full.install,
        remove: Vec::new(),
        versions: full
            .versions
            .into_iter()
            .filter(|(name, _)| new.contains(name))
            .collect(),
        reasons: new.iter().map(|name| ((*name).clone(), names.contains(name))).collect(),
    };
    archix::conflicts::check(&transaction.install, &transaction.remove, &paths.local_db())?;
    apply::check_packages(&transaction)?;
    let what = format!("a temporary install of {}", names.join(" "));
    let mut journal = Journal::new(Kind::Switch, target.host, what, installed, transaction);
    journal.save(&paths.journal())?;
    apply::run(&paths, &mut journal, false)?;
    println!(
        "installed {} without declaring it; the next `archix switch` removes it unless it's declared",
        names.join(" ")
    );
    Ok(())
}

/// Searches the locked snapshot's repos and the AUR; every term has to match the name or description.
/// One search result: where it's from, what it is, and how it's shown.
struct Hit {
    name: String,
    line: String,
    aur: bool,
}

/// How closely a name matches the terms: exact, prefix, contained, or only in the description.
fn rank(name: &str, terms: &[String]) -> u8 {
    let name = name.to_lowercase();
    let terms: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();
    if terms.contains(&name) {
        0
    } else if terms.first().is_some_and(|t| name.starts_with(t.as_str())) {
        1
    } else if terms.iter().any(|t| name.contains(t.as_str())) {
        2
    } else {
        3
    }
}

pub fn search(target: &Target, terms: &[String]) -> Result<()> {
    for hit in hits(target, terms)? {
        println!("{}", hit.line);
    }
    Ok(())
}

/// `archix <terms>`, like `yay <terms>`: numbered results with the best match last, then the picks to install.
pub fn pick(target: &Target, terms: &[String]) -> Result<Vec<String>> {
    let hits = hits(target, terms)?;
    if hits.is_empty() {
        println!("no packages match {}", terms.join(" "));
        return Ok(Vec::new());
    }
    for (number, hit) in hits.iter().enumerate().rev() {
        println!("{} {}", number + 1, hit.line);
    }
    eprint!("==> Packages to install (eg: 1 2 3, 1-3 or ^4): ");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    let picked = selection(&answer, hits.len())?;
    Ok(picked.into_iter().map(|number| hits[number - 1].name.clone()).collect())
}

/// Numbers picked from 1..=count: "1 2 3", "1-3", "^4" to leave one out (alone, it means all but that), commas allowed.
fn selection(answer: &str, count: usize) -> Result<Vec<usize>> {
    let mut picked = std::collections::BTreeSet::new();
    let mut excluded = std::collections::BTreeSet::new();
    let mut only_exclusions = true;
    for word in answer.split([' ', ',', '\t', '\n']).filter(|w| !w.is_empty()) {
        let (exclude, range) = match word.strip_prefix('^') {
            Some(rest) => (true, rest),
            None => (false, word),
        };
        let (from, to) = match range.split_once('-') {
            Some((a, b)) => (a.parse::<usize>(), b.parse::<usize>()),
            None => (range.parse::<usize>(), range.parse::<usize>()),
        };
        let (Ok(from), Ok(to)) = (from, to) else {
            bail!("{word:?} isn't a number, a range like 1-3 or an exclusion like ^4");
        };
        if from == 0 || to > count || from > to {
            bail!("{word} is outside 1-{count}");
        }
        if exclude {
            excluded.extend(from..=to);
        } else {
            only_exclusions = false;
            picked.extend(from..=to);
        }
    }
    if only_exclusions && !excluded.is_empty() {
        picked.extend(1..=count);
    }
    Ok(picked.difference(&excluded).copied().collect())
}

fn hits(target: &Target, terms: &[String]) -> Result<Vec<Hit>> {
    let declared = declared(target.config)?;
    let archive = Archive::new(&declared.archive_url).with_caches(&declared.package_caches);
    let date = match Lockfile::load(&target.config_dir.join(LOCK_FILE))?
        .hosts
        .get(target.host)
    {
        Some(lock) => lock.date,
        None => archive.latest()?,
    };
    let cache = user_cache()?;
    let installed = read_local_db(&Paths::system().local_db()).unwrap_or_default();
    let lowered: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();
    let matches = |name: &str, description: &str| {
        let haystack = format!("{name} {description}").to_lowercase();
        lowered.iter().all(|term| haystack.contains(term))
    };
    let marker = |name: &str| {
        if installed.contains_key(name) {
            " [installed]"
        } else {
            ""
        }
    };

    let mut hits = Vec::new();
    let (_, db) = snapshot_conf(&archive, &cache, date, &declared.repos)?;
    for repo in &declared.repos {
        for pkg in read_sync_db(&db.join("sync").join(format!("{}.db", repo.name)))?.into_values() {
            if matches(&pkg.name, &pkg.description) {
                let line = format!(
                    "{}/{} {}{}\n    {}",
                    repo.name,
                    pkg.name,
                    pkg.version,
                    marker(&pkg.name),
                    pkg.description
                );
                hits.push(Hit {
                    name: pkg.name,
                    line,
                    aur: false,
                });
            }
        }
    }
    let index = Index::load(&archive, &declared.aur_url, &cache, false)?;
    for entry in index.search(terms) {
        let stale = if entry.out_of_date.is_some() {
            " (out of date)"
        } else {
            ""
        };
        let line = format!(
            "aur/{} {} (+{} {:.2}){stale}{}\n    {}",
            entry.name,
            entry.version,
            entry.num_votes,
            entry.popularity,
            marker(&entry.name),
            entry.description.as_deref().unwrap_or("")
        );
        hits.push(Hit {
            name: entry.name.clone(),
            line,
            aur: true,
        });
    }
    hits.sort_by_key(|hit| (rank(&hit.name, terms), hit.aur, hit.name.clone()));
    Ok(hits)
}

/// `archix -Syu`: pacman -Syu from live mirrors, adopt it, move AUR packages to their newest recipes, then switch.
pub fn upgrade(target: &Target, noconfirm: bool) -> Result<()> {
    require_root("upgrade")?;
    let paths = Paths::system();
    refuse_if_unfinished(&paths)?;
    check_pacman_free(&paths)?;
    let mut pacman = Command::new("pacman");
    pacman.arg("-Syu");
    if noconfirm {
        pacman.arg("--noconfirm");
    }
    if !pacman.status().context("running pacman -Syu")?.success() {
        bail!("pacman -Syu failed; nothing else was changed");
    }

    let declared = declared(target.config)?;
    let (mut lock, _) = current_lock(target, &declared, Snapshot::Locked, true, noconfirm, false)?;
    let archive = Archive::new(&declared.archive_url).with_caches(&declared.package_caches);
    let git = aur_git(&declared)?;
    let installed = read_local_db(&paths.local_db())?;
    if !newer_than_lock(&lock, &installed).is_empty() {
        let previous = lock.clone();
        let recipes = recipes(target, &declared)?;
        let aur = aur_source(&git, &declared, &archive, Some(&previous), false, &recipes);
        let adopted = adopt(
            &archive,
            &user_cache()?,
            &paths,
            &previous,
            &installed,
            &declared.repos,
            Some(&aur),
        )?;
        let mut lockfile = Lockfile::load(&target.config_dir.join(LOCK_FILE))?;
        save_lock(target, &mut lockfile, Some(&previous), &adopted)?;
        drop(aur);
        lock = adopted;
    }
    if lock.packages.values().any(|pkg| pkg.repo == AUR_REPO) {
        let recipes = recipes(target, &declared)?;
        let aur = aur_source(&git, &declared, &archive, None, true, &recipes);
        let mut fresh = resolve(
            &archive,
            &user_cache()?,
            lock.date,
            &declared.repos,
            &declared.packages,
            &declared.prefer,
            Some(&aur),
        )?;
        fresh.keep_pins(&lock);
        if fresh != lock {
            review(&git, Some(&lock), &fresh, declared.aur_review && !noconfirm, &confirm)?;
            let mut lockfile = Lockfile::load(&target.config_dir.join(LOCK_FILE))?;
            save_lock(target, &mut lockfile, Some(&lock), &fresh)?;
        }
    }
    super::switch(target, noconfirm, None, false)
}

/// Downloads `url` and prints the fetchurl snippet with its sha256; as root it also keeps the file for the next switch.
pub fn prefetch(url: &str) -> Result<()> {
    let dir = user_cache()?.join("prefetch");
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!("download.{}", std::process::id()));
    if !Archive::new(ARCHIVE_URL).fetch(url, &tmp)? {
        bail!("{url} doesn't exist");
    }
    let sha256 = archix::fetch::sha256(&tmp)?;
    if unsafe { libc::geteuid() } == 0 {
        let blob = archix::system::blobs::path(&Paths::system(), &sha256);
        if let Some(parent) = blob.parent() {
            fs::create_dir_all(parent)?;
        }
        if fs::rename(&tmp, &blob).is_err() {
            fs::copy(&tmp, &blob)?;
        }
    }
    let _ = fs::remove_file(&tmp);
    println!(
        "archix.fetchurl {{ url = {}, sha256 = \"{sha256}\" }}",
        archix::luagen::string(url)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::selection;

    #[test]
    fn selections_read_like_yay() {
        assert_eq!(selection("1 2 3\n", 5).unwrap(), [1, 2, 3]);
        assert_eq!(selection("2-4", 5).unwrap(), [2, 3, 4]);
        assert_eq!(selection("1,3", 5).unwrap(), [1, 3]);
        assert_eq!(
            selection("^4", 5).unwrap(),
            [1, 2, 3, 5],
            "only exclusions means all but those"
        );
        assert_eq!(selection("1-5 ^2-3", 5).unwrap(), [1, 4, 5]);
        assert!(selection("\n", 5).unwrap().is_empty());
        assert!(selection("6", 5).is_err());
        assert!(selection("x", 5).is_err());
    }
}

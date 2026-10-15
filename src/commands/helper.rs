use std::collections::{BTreeMap, BTreeSet};
use std::fs;
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
    Snapshot, Target, aur_git, aur_source, check_pacman_free, confirm, count, declared, refuse_if_unfinished,
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
    let (lock, _) = current_lock(target, &declared, Snapshot::Locked, false, true)?;
    let archive = Archive::new(ARCHIVE_URL);
    let git = aur_git(&declared)?;
    let aur = aur_source(&git, &declared, &archive, Some(&lock), false);
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
            Change::Install { name, .. } if temp.packages[name].repo == AUR_REPO => Some(name.clone()),
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
        };
        Installer::new(&builder, &temp, &declared.build_on_host)?.build(&aur_names)?
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
pub fn search(target: &Target, terms: &[String]) -> Result<()> {
    let declared = declared(target.config)?;
    let archive = Archive::new(ARCHIVE_URL);
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

    let (_, db) = snapshot_conf(&archive, &cache, date, &declared.repos)?;
    for repo in &declared.repos {
        for pkg in read_sync_db(&db.join("sync").join(format!("{}.db", repo.name)))?.into_values() {
            if matches(&pkg.name, &pkg.description) {
                println!(
                    "{}/{} {}{}\n    {}",
                    repo.name,
                    pkg.name,
                    pkg.version,
                    marker(&pkg.name),
                    pkg.description
                );
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
        println!(
            "aur/{} {} (+{} {:.2}){stale}{}\n    {}",
            entry.name,
            entry.version,
            entry.num_votes,
            entry.popularity,
            marker(&entry.name),
            entry.description.as_deref().unwrap_or("")
        );
    }
    Ok(())
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
    let (mut lock, _) = current_lock(target, &declared, Snapshot::Locked, true, noconfirm)?;
    let archive = Archive::new(ARCHIVE_URL);
    let git = aur_git(&declared)?;
    let installed = read_local_db(&paths.local_db())?;
    if !newer_than_lock(&lock, &installed).is_empty() {
        let previous = lock.clone();
        let aur = aur_source(&git, &declared, &archive, Some(&previous), false);
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
        let aur = aur_source(&git, &declared, &archive, None, true);
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
    super::switch(target, noconfirm, None)
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

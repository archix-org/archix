use std::fs;
use std::io::{self, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};

use archix::archive::{ARCHIVE_URL, Archive};
use archix::aur::index::Index;
use archix::aur::review::review;
use archix::installed::{self, FILE};
use archix::lock::AUR_REPO;
use archix::pacman::{read_local_db, read_sync_db};
use archix::paths::Paths;
use archix::resolve::{resolve, snapshot_conf};

use super::lock::{current_lock, load_lockfile, save_lock};
use super::{
    Snapshot, Target, aur_git, aur_source, check_pacman_free, confirm, declared, recipes, require_root, user_cache,
};

/// Adds (or removes) packages in the host's installed.lua, or the one in `path`; the new text waits for the switch to commit and is evaluated from memory until then.
pub fn edit_installed(
    config_dir: &Path,
    host: &str,
    path: Option<&Path>,
    add: &[String],
    remove: &[String],
) -> Result<()> {
    let dir = match path {
        Some(dir) => {
            let module = installed::module_name(config_dir, dir)?;
            let default = installed::default_dir(config_dir, host);
            if dir.canonicalize().ok() != default.canonicalize().ok() {
                eprintln!("note: import {module:?} from a module this host loads, or archix won't see it");
            }
            std::path::absolute(dir)?
        }
        None => installed::default_dir(config_dir, host),
    };
    let file = dir.join(FILE);
    let current = match super::lock::deferred_text(&file) {
        Some(text) => Some(text),
        None => archix::captured::read_no_follow(&file)?,
    };
    let mut packages = match &current {
        Some(text) => installed::parse(text, &file)?,
        None => Vec::new(),
    };
    let elsewhere = declared_elsewhere(config_dir, host, &packages, add);
    for name in &elsewhere {
        println!(
            "{name} is already declared in the config; not adding it to {}",
            file.display()
        );
    }
    for name in add {
        if !packages.contains(name) && !elsewhere.contains(name) {
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
    let text = installed::render(&packages);
    if (current.is_none() && packages.is_empty()) || current.as_deref() == Some(text.as_str()) {
        return Ok(());
    }
    if !super::lock::defer_write(&file, &text) {
        super::lock::write_config(&file, text.as_bytes())?;
    }
    Ok(())
}

/// Of `add`, those the config declares outside installed.lua (whose own list is `listed`).
fn declared_elsewhere(config_dir: &Path, host: &str, listed: &[String], add: &[String]) -> Vec<String> {
    if add.is_empty() {
        return Vec::new();
    }
    let Ok(evaluation) = crate::eval::evaluate(&crate::eval::Input {
        config_dir: config_dir.to_path_buf(),
        hostname: host.to_string(),
        ..Default::default()
    }) else {
        return Vec::new();
    };
    let Ok(declared) = declared(&evaluation.config) else {
        return Vec::new();
    };
    add.iter()
        .filter(|name| declared.packages.contains(name) && !listed.contains(name))
        .cloned()
        .collect()
}

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

/// Searches the locked snapshot's repos and the AUR; every term has to match the name or description.
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
    eprint!("==> Install which? Numbers (1 3), ranges (2-5), ^ to leave one out: ");
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
    let archive = Archive::new(&declared.archive_url)
        .with_mirrors(&declared.archive_mirrors)
        .with_caches(&declared.package_caches);
    let date = match load_lockfile(target)?.hosts.get(target.host) {
        Some(lock) => lock.date,
        None => archive.latest()?,
    };
    let cache = user_cache()?;
    let installed = read_local_db(&super::switch::installed_db(&Paths::system())).unwrap_or_default();
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

/// `archix -Syu`: moves the lock to the newest archive snapshot and AUR packages to their newest recipes, then switches.
pub fn upgrade(target: &Target, noconfirm: bool) -> Result<()> {
    require_root("upgrade")?;
    check_pacman_free(&Paths::system())?;
    let declared = declared(target.config)?;
    let (lock, _) = current_lock(target, &declared, Snapshot::Latest, true, noconfirm, false)?;
    if lock.packages.values().any(|pkg| pkg.repo == AUR_REPO) {
        let git = aur_git(&declared)?;
        let archive = Archive::new(&declared.archive_url)
            .with_mirrors(&declared.archive_mirrors)
            .with_caches(&declared.package_caches);
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
            &declared.holds,
        )?;
        fresh.keep_pins(&lock);
        if fresh != lock {
            review(&git, Some(&lock), &fresh, declared.aur_review && !noconfirm, &confirm)?;
            let mut lockfile = load_lockfile(target)?;
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

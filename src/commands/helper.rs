use std::fs;

use anyhow::{Context, Result, bail};

use archix::archive::{ARCHIVE_URL, Archive};
use archix::aur::index::Index;
use archix::pacman::{read_local_db, read_sync_db};
use archix::paths::Paths;
use archix::resolve::snapshot_conf;

use super::lock::load_lockfile;
use super::{Target, declared, user_cache};

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
        if archix::durable::rename(&tmp, &blob).is_err() {
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

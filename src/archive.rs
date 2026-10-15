use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::date::Date;

pub const ARCHIVE_URL: &str = "https://archive.archlinux.org";
pub const ARCH: &str = "x86_64";
const SEARCH_DAYS: usize = 14;

/// The Arch Linux Archive: dated, immutable snapshots of the official repositories.
pub struct Archive {
    base: String,
    agent: ureq::Agent,
}

impl Archive {
    pub fn new(base: &str) -> Archive {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(300)))
            .user_agent(concat!("archix/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Archive {
            base: base.trim_end_matches('/').to_string(),
            agent,
        }
    }

    pub fn repo_url(&self, date: Date, repo: &str) -> String {
        format!(
            "{}/repos/{:04}/{:02}/{:02}/{repo}/os/{ARCH}",
            self.base, date.year, date.month, date.day
        )
    }

    /// The newest snapshot, searching back from today because today's may not be published yet.
    pub fn latest(&self) -> Result<Date> {
        let mut date = Date::today_utc();
        for _ in 0..SEARCH_DAYS {
            let url = format!("{}/core.db", self.repo_url(date, "core"));
            match self.agent.head(&url).call() {
                Ok(_) => return Ok(date),
                Err(ureq::Error::StatusCode(404)) => date = date.previous(),
                Err(e) => return Err(e).with_context(|| format!("checking {url}")),
            }
        }
        bail!(
            "no archive snapshot found in the last {SEARCH_DAYS} days at {}",
            self.base
        )
    }

    /// A pacman db directory for a snapshot: `sync/` holds that day's repo databases, `local/` is empty.
    pub fn snapshot_db(&self, cache: &Path, date: Date, repos: &[String]) -> Result<PathBuf> {
        let dir = cache.join("snapshots").join(date.to_string());
        let sync = dir.join("sync");
        fs::create_dir_all(&sync).with_context(|| format!("creating {}", sync.display()))?;
        fs::create_dir_all(dir.join("local"))?;
        for repo in repos {
            let path = sync.join(format!("{repo}.db"));
            if !path.is_file() {
                let url = format!("{}/{repo}.db", self.repo_url(date, repo));
                if !self.fetch(&url, &path)? {
                    bail!("{url} does not exist in the archive");
                }
            }
        }
        Ok(dir)
    }

    /// The archive's permanent copy of one package version, kept long after it leaves the repos.
    pub fn package_url(&self, name: &str, file: &str) -> String {
        let first = name.chars().next().unwrap_or('_');
        format!("{}/packages/{first}/{name}/{file}", self.base)
    }

    /// Downloads via a per-process `.part` file so partial downloads never look complete; false if missing.
    pub fn fetch(&self, url: &str, dest: &Path) -> Result<bool> {
        if let Some(path) = url.strip_prefix("file://") {
            return match fs::copy(path, dest) {
                Ok(_) => Ok(true),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(e).with_context(|| format!("copying {path}")),
            };
        }
        let mut response = match self.agent.get(url).call() {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(404)) => return Ok(false),
            Err(e) => return Err(e).with_context(|| format!("downloading {url}")),
        };
        let part = dest.with_extension(format!("part.{}", std::process::id()));
        let mut file = File::create(&part).with_context(|| format!("creating {}", part.display()))?;
        io::copy(&mut response.body_mut().as_reader(), &mut file).with_context(|| format!("downloading {url}"))?;
        file.sync_all()?;
        fs::rename(&part, dest).with_context(|| format!("renaming {}", part.display()))?;
        Ok(true)
    }
}

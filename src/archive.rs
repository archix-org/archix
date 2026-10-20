use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::date::Date;

pub const ARCHIVE_URL: &str = "https://archive.archlinux.org";
pub const ARCH: &str = "x86_64";

/// The archive `switch` last used, which rollback follows since it doesn't evaluate the config.
pub fn configured(state: &Path) -> String {
    fs::read_to_string(state.join("archive-url"))
        .map(|url| url.trim().to_string())
        .ok()
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| ARCHIVE_URL.to_string())
}

/// The archive mirrors `switch` last used, for rollback.
pub fn configured_mirrors(state: &Path) -> Vec<String> {
    fs::read_to_string(state.join("archive-mirrors"))
        .map(|text| {
            text.lines()
                .map(str::to_string)
                .filter(|line| !line.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

pub fn remember_mirrors(state: &Path, mirrors: &[String]) -> Result<()> {
    if configured_mirrors(state) == mirrors {
        return Ok(());
    }
    fs::create_dir_all(state)?;
    crate::durable::write(&state.join("archive-mirrors"), mirrors.join("\n").as_bytes())
}

/// The package caches `switch` last used, for rollback.
pub fn configured_caches(state: &Path) -> Vec<String> {
    fs::read_to_string(state.join("package-caches"))
        .map(|text| {
            text.lines()
                .map(str::to_string)
                .filter(|line| !line.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

pub fn remember_caches(state: &Path, caches: &[String]) -> Result<()> {
    if configured_caches(state) == caches {
        return Ok(());
    }
    fs::create_dir_all(state)?;
    crate::durable::write(&state.join("package-caches"), caches.join("\n").as_bytes())
}

/// The archive, its mirrors and the package caches `switch` last used, for commands that don't evaluate the config.
pub fn for_system(state: &Path) -> Archive {
    Archive::new(&configured(state))
        .with_mirrors(&configured_mirrors(state))
        .with_caches(&configured_caches(state))
}

pub fn remember(state: &Path, url: &str) -> Result<()> {
    if configured(state) == url {
        return Ok(());
    }
    fs::create_dir_all(state)?;
    fs::write(state.join("archive-url"), format!("{url}\n")).with_context(|| format!("writing {}", state.display()))
}

/// The package architectures this CPU can run: x86_64 plus each x86-64 level the dynamic loader reports as supported, for CachyOS's x86_64_v3/v4 repos.
pub fn architectures() -> String {
    let report = std::process::Command::new("/lib/ld-linux-x86-64.so.2")
        .arg("--help")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    levels(&report)
}

/// Locked package files built for an x86-64 level `supported` (as from `architectures`) doesn't include, as "name (level)".
pub fn beyond_cpu<'a>(files: impl Iterator<Item = (&'a str, &'a str)>, supported: &str) -> Vec<String> {
    files
        .filter_map(|(name, file)| {
            let arch = file.strip_suffix(".pkg.tar.zst")?.rsplit_once('-')?.1;
            (arch.starts_with("x86_64_v") && !supported.split(' ').any(|a| a == arch))
                .then(|| format!("{name} ({arch})"))
        })
        .collect()
}

fn levels(report: &str) -> String {
    let mut arches = vec![ARCH.to_string()];
    for level in ["v2", "v3", "v4"] {
        if report
            .lines()
            .any(|l| l.trim_start().starts_with(&format!("x86-64-{level} (supported")))
        {
            arches.push(format!("{ARCH}_{level}"));
        }
    }
    arches.join(" ")
}

const SEARCH_DAYS: usize = 14;

/// The Arch Linux Archive: dated, immutable snapshots of the official repositories.
pub struct Archive {
    base: String,
    agent: ureq::Agent,
    /// Package caches (directories or HTTP mirrors of archix's package store) tried before the archive.
    caches: Vec<String>,
    /// Copies of the archive with its layout, tried in order when it fails or lacks a file.
    mirrors: Vec<String>,
    /// Set once the archive fails to answer, so later requests go straight to the mirrors.
    down: AtomicBool,
}

impl Archive {
    pub fn new(base: &str) -> Archive {
        let agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_recv_response(Some(Duration::from_secs(120)))
            // ureq has no idle timeout, so the body gets an hour: a 200 MB AppImage at 60 kB/s still makes it.
            .timeout_recv_body(Some(Duration::from_secs(3600)))
            .user_agent(concat!("archix/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Archive {
            base: base.trim_end_matches('/').to_string(),
            agent,
            caches: Vec::new(),
            mirrors: Vec::new(),
            down: AtomicBool::new(false),
        }
    }

    pub fn with_mirrors(mut self, mirrors: &[String]) -> Archive {
        self.mirrors = mirrors
            .iter()
            .map(|mirror| mirror.trim_end_matches('/').to_string())
            .collect();
        self
    }

    /// Where to look for `url`: on the archive, then on each mirror; a URL not on the archive is just itself.
    fn sources(&self, url: &str) -> Vec<String> {
        let rest = match url.strip_prefix(&self.base) {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => rest,
            _ => return vec![url.to_string()],
        };
        let skip_archive = self.down.load(Ordering::Relaxed) && !self.mirrors.is_empty();
        let archive = (!skip_archive).then(|| url.to_string());
        archive
            .into_iter()
            .chain(self.mirrors.iter().map(|mirror| format!("{mirror}{rest}")))
            .collect()
    }

    /// Tries `attempt` on each source of `url` in turn until one has it; an archive that fails is skipped from then on.
    fn on_sources(&self, url: &str, attempt: impl Fn(&str) -> Result<bool>) -> Result<bool> {
        let sources = self.sources(url);
        let mut sources = sources.iter().peekable();
        while let Some(source) = sources.next() {
            let Some(next) = sources.peek() else {
                return attempt(source);
            };
            match attempt(source) {
                Ok(true) => return Ok(true),
                Ok(false) => {}
                Err(e) if crate::fetch::is_disk_full(&e) => return Err(e),
                Err(e) => {
                    if source == url {
                        self.down.store(true, Ordering::Relaxed);
                    }
                    eprintln!("warning: {e:#}; trying {next}");
                }
            }
        }
        Ok(false)
    }

    pub fn with_caches(mut self, caches: &[String]) -> Archive {
        self.caches = caches
            .iter()
            .map(|cache| cache.trim_end_matches('/').to_string())
            .collect();
        self
    }

    /// Where each package cache would hold `path` (a package file, or `aur-builds/...` for a build record).
    pub fn cache_urls(&self, path: &str) -> Vec<String> {
        self.caches.iter().map(|cache| format!("{cache}/{path}")).collect()
    }

    /// A repo snapshot on the archive and then on each mirror, for pacman's Server lines.
    pub fn repo_urls(&self, date: Date, repo: &str) -> Vec<String> {
        self.sources(&self.repo_url(date, repo))
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
            if self.exists(&url)? {
                return Ok(date);
            }
            date = date.previous();
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
        let local = dir.join("local");
        fs::create_dir_all(&local)?;
        let version = local.join("ALPM_DB_VERSION");
        if !version.exists() {
            // pacman writes this into an empty local db on first use; parallel first uses race on it.
            let tmp = local.join(format!("ALPM_DB_VERSION.{}", std::process::id()));
            fs::write(&tmp, "9\n")?;
            fs::rename(&tmp, &version)?;
        }
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

    /// Whether `url` exists on the archive or a mirror, retrying a flaky connection a few times.
    fn exists(&self, url: &str) -> Result<bool> {
        self.on_sources(url, |source| self.exists_at(source))
    }

    fn exists_at(&self, url: &str) -> Result<bool> {
        let mut attempt = 1;
        loop {
            match self.agent.head(url).call() {
                Ok(_) => return Ok(true),
                Err(ureq::Error::StatusCode(404)) => return Ok(false),
                Err(e) if attempt < ATTEMPTS => {
                    eprintln!("warning: checking {url}: {e}; trying again");
                    pause(attempt);
                }
                Err(e) => return Err(e).with_context(|| format!("checking {url}")),
            }
            attempt += 1;
        }
    }

    /// Downloads via a per-process `.part` file so partial downloads never look complete, from a mirror when the archive fails or lacks it; false if missing.
    pub fn fetch(&self, url: &str, dest: &Path) -> Result<bool> {
        self.on_sources(url, |source| self.fetch_from(source, dest))
    }

    fn fetch_from(&self, url: &str, dest: &Path) -> Result<bool> {
        let mut attempt = 1;
        loop {
            match self.fetch_once(url, dest) {
                Err(e) if attempt < ATTEMPTS && !url.starts_with("file://") && !crate::fetch::is_disk_full(&e) => {
                    eprintln!("warning: {e:#}; trying again");
                    pause(attempt);
                }
                result => return result,
            }
            attempt += 1;
        }
    }

    fn fetch_once(&self, url: &str, dest: &Path) -> Result<bool> {
        let part = dest.with_extension(format!("part.{}", std::process::id()));
        if let Some(path) = url.strip_prefix("file://") {
            match fs::copy(path, &part) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(e) => return Err(e).with_context(|| format!("copying {path}")),
            }
            fs::rename(&part, dest).with_context(|| format!("renaming {}", part.display()))?;
            return Ok(true);
        }
        let mut response = match self.agent.get(url).call() {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(404)) => return Ok(false),
            Err(e) => return Err(e).with_context(|| format!("downloading {url}")),
        };
        let mut file = File::create(&part).with_context(|| format!("creating {}", part.display()))?;
        if let Err(e) = io::copy(&mut response.body_mut().as_reader(), &mut file) {
            let _ = fs::remove_file(&part);
            return Err(e).with_context(|| format!("downloading {url}"));
        }
        file.sync_all()?;
        fs::rename(&part, dest).with_context(|| format!("renaming {}", part.display()))?;
        Ok(true)
    }
}

const ATTEMPTS: u32 = 3;

/// Waits before another attempt: 4 seconds, then 8.
fn pause(attempt: u32) {
    std::thread::sleep(Duration::from_secs(2u64 << attempt));
}

#[cfg(test)]
mod level_tests {
    #[test]
    fn only_supported_levels_are_accepted() {
        let report = "Subdirectories of glibc-hwcaps directories, in priority order:\n  x86-64-v4\n  x86-64-v3 (supported, searched)\n  x86-64-v2 (supported, searched)\n";
        assert_eq!(super::levels(report), "x86_64 x86_64_v2 x86_64_v3");
    }

    #[test]
    fn packages_beyond_the_cpu_are_named() {
        let files = [
            ("zlib", "zlib-1:1.3.2-4.1-x86_64_v4.pkg.tar.zst"),
            ("bash", "bash-5.3-1-x86_64_v3.pkg.tar.zst"),
            ("tzdata", "tzdata-2026a-1-any.pkg.tar.zst"),
            ("glibc", "glibc-2.42-1-x86_64.pkg.tar.zst"),
        ];
        assert_eq!(
            super::beyond_cpu(files.into_iter(), "x86_64 x86_64_v2 x86_64_v3"),
            ["zlib (x86_64_v4)"]
        );
    }
}

#[cfg(test)]
mod mirror_tests {
    use super::*;

    #[test]
    fn a_file_the_archive_lacks_comes_from_the_first_mirror_that_has_it() {
        let dir = tempfile::tempdir().unwrap();
        let url = |name: &str| format!("file://{}/{name}", dir.path().display());
        fs::create_dir_all(dir.path().join("second/packages/t/tree")).unwrap();
        fs::write(dir.path().join("second/packages/t/tree/tree.pkg"), "from the second").unwrap();
        let archive = Archive::new(&url("archive")).with_mirrors(&[url("first"), url("second/")]);
        let dest = dir.path().join("tree.pkg");
        assert!(archive.fetch(&archive.package_url("tree", "tree.pkg"), &dest).unwrap());
        assert_eq!(fs::read_to_string(&dest).unwrap(), "from the second");
        assert!(!archive.fetch(&archive.package_url("jq", "jq.pkg"), &dest).unwrap());
    }

    #[test]
    fn an_archive_that_failed_is_skipped_and_other_urls_stay_as_they_are() {
        let archive = Archive::new("https://archive.test").with_mirrors(&["https://mirror.test".to_string()]);
        let date: Date = "2026-09-09".parse().unwrap();
        assert_eq!(
            archive.repo_urls(date, "core"),
            [
                "https://archive.test/repos/2026/09/09/core/os/x86_64",
                "https://mirror.test/repos/2026/09/09/core/os/x86_64"
            ]
        );
        assert_eq!(
            archive.sources("https://archive.testing/x"),
            ["https://archive.testing/x"]
        );
        archive.down.store(true, Ordering::Relaxed);
        assert_eq!(archive.sources("https://archive.test/x"), ["https://mirror.test/x"]);
        assert_eq!(
            Archive::new("https://archive.test").sources("https://archive.test/x"),
            ["https://archive.test/x"]
        );
    }
}

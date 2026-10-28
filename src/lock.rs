use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::date::Date;

pub const LOCK_FILE: &str = "archix.lock";
/// The `repo` of entries pinned to an exact version from the archive's per-package history.
pub const ARCHIVE_REPO: &str = "archive";
pub const AUR_REPO: &str = "aur";
/// The `repo` of packages built from a recipe in the config: a local PKGBUILD or a patched Arch package.
pub const LOCAL_REPO: &str = "local";
const LOCK_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Lockfile {
    pub hosts: BTreeMap<String, HostLock>,
}

/// One machine's locked package set: an archive snapshot date and the exact closure resolved from it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct HostLock {
    pub date: Date,
    pub repos: Vec<String>,
    pub declared: Vec<String>,
    /// Packages that satisfy dependencies with several possible providers, installed as dependencies.
    #[serde(default)]
    pub prefer: Vec<String>,
    /// Packages held at a version other than the snapshot's, e.g. after adopting a manual upgrade.
    #[serde(default)]
    pub pinned: Vec<String>,
    /// The config's `holds` this lock was resolved with, by package.
    #[serde(default, rename = "holds")]
    pub held: BTreeMap<String, Held>,
    #[serde(default)]
    pub packages: BTreeMap<String, Locked>,
    /// Flatpak apps by id, each at an exact commit.
    #[serde(default)]
    pub flatpak: BTreeMap<String, crate::flatpak::LockedApp>,
    /// Hashes of the config's local recipes and patches the lock was resolved with.
    #[serde(default)]
    pub recipes: BTreeMap<String, String>,
}

/// One package's hold as a lock was resolved with it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Held {
    /// The hold: a version, `>=` a version, or `commit:` an AUR commit.
    pub hold: String,
    /// What the snapshot, or for AUR packages the AUR's newest recipe, has instead.
    pub newest: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Locked {
    pub version: String,
    pub repo: String,
    pub file: String,
    /// For AUR and recipe packages, the first build of the locked commit at the lock's snapshot, which package caches have to match; empty until it's built.
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub explicit: bool,
    /// The AUR package base and the commit of its build recipe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Needed only to build other AUR packages, so it's built but never installed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub build_only: bool,
    /// The AUR maintainer when locked, empty for an orphaned package; None in locks from before archix recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintainer: Option<String>,
    /// For AUR and recipe packages, what the first build's sources were, by `.SRCINFO` entry: each git source's commit and the sha256 of each download its checksums skip, which later builds reuse.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sources: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct OnDisk {
    version: u32,
    #[serde(default)]
    hosts: BTreeMap<String, HostLock>,
}

impl HostLock {
    /// The config's local recipes this lock builds from.
    pub fn local_recipes(&self) -> std::collections::BTreeSet<String> {
        self.packages
            .values()
            .filter(|pkg| pkg.repo == LOCAL_REPO)
            .filter_map(|pkg| pkg.base.clone())
            .collect()
    }
}

impl Locked {
    /// Built by archix from a recipe (AUR or the config's own) rather than downloaded.
    pub fn is_built(&self) -> bool {
        self.repo == AUR_REPO || self.repo == LOCAL_REPO
    }
}

impl HostLock {
    /// Whether this lock was resolved for the same declared packages, preferred providers, repos, recipes and holds (as specs).
    pub fn matches(
        &self,
        declared: &[String],
        prefer: &[String],
        repos: &[String],
        recipes: &BTreeMap<String, String>,
        holds: &BTreeMap<String, String>,
    ) -> bool {
        let held: BTreeMap<&String, &String> = self.held.iter().map(|(name, held)| (name, &held.hold)).collect();
        self.declared == declared
            && self.prefer == prefer
            && self.repos == repos
            && self.recipes == *recipes
            && held == holds.iter().collect::<BTreeMap<_, _>>()
    }

    pub fn pin(&mut self, name: &str, locked: Locked) {
        self.packages.insert(name.to_string(), locked);
        if !self.pinned.iter().any(|p| p == name) {
            self.pinned.push(name.to_string());
            self.pinned.sort();
        }
    }

    /// Carries over to a fresh resolution at the same date what it can't work out again: `old`'s pins, and the build hashes and pinned sources of AUR and recipe packages still at the same commit.
    pub fn keep_pins(&mut self, old: &HostLock) {
        if old.date != self.date {
            return;
        }
        for (name, fresh) in self
            .packages
            .iter_mut()
            .filter(|(_, pkg)| pkg.is_built() && pkg.sha256.is_empty())
        {
            if let Some(before) = old.packages.get(name)
                && before.commit == fresh.commit
                && before.version == fresh.version
            {
                fresh.sha256 = before.sha256.clone();
                fresh.sources = before.sources.clone();
            }
        }
        for name in &old.pinned {
            if self.held.contains_key(name) {
                continue;
            }
            if let (Some(pinned), Some(fresh)) = (old.packages.get(name), self.packages.get(name)) {
                let locked = Locked {
                    explicit: fresh.explicit,
                    ..pinned.clone()
                };
                self.pin(name, locked);
            }
        }
    }
}

impl Lockfile {
    pub fn load(path: &Path) -> Result<Lockfile> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Lockfile::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Lockfile::parse(&text, path)
    }

    /// A lock from its text; `path` names it in errors.
    pub fn parse(text: &str, path: &Path) -> Result<Lockfile> {
        let on_disk: OnDisk = toml::from_str(text).with_context(|| format!("parsing {}", path.display()))?;
        if on_disk.version != LOCK_VERSION {
            bail!(
                "{} has lock format version {}, but this archix reads version {LOCK_VERSION}",
                path.display(),
                on_disk.version
            );
        }
        Ok(Lockfile { hosts: on_disk.hosts })
    }

    /// Writes through a temp file and rename, so a crash never leaves a half-written lock.
    pub fn save(&self, path: &Path) -> Result<()> {
        crate::durable::write_config(path, self.render().as_bytes())
    }

    pub fn render(&self) -> String {
        let mut out = String::from("# Generated by archix: change it with `archix lock` or `archix update`.\n");
        let _ = writeln!(out, "version = {LOCK_VERSION}");
        for (host, lock) in &self.hosts {
            let host = key(host);
            let _ = writeln!(out, "\n[hosts.{host}]");
            let _ = writeln!(out, "date = \"{}\"", lock.date);
            let _ = writeln!(out, "repos = {}", array(&lock.repos));
            let _ = writeln!(out, "declared = {}", array(&lock.declared));
            if !lock.prefer.is_empty() {
                let _ = writeln!(out, "prefer = {}", array(&lock.prefer));
            }
            if !lock.pinned.is_empty() {
                let _ = writeln!(out, "pinned = {}", array(&lock.pinned));
            }
            if !lock.held.is_empty() {
                let held: Vec<String> = lock
                    .held
                    .iter()
                    .map(|(name, held)| {
                        format!(
                            "{} = {{ hold = {}, newest = {} }}",
                            string(name),
                            string(&held.hold),
                            string(&held.newest)
                        )
                    })
                    .collect();
                let _ = writeln!(out, "holds = {{ {} }}", held.join(", "));
            }
            let _ = writeln!(out, "\n[hosts.{host}.packages]");
            for (name, pkg) in &lock.packages {
                let mut fields = vec![
                    format!("version = {}", string(&pkg.version)),
                    format!("repo = {}", string(&pkg.repo)),
                    format!("file = {}", string(&pkg.file)),
                ];
                if !pkg.sha256.is_empty() {
                    fields.push(format!("sha256 = {}", string(&pkg.sha256)));
                }
                if let (Some(base), Some(commit)) = (&pkg.base, &pkg.commit) {
                    fields.push(format!("base = {}", string(base)));
                    fields.push(format!("commit = {}", string(commit)));
                }
                if let Some(maintainer) = &pkg.maintainer {
                    fields.push(format!("maintainer = {}", string(maintainer)));
                }
                if !pkg.sources.is_empty() {
                    let pins: Vec<String> = pkg
                        .sources
                        .iter()
                        .map(|(entry, pin)| format!("{} = {}", string(entry), string(pin)))
                        .collect();
                    fields.push(format!("sources = {{ {} }}", pins.join(", ")));
                }
                fields.push(format!("explicit = {}", pkg.explicit));
                if pkg.build_only {
                    fields.push("build_only = true".to_string());
                }
                let _ = writeln!(out, "{} = {{ {} }}", key(name), fields.join(", "));
            }
            if !lock.recipes.is_empty() {
                let _ = writeln!(out, "\n[hosts.{host}.recipes]");
                for (id, hash) in &lock.recipes {
                    let _ = writeln!(out, "{} = {}", key(id), string(hash));
                }
            }
            if !lock.flatpak.is_empty() {
                let _ = writeln!(out, "\n[hosts.{host}.flatpak]");
                for (id, app) in &lock.flatpak {
                    let _ = writeln!(
                        out,
                        "{} = {{ remote = {}, ref = {}, commit = {} }}",
                        key(id),
                        string(&app.remote),
                        string(&app.reference),
                        string(&app.commit)
                    );
                }
            }
        }
        out
    }
}

fn key(name: &str) -> String {
    let bare = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if bare { name.to_string() } else { string(name) }
}

fn string(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn array(items: &[String]) -> String {
    let items: Vec<String> = items.iter().map(|item| string(item)).collect();
    format!("[{}]", items.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Lockfile {
        let mut packages = BTreeMap::new();
        packages.insert(
            "gtk+".to_string(),
            Locked {
                version: "1:2.24-1".into(),
                repo: "extra".into(),
                file: "gtk+-1:2.24-1-x86_64.pkg.tar.zst".into(),
                sha256: "ab".into(),
                explicit: true,
                ..Default::default()
            },
        );
        packages.insert(
            "zlib".to_string(),
            Locked {
                version: "1:1.3.1-2".into(),
                repo: "core".into(),
                file: "zlib-1:1.3.1-2-x86_64.pkg.tar.zst".into(),
                sha256: "cd".into(),
                explicit: false,
                ..Default::default()
            },
        );
        packages.insert(
            "yay-bin".to_string(),
            Locked {
                version: "13.0.1-1".into(),
                repo: AUR_REPO.into(),
                file: "yay-bin-13.0.1-1-x86_64.pkg.tar.zst".into(),
                base: Some("yay-bin".into()),
                commit: Some("0123abcd".into()),
                build_only: true,
                sources: BTreeMap::from([("yay::git+https://example.org/yay.git".to_string(), "c0ffee".to_string())]),
                ..Default::default()
            },
        );
        let host = HostLock {
            date: "2026-10-14".parse().unwrap(),
            repos: vec!["core".into(), "extra".into()],
            declared: vec!["gtk+".into()],
            prefer: vec!["noto-fonts".into()],
            pinned: vec!["zlib".into()],
            held: BTreeMap::from([(
                "gtk+".to_string(),
                Held {
                    hold: "1:2.24".into(),
                    newest: "1:2.25-1".into(),
                },
            )]),
            packages,
            flatpak: BTreeMap::from([(
                "org.mozilla.firefox".to_string(),
                crate::flatpak::LockedApp {
                    remote: "flathub".into(),
                    reference: "app/org.mozilla.firefox/x86_64/stable".into(),
                    commit: "0bb3a9e4".into(),
                },
            )]),
            recipes: BTreeMap::from([("local:hello".to_string(), "abc123".to_string())]),
        };
        Lockfile {
            hosts: BTreeMap::from([("my.laptop".to_string(), host)]),
        }
    }

    #[test]
    fn round_trips_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOCK_FILE);
        let lock = sample();
        lock.save(&path).unwrap();
        assert_eq!(Lockfile::load(&path).unwrap(), lock);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("[hosts.\"my.laptop\".packages]"), "{text}");
        assert!(text.contains("\"gtk+\" = { version = \"1:2.24-1\""), "{text}");
        assert!(
            text.contains("holds = { \"gtk+\" = { hold = \"1:2.24\", newest = \"1:2.25-1\" } }"),
            "{text}"
        );
        assert!(
            text.contains("yay-bin = { version = \"13.0.1-1\", repo = \"aur\", file = \"yay-bin-13.0.1-1-x86_64.pkg.tar.zst\", base = \"yay-bin\", commit = \"0123abcd\", sources = { \"yay::git+https://example.org/yay.git\" = \"c0ffee\" }, explicit = false, build_only = true }"),
            "{text}"
        );
    }

    #[test]
    fn missing_lock_is_empty_and_future_versions_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOCK_FILE);
        assert_eq!(Lockfile::load(&path).unwrap(), Lockfile::default());
        fs::write(&path, "version = 2\n").unwrap();
        assert!(Lockfile::load(&path).unwrap_err().to_string().contains("version 2"));
    }
}

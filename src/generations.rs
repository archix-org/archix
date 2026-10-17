use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::date::Date;
use crate::lock::Locked;
use crate::paths::Paths;
use crate::system::SystemState;

const GENERATION_FILE: &str = "generation.json";

/// Everything one switch applied, so the system can be put back exactly that way later.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Generation {
    pub number: u32,
    pub created: u64,
    pub host: String,
    pub date: Date,
    pub packages: BTreeMap<String, Locked>,
    pub settings: Settings,
    pub config_commit: Option<String>,
    pub config_dirty: bool,
    #[serde(default)]
    pub system: SystemState,
    /// A read-only btrfs snapshot of / taken just before this generation was applied.
    #[serde(default)]
    pub snapshot: Option<String>,
    /// Flatpak apps this generation pinned, by id.
    #[serde(default)]
    pub flatpak: BTreeMap<String, crate::flatpak::LockedApp>,
}

/// The config settings a generation was applied with, kept so rollback and gc work without evaluating the config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub remove_undeclared: bool,
    pub ignored: Vec<String>,
    pub keep: u32,
    pub keep_days: u32,
    /// The ESP holding this generation's boot entry, when archix manages booting.
    #[serde(default)]
    pub esp: Option<String>,
    /// The boot settings the entry was built with, so changing them makes a new generation.
    #[serde(default)]
    pub boot: Option<String>,
}

impl Generation {
    /// Whether applying `other` would leave the system exactly as this generation describes it.
    pub fn same_system(&self, other: &Generation) -> bool {
        self.date == other.date
            && self.packages == other.packages
            && self.settings == other.settings
            && self.system == other.system
            && self.flatpak == other.flatpak
    }
}

pub struct Store {
    dir: PathBuf,
    current: PathBuf,
}

impl Store {
    pub fn new(paths: &Paths) -> Store {
        Store {
            dir: paths.state.join("generations"),
            current: paths.state.join("current"),
        }
    }

    fn path(&self, number: u32) -> PathBuf {
        self.dir.join(number.to_string())
    }

    pub fn list(&self) -> Result<Vec<Generation>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.dir.display())),
        };
        let mut generations = Vec::new();
        for entry in entries {
            let entry = entry?;
            if entry.file_name().to_string_lossy().parse::<u32>().is_ok() {
                // A generation a power cut tore before archix wrote it durably shouldn't hide all the others.
                match read(&entry.path()) {
                    Ok(generation) => generations.push(generation),
                    Err(e) => eprintln!("warning: skipping {}: {e:#}", entry.path().display()),
                }
            }
        }
        generations.sort_by_key(|generation| generation.number);
        Ok(generations)
    }

    pub fn get(&self, number: u32) -> Result<Generation> {
        let path = self.path(number);
        if !path.is_dir() {
            bail!("generation {number} does not exist; `archix generations` lists them");
        }
        read(&path)
    }

    pub fn current(&self) -> Result<Option<u32>> {
        match fs::read_link(&self.current) {
            Ok(target) => {
                let name = target
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                name.parse()
                    .map(Some)
                    .with_context(|| format!("{} points at {}", self.current.display(), target.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.current.display())),
        }
    }

    /// Stores a new generation under the next free number; it only becomes current through `activate`.
    pub fn create(&self, mut generation: Generation) -> Result<Generation> {
        fs::create_dir_all(&self.dir).with_context(|| format!("creating {}", self.dir.display()))?;
        let last = self.list()?.last().map_or(0, |g| g.number);
        generation.number = last + 1;
        let tmp = self.dir.join(format!(".new-{}", generation.number));
        if tmp.exists() {
            fs::remove_dir_all(&tmp)?;
        }
        fs::create_dir_all(&tmp)?;
        crate::durable::write(
            &tmp.join(GENERATION_FILE),
            serde_json::to_string_pretty(&generation)?.as_bytes(),
        )?;
        fs::rename(&tmp, self.path(generation.number))
            .with_context(|| format!("storing generation {}", generation.number))?;
        crate::durable::sync_dir(&self.path(generation.number))?;
        Ok(generation)
    }

    /// Rewrites a stored generation, such as after its snapshot was taken.
    pub fn update(&self, generation: &Generation) -> Result<()> {
        let path = self.path(generation.number).join(GENERATION_FILE);
        crate::durable::write(&path, serde_json::to_string_pretty(generation)?.as_bytes())
            .with_context(|| format!("updating generation {}", generation.number))
    }

    /// Points `current` at a generation by renaming a fresh symlink over it, which is atomic.
    pub fn activate(&self, number: u32) -> Result<()> {
        let tmp = self.current.with_extension("new");
        let _ = fs::remove_file(&tmp);
        symlink(Path::new("generations").join(number.to_string()), &tmp)?;
        fs::rename(&tmp, &self.current).with_context(|| format!("activating generation {number}"))?;
        crate::durable::sync_dir(&self.current)
    }

    pub fn delete(&self, number: u32) -> Result<()> {
        if self.current()? == Some(number) {
            bail!("generation {number} is current and can't be deleted");
        }
        fs::remove_dir_all(self.path(number)).with_context(|| format!("deleting generation {number}"))
    }
}

fn read(dir: &Path) -> Result<Generation> {
    let path = dir.join(GENERATION_FILE);
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Generations gc keeps: the current one, the newest `keep`, and every one younger than `keep_days`.
pub fn kept(generations: &[Generation], current: Option<u32>, keep: u32, keep_days: u32, now: u64) -> BTreeSet<u32> {
    let newest = generations.iter().rev().take(keep as usize).map(|g| g.number);
    let recent = generations
        .iter()
        .filter(|g| now.saturating_sub(g.created) < u64::from(keep_days) * 86_400)
        .map(|g| g.number);
    newest.chain(recent).chain(current).collect()
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The config's git commit and whether the tree has uncommitted changes, or None outside a git repo.
pub fn config_commit(config_dir: &Path) -> Option<(String, bool)> {
    let git = |args: &[&str]| -> Option<String> {
        let output = Command::new("git")
            .args(["-c", "safe.directory=*", "-C"])
            .arg(config_dir)
            .args(args)
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    let commit = git(&["rev-parse", "HEAD"])?;
    let dirty = !git(&["status", "--porcelain"])?.is_empty();
    Some((commit, dirty))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generation(number: u32, created: u64) -> Generation {
        Generation {
            number,
            created,
            host: "box".into(),
            date: "2026-10-10".parse().unwrap(),
            packages: BTreeMap::new(),
            settings: Settings {
                remove_undeclared: true,
                ignored: vec![],
                keep: 10,
                keep_days: 30,
                esp: None,
                boot: None,
            },
            config_commit: None,
            config_dirty: false,
            system: SystemState::default(),
            snapshot: None,
            flatpak: BTreeMap::new(),
        }
    }

    fn store(root: &Path) -> Store {
        let mut paths = Paths::system();
        paths.state = root.to_path_buf();
        Store::new(&paths)
    }

    #[test]
    fn creates_numbers_and_activates() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        assert_eq!(store.current().unwrap(), None);
        let first = store.create(generation(0, 1)).unwrap();
        let second = store.create(generation(0, 2)).unwrap();
        assert_eq!((first.number, second.number), (1, 2));
        store.activate(1).unwrap();
        assert_eq!(store.current().unwrap(), Some(1));
        store.activate(2).unwrap();
        assert_eq!(store.current().unwrap(), Some(2));
        assert!(store.delete(2).is_err(), "the current generation is protected");
        store.delete(1).unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn keeps_newest_recent_and_current() {
        let day = 86_400;
        let now = 100 * day;
        let generations: Vec<Generation> = (1..=6)
            .map(|n| generation(n, now - (7 - u64::from(n)) * 10 * day))
            .collect();
        assert_eq!(kept(&generations, Some(2), 2, 0, now), BTreeSet::from([2, 5, 6]));
        assert_eq!(kept(&generations, Some(6), 1, 25, now), BTreeSet::from([5, 6]));
    }
}

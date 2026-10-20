use std::collections::{BTreeMap, BTreeSet};
use std::fs;
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
    /// Flatpak apps this generation pinned, by id.
    #[serde(default)]
    pub flatpak: BTreeMap<String, crate::flatpak::LockedApp>,
    /// The system as the first switch found it, laid out as deployment 1; its packages have no locked files, so it can't be rebuilt once dropped.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub before_archix: bool,
}

/// The config settings a generation was applied with, kept so rollback and gc work without evaluating the config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub remove_undeclared: bool,
    pub ignored: Vec<String>,
    pub keep: u32,
    pub keep_days: u32,
    /// How many deployments gc keeps besides the booted, current and boot-default ones.
    #[serde(default = "default_deployments_keep")]
    pub deployments_keep: u32,
    /// How the generation's boot entry is built, so changing it makes a new generation and a rebuild builds the same entry.
    #[serde(default)]
    pub boot: Option<crate::system::boot::BootPlan>,
}

fn default_deployments_keep() -> u32 {
    2
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

/// The generation records; the current generation is the deployment /archix/current names.
pub struct Store {
    dir: PathBuf,
    current: PathBuf,
    deploy: PathBuf,
}

/// Writes a deployment's own copy of its record into its tree before the commit, which the store takes back if a crash came between the commit and saving the record.
pub fn write_into(deployment: &Path, generation: &Generation) -> Result<()> {
    crate::durable::write(
        &deployment.join(GENERATION_FILE),
        serde_json::to_string_pretty(generation)?.as_bytes(),
    )
}

impl Store {
    pub fn new(paths: &Paths) -> Store {
        Store {
            dir: paths.state.join("generations"),
            current: paths.root.join("archix/current"),
            deploy: paths.root.join("archix/deploy"),
        }
    }

    fn path(&self, number: u32) -> PathBuf {
        self.dir.join(number.to_string())
    }

    /// Whether the store itself holds generation `number`'s record.
    pub fn has(&self, number: u32) -> bool {
        self.path(number).is_dir()
    }

    /// The record committed deployment `number` holds of itself, saved to the store when the store lacks it.
    fn recovered(&self, number: u32) -> Option<Generation> {
        let deployment = self.deploy.join(number.to_string());
        if !deployment.join(GENERATION_FILE).is_file() || self.path(number).exists() {
            return None;
        }
        let generation = read(&deployment).ok().filter(|g| g.number == number)?;
        if let Err(e) = self.save(&generation) {
            eprintln!("warning: couldn't save generation {number}'s record from its deployment: {e:#}");
        }
        Some(generation)
    }

    pub fn list(&self) -> Result<Vec<Generation>> {
        let mut generations = Vec::new();
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries.collect::<std::io::Result<Vec<_>>>()?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.dir.display())),
        };
        for entry in entries {
            if entry.file_name().to_string_lossy().parse::<u32>().is_ok() {
                // A generation a power cut tore before archix wrote it durably shouldn't hide all the others.
                match read(&entry.path()) {
                    Ok(generation) => generations.push(generation),
                    Err(e) => eprintln!("warning: skipping {}: {e:#}", entry.path().display()),
                }
            }
        }
        for entry in fs::read_dir(&self.deploy).into_iter().flatten().flatten() {
            if let Some(number) = entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok())
                && let Some(generation) = self.recovered(number)
            {
                generations.push(generation);
            }
        }
        generations.sort_by_key(|generation| generation.number);
        Ok(generations)
    }

    pub fn get(&self, number: u32) -> Result<Generation> {
        let path = self.path(number);
        if !path.is_dir() {
            if let Some(generation) = self.recovered(number) {
                return Ok(generation);
            }
            bail!("generation {number} does not exist; `archix generations` lists them");
        }
        read(&path)
    }

    /// The generation of the current deployment, which boots when no boot entry picks another.
    pub fn current(&self) -> Result<Option<u32>> {
        match fs::read_link(&self.current) {
            Ok(target) => crate::deploy::store::parse_current(&target).map(Some).with_context(|| {
                format!(
                    "{} points at {}, not deploy/<number>",
                    self.current.display(),
                    target.display()
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.current.display())),
        }
    }

    /// One past the newest generation.
    pub fn next_number(&self) -> Result<u32> {
        Ok(self.list()?.last().map_or(0, |g| g.number) + 1)
    }

    /// Stores a new generation under its own number, durably and whole.
    pub fn save(&self, generation: &Generation) -> Result<()> {
        fs::create_dir_all(&self.dir).with_context(|| format!("creating {}", self.dir.display()))?;
        if self.path(generation.number).exists() {
            bail!("generation {} already exists", generation.number);
        }
        let tmp = self.dir.join(format!(".new-{}", generation.number));
        if tmp.exists() {
            fs::remove_dir_all(&tmp)?;
        }
        fs::create_dir_all(&tmp)?;
        crate::durable::write(
            &tmp.join(GENERATION_FILE),
            serde_json::to_string_pretty(generation)?.as_bytes(),
        )?;
        fs::rename(&tmp, self.path(generation.number))
            .with_context(|| format!("storing generation {}", generation.number))?;
        crate::durable::sync_dir(&self.path(generation.number))
    }

    /// Rewrites a stored generation, such as after its Flatpak apps were locked.
    pub fn update(&self, generation: &Generation) -> Result<()> {
        let path = self.path(generation.number).join(GENERATION_FILE);
        crate::durable::write(&path, serde_json::to_string_pretty(generation)?.as_bytes())
            .with_context(|| format!("updating generation {}", generation.number))
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
    use std::os::unix::fs::symlink;

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
                deployments_keep: 2,
                boot: None,
            },
            config_commit: None,
            config_dirty: false,
            system: SystemState::default(),
            flatpak: BTreeMap::new(),
            before_archix: false,
        }
    }

    #[test]
    fn a_record_lost_after_its_deployment_was_committed_comes_back_from_the_deployment() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(&Paths::under(dir.path()));
        store.save(&generation(1, 1)).unwrap();
        let deployment = dir.path().join("archix/deploy/2");
        fs::create_dir_all(&deployment).unwrap();
        write_into(&deployment, &generation(2, 2)).unwrap();
        let staged = dir.path().join("archix/deploy/.staging-3");
        fs::create_dir_all(&staged).unwrap();
        write_into(&staged, &generation(3, 3)).unwrap();
        let numbers: Vec<u32> = store.list().unwrap().iter().map(|g| g.number).collect();
        assert_eq!(numbers, [1, 2], "an uncommitted staging tree's record isn't taken");
        assert!(store.path(2).is_dir(), "the store keeps it from now on");
        fs::remove_dir_all(store.path(2)).unwrap();
        assert_eq!(store.get(2).unwrap().number, 2);
        assert!(store.get(3).is_err());
    }

    #[test]
    fn the_current_generation_is_the_current_deployment() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(&Paths::under(dir.path()));
        assert_eq!(store.current().unwrap(), None);
        assert_eq!(store.next_number().unwrap(), 1);
        store.save(&generation(1, 1)).unwrap();
        store.save(&generation(4, 2)).unwrap();
        assert!(store.save(&generation(4, 3)).is_err(), "a number is taken once");
        assert_eq!(store.next_number().unwrap(), 5);
        fs::create_dir_all(dir.path().join("archix")).unwrap();
        symlink("deploy/4", dir.path().join("archix/current")).unwrap();
        assert_eq!(store.current().unwrap(), Some(4));
        assert!(store.delete(4).is_err(), "the current generation is protected");
        store.delete(1).unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
        fs::remove_file(dir.path().join("archix/current")).unwrap();
        symlink("generations/4", dir.path().join("archix/current")).unwrap();
        assert!(store.current().is_err());
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

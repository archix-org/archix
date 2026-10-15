use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::pacman::LocalPkg;
use crate::system::apply::SystemChange;

const JOURNAL_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Switch,
    Rollback,
}

/// Apply steps in order; each one only moves the system toward the transaction's end state, so re-running is safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Step {
    Install,
    Remove,
    Reasons,
    Users,
    StopServices,
    Files,
    Reload,
    Services,
    Verify,
    Activate,
}

pub const STEPS: [Step; 10] = [
    Step::Install,
    Step::Remove,
    Step::Reasons,
    Step::Users,
    Step::StopServices,
    Step::Files,
    Step::Reload,
    Step::Services,
    Step::Verify,
    Step::Activate,
];

impl Step {
    pub fn describe(self) -> &'static str {
        match self {
            Step::Install => "installing packages",
            Step::Remove => "removing packages",
            Step::Reasons => "setting install reasons",
            Step::Users => "updating users and groups",
            Step::StopServices => "stopping and disabling services",
            Step::Files => "writing managed files",
            Step::Reload => "reloading systemd, sysctl, udev and locales",
            Step::Services => "enabling and starting services",
            Step::Verify => "verifying the result",
            Step::Activate => "making the new generation current",
        }
    }
}

/// The package changes of one switch or rollback, fully prepared before anything on the system changes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Transaction {
    pub install: Vec<PathBuf>,
    pub remove: Vec<String>,
    /// Version every touched or kept package must have afterwards.
    pub versions: BTreeMap<String, String>,
    /// Whether each package must end up explicitly installed (true) or as a dependency.
    pub reasons: BTreeMap<String, bool>,
    /// Files, users and services, when the transaction manages them.
    #[serde(default)]
    pub system: Option<SystemChange>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Journal {
    pub version: u32,
    pub kind: Kind,
    pub host: String,
    pub target: String,
    pub started: u64,
    pub before: BTreeMap<String, LocalPkg>,
    pub transaction: Transaction,
    /// The generation that becomes current in the last step; None for recovery rollbacks.
    #[serde(default)]
    pub generation: Option<u32>,
    pub done: Vec<Step>,
}

impl Journal {
    pub fn new(
        kind: Kind,
        host: &str,
        target: String,
        before: BTreeMap<String, LocalPkg>,
        transaction: Transaction,
    ) -> Self {
        let started = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        Journal {
            version: JOURNAL_VERSION,
            kind,
            host: host.to_string(),
            target,
            started,
            before,
            transaction,
            generation: None,
            done: Vec::new(),
        }
    }

    pub fn next_step(&self) -> Option<Step> {
        STEPS.into_iter().find(|step| !self.done.contains(step))
    }

    pub fn load(path: &Path) -> Result<Option<Journal>> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let journal: Journal = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if journal.version != JOURNAL_VERSION {
            bail!(
                "{} has journal version {}, which this archix can't read",
                path.display(),
                journal.version
            );
        }
        Ok(Some(journal))
    }

    /// Written via temp file, fsync and rename, so a power cut leaves either the old or the new journal.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        let mut file = File::create(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
        file.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
        if let Some(dir) = path.parent() {
            File::open(dir)?.sync_all()?;
        }
        Ok(())
    }

    pub fn delete(path: &Path) -> Result<()> {
        match fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("removing {}", path.display()))
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_tracks_steps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/journal.json");
        let mut journal = Journal::new(
            Kind::Switch,
            "box",
            "snapshot 2026-10-10".into(),
            BTreeMap::new(),
            Transaction::default(),
        );
        assert_eq!(journal.next_step(), Some(Step::Install));
        journal.done.push(Step::Install);
        journal.save(&path).unwrap();
        let loaded = Journal::load(&path).unwrap().unwrap();
        assert_eq!(loaded, journal);
        assert_eq!(loaded.next_step(), Some(Step::Remove));
        Journal::delete(&path).unwrap();
        assert!(Journal::load(&path).unwrap().is_none());
    }
}

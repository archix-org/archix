use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::pacman::LocalPkg;
use crate::system::apply::SystemChange;
use crate::system::boot::BootPlan;

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
    Remove,
    Install,
    Reasons,
    Users,
    StopServices,
    Files,
    Reload,
    Services,
    Restart,
    Verify,
    Boot,
    Activate,
}

/// Installs come first, so pacman and what it needs are never missing in between; dropping what the lock leaves out comes after.
pub const STEPS: [Step; 12] = [
    Step::Install,
    Step::Remove,
    Step::Reasons,
    Step::Users,
    Step::StopServices,
    Step::Files,
    Step::Reload,
    Step::Services,
    Step::Restart,
    Step::Verify,
    Step::Boot,
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
            Step::Restart => "restarting what uses replaced files",
            Step::Verify => "verifying the result",
            Step::Boot => "writing the boot entry",
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
    /// The boot entry to write, when archix manages booting.
    #[serde(default)]
    pub boot: Option<BootPlan>,
    /// Some install is a rebuild at the installed version, so pacman mustn't skip up-to-date packages.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reinstall: bool,
    /// A boot-time rollback, which runs before sysinit.target: units are queued with --no-block instead of waited for, and nothing is restarted.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub at_boot: bool,
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
        crate::durable::write(path, serde_json::to_string_pretty(self)?.as_bytes())
    }

    /// Durable like `save`: a power cut right after a finished switch must not bring its journal back.
    pub fn delete(path: &Path) -> Result<()> {
        match fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("removing {}", path.display()))
            }
            Err(_) => Ok(()),
            Ok(()) => crate::durable::sync_dir(path),
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

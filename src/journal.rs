use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::pacman::LocalPkg;
use crate::system::apply::SystemChange;
use crate::system::boot::BootPlan;

/// The newest journal this archix can finish; each journal records the oldest archix that can, so older ones still finish what they understand.
const JOURNAL_VERSION: u32 = 2;

/// A copy of the archix that started the open journal, next to it, for an older archix to hand over to.
const WRITER: &str = "journal-archix";
const HANDED_OVER: &str = "ARCHIX_HANDED_OVER";

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
    /// Config files (archix.lock, installed.lua) the switch writes when it makes its generation current, and not before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config: Vec<ConfigWrite>,
}

/// One config file's new text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigWrite {
    pub path: PathBuf,
    pub text: String,
}

/// Writes config files keeping their owner, as the last thing a committed switch does.
pub fn write_config(writes: &[ConfigWrite]) -> anyhow::Result<()> {
    for write in writes {
        crate::durable::write_config(&write.path, write.text.as_bytes())?;
    }
    Ok(())
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
        // An archix before version 2 skips config writes it doesn't know about.
        let version = if transaction.config.is_empty() { 1 } else { 2 };
        Journal {
            version,
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
        if journal.version > JOURNAL_VERSION {
            return Err(hand_over(path, journal.version));
        }
        Ok(Some(journal))
    }

    /// Saves a new journal, after keeping a copy of this archix next to it: a rollback can install an older archix before it finishes.
    pub fn begin(&self, path: &Path) -> Result<()> {
        let exe = std::env::current_exe().context("finding the archix binary")?;
        // The initramfs can only run the static build, which the package installs next to the dynamic one.
        let packaged = Path::new(crate::integration::STATIC_BINARY);
        let source = if exe == Path::new("/usr/bin/archix") && packaged.exists() {
            packaged
        } else {
            &exe
        };
        self.begin_with(path, source)
    }

    fn begin_with(&self, path: &Path, archix: &Path) -> Result<()> {
        let binary = fs::read(archix).with_context(|| format!("reading {}", archix.display()))?;
        crate::durable::write_executable(&path.with_file_name(WRITER), &binary)?;
        self.save(path)
    }

    /// Written via temp file, fsync and rename, so a power cut leaves either the old or the new journal.
    pub fn save(&self, path: &Path) -> Result<()> {
        crate::durable::write(path, serde_json::to_string_pretty(self)?.as_bytes())
    }

    /// Durable like `save`: a power cut right after a finished switch must not bring its journal back.
    pub fn delete(path: &Path) -> Result<()> {
        let _ = fs::remove_file(path.with_file_name(WRITER));
        match fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("removing {}", path.display()))
            }
            Err(_) => Ok(()),
            Ok(()) => crate::durable::sync_dir(path),
        }
    }
}

/// Runs the archix that started a journal too new for this one with the same arguments, or says how to finish it.
fn hand_over(path: &Path, version: u32) -> anyhow::Error {
    let writer = path.with_file_name(WRITER);
    let trusted = fs::metadata(&writer).is_ok_and(|meta| meta.is_file() && meta.uid() == 0 && meta.mode() & 0o022 == 0);
    if trusted && std::env::var_os(HANDED_OVER).is_none() {
        eprintln!("{} was started by a newer archix; handing over to it", path.display());
        let error = Command::new(&writer)
            .args(std::env::args_os().skip(1))
            .env(HANDED_OVER, "1")
            .exec();
        eprintln!("warning: couldn't run {}: {error}", writer.display());
    }
    anyhow!(
        "{} needs journal version {version}, but this archix reads up to version {JOURNAL_VERSION}; finish it with the archix that started it ({}), or install a newer archix",
        path.display(),
        writer.display()
    )
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
        let archix = dir.path().join("archix");
        fs::write(&archix, "#!/bin/sh\n").unwrap();
        journal.begin_with(&path, &archix).unwrap();
        let writer = path.with_file_name(WRITER);
        assert_eq!(fs::read(&writer).unwrap(), b"#!/bin/sh\n");
        assert_eq!(fs::metadata(&writer).unwrap().mode() & 0o777, 0o755);
        Journal::delete(&path).unwrap();
        assert!(Journal::load(&path).unwrap().is_none());
        assert!(!writer.exists(), "the copy goes with the journal");
    }

    #[test]
    fn records_the_oldest_archix_that_can_finish_it() {
        let rollback = Journal::new(
            Kind::Rollback,
            "box",
            "generation 3".into(),
            BTreeMap::new(),
            Transaction::default(),
        );
        assert_eq!(
            rollback.version, 1,
            "an archix from before config writes can finish a rollback"
        );
        let transaction = Transaction {
            config: vec![ConfigWrite {
                path: "/etc/archix/archix.lock".into(),
                text: "version = 1\n".into(),
            }],
            ..Transaction::default()
        };
        let switch = Journal::new(
            Kind::Switch,
            "box",
            "snapshot 2026-10-10".into(),
            BTreeMap::new(),
            transaction,
        );
        assert_eq!(
            switch.version, 2,
            "an archix that ignores config writes must not finish a switch"
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.json");
        let written_by_1 = r#"{"version":1,"kind":"switch","host":"box","target":"snapshot 2026-06-01","started":1,"before":{},
            "transaction":{"install":[],"remove":["tree"],"versions":{},"reasons":{}},"done":["install"]}"#;
        fs::write(&path, written_by_1).unwrap();
        let loaded = Journal::load(&path).unwrap().unwrap();
        assert_eq!(
            (loaded.version, loaded.transaction.remove),
            (1, vec!["tree".to_string()])
        );

        let mut newer = serde_json::to_value(&switch).unwrap();
        newer["version"] = 99.into();
        fs::write(&path, newer.to_string()).unwrap();
        let error = format!("{:#}", Journal::load(&path).unwrap_err());
        assert!(
            error.contains("version 99") && error.contains("journal-archix"),
            "{error}"
        );
    }
}

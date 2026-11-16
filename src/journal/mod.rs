//! SCOPE 4's journal: one directory per switch under /var/log/archix/switches, written by the switch and read by `archix log`.
pub mod read;
pub mod verify;
pub mod writemap;
pub mod writer;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub use writer::JournalWriter;

pub const SWITCHES: &str = "/var/log/archix/switches";

/// One switch's journal directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Journal {
    pub dir: PathBuf,
    pub n: u32,
}

impl Journal {
    pub fn plan_json(&self) -> PathBuf {
        self.dir.join("plan.json")
    }

    pub fn config_tar(&self) -> PathBuf {
        self.dir.join("config.tar")
    }

    pub fn meta_json(&self) -> PathBuf {
        self.dir.join("meta.json")
    }

    pub fn journal_jsonl(&self) -> PathBuf {
        self.dir.join("journal.jsonl")
    }
}

/// The switch steps in the order they run.
pub const STEPS: [&str; 14] = [
    "plan",
    "builds",
    "lock",
    "snapshot-pre",
    "install",
    "files",
    "users",
    "services",
    "remove",
    "snapshot-post",
    "unlock",
    "home",
    "flatpak",
    "finish",
];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlpmWindow {
    pub start: Option<String>,
    pub end: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    Running,
    Ok,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub n: u32,
    pub archix_version: String,
    pub started: String,
    pub snapper_pre: Option<u32>,
    pub snapper_post: Option<u32>,
    pub alpm_window: AlpmWindow,
    /// Each home-manager user's store path built in step 2.
    pub home: BTreeMap<String, String>,
    pub outcome: Outcome,
    pub failed_step: Option<String>,
}

impl Meta {
    pub fn new(n: u32, started: String) -> Meta {
        Meta {
            n,
            archix_version: env!("CARGO_PKG_VERSION").to_string(),
            started,
            snapper_pre: None,
            snapper_post: None,
            alpm_window: AlpmWindow::default(),
            home: BTreeMap::new(),
            outcome: Outcome::Running,
            failed_step: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActionKind {
    Install,
    Upgrade,
    Downgrade,
    Remove,
    FileWrite,
    FileDelete,
    User,
    Group,
    UnitEnable,
    UnitDisable,
    UnitStart,
    UnitRestart,
    UnitReload,
    FlatpakInstall,
    FlatpakRemove,
    HomeActivate,
    SyncDbReplace,
    MarkExplicit,
    MarkDependency,
    UnitMask,
    UnitUnmask,
}

/// One line of journal.jsonl.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum Event {
    StepStart {
        t: String,
        step: String,
    },
    StepEnd {
        t: String,
        step: String,
        ok: bool,
        detail: String,
    },
    Action {
        t: String,
        step: String,
        kind: ActionKind,
        target: String,
    },
}

pub fn switches_dir(root: &Path) -> PathBuf {
    root.join(SWITCHES.trim_start_matches('/'))
}

/// The switch numbers in the journal directory under `root`, ascending; none when it doesn't exist yet.
pub fn list(root: &Path) -> Result<Vec<u32>> {
    let dir = switches_dir(root);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut out: Vec<u32> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .collect();
    out.sort_unstable();
    Ok(out)
}

/// `time` as RFC 3339 in UTC with seconds, the journal's timestamp format.
pub fn rfc3339(time: SystemTime) -> String {
    let secs = time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

pub fn now() -> String {
    rfc3339(SystemTime::now())
}

#[cfg(test)]
pub(crate) fn read_lines(journal: &Journal) -> Result<Vec<Event>> {
    fs::read_to_string(journal.journal_jsonl())?
        .lines()
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn timestamps_are_rfc3339_utc() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(1_794_787_200 + 3_723)),
            "2026-11-16T01:02:03Z"
        );
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29T00:00:00Z"
        );
    }

    #[test]
    fn events_serialize_as_the_interface_fixes() {
        let line = serde_json::to_string(&Event::Action {
            t: "x".into(),
            step: "install".into(),
            kind: ActionKind::SyncDbReplace,
            target: "core".into(),
        })
        .unwrap();
        assert_eq!(
            line,
            r#"{"event":"action","t":"x","step":"install","kind":"sync-db-replace","target":"core"}"#
        );
        let start = serde_json::to_string(&Event::StepStart {
            t: "x".into(),
            step: "lock".into(),
        })
        .unwrap();
        assert_eq!(start, r#"{"event":"step-start","t":"x","step":"lock"}"#);
        let end: Event =
            serde_json::from_str(r#"{"t":"x","event":"step-end","step":"files","ok":false,"detail":"boom"}"#).unwrap();
        assert!(matches!(end, Event::StepEnd { ok: false, .. }));
    }

    #[test]
    fn meta_has_every_field_the_interface_names() {
        let meta = Meta::new(3, "t".into());
        let value = serde_json::to_value(&meta).unwrap();
        for key in [
            "n",
            "archix_version",
            "started",
            "snapper_pre",
            "snapper_post",
            "alpm_window",
            "home",
            "outcome",
            "failed_step",
        ] {
            assert!(value.get(key).is_some(), "{key} missing from {value}");
        }
        assert_eq!(value["outcome"], "running");
        assert_eq!(value["alpm_window"]["start"], serde_json::Value::Null);
    }

    #[test]
    fn list_reads_only_numbered_directories() {
        let root = tempfile::tempdir().unwrap();
        assert!(list(root.path()).unwrap().is_empty());
        let dir = switches_dir(root.path());
        for name in ["2", "10", "1", "junk"] {
            fs::create_dir_all(dir.join(name)).unwrap();
        }
        fs::write(dir.join("7"), "").unwrap();
        assert_eq!(list(root.path()).unwrap(), [1, 2, 10]);
    }
}

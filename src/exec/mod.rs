//! The switch's executor: SCOPE 2.2's steps in order over the one Plan diff prints, each recorded in the journal (SCOPE 4).
pub mod compare;
pub mod dblock;
pub mod failure;
pub mod live;
pub mod output;
pub mod signals;
pub mod snapshot;
pub mod transaction;

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::journal::{JournalWriter, Outcome, STEPS};
use crate::plan;

/// Builds AUR packages in a clean chroot (SCOPE 2.4); the aurchroot branch's DevtoolsBuilder implements it.
pub trait AurBuilder {
    fn build(&self, items: &[plan::AurItem], conf: &Path, sync_dbs: &Path) -> Result<Vec<PathBuf>>;
}

/// Builds and activates a user's home-manager generation (SCOPE 7); the hm branch's Runner implements it.
pub trait HomeManager {
    fn build(&self, user: &str, flake_dir: &Path) -> Result<PathBuf>;
    fn activate(&self, user: &str, store_path: &Path) -> Result<()>;
}

/// Stands in for the chroot builder until it merges.
pub struct PendingAur;

impl AurBuilder for PendingAur {
    fn build(&self, _: &[plan::AurItem], _: &Path, _: &Path) -> Result<Vec<PathBuf>> {
        bail!("AUR builds land with the chroot builder")
    }
}

/// Stands in for the home-manager runner until it merges.
pub struct PendingHome;

impl HomeManager for PendingHome {
    fn build(&self, _: &str, _: &Path) -> Result<PathBuf> {
        bail!("home-manager builds land with the home-manager runner")
    }
    fn activate(&self, _: &str, _: &Path) -> Result<()> {
        bail!("home-manager activation lands with the home-manager runner")
    }
}

/// A step that stopped the switch without changing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused(pub String);

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

/// The steps that change the system; a failure in or after one is a partway failure (exit 3).
pub const WRITES: [&str; 7] = ["install", "files", "users", "services", "remove", "home", "flatpak"];

/// What runs each step; `run` returns the step's journal detail.
pub trait Steps {
    fn run(&mut self, step: &str, journal: &mut JournalWriter) -> Result<String>;
    /// Whether db.lck is held, so a failure must still release it.
    fn holds_lock(&self) -> bool;
}

pub mod exit {
    pub const OK: u8 = 0;
    pub const REFUSED: u8 = 2;
    pub const PARTWAY: u8 = 3;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub outcome: Outcome,
    pub failed_step: Option<String>,
    pub error: Option<String>,
    /// Whether a step that changes the system had started.
    pub wrote: bool,
}

impl Report {
    pub fn exit_code(&self) -> u8 {
        match (self.outcome, self.wrote) {
            (Outcome::Ok, _) => exit::OK,
            (_, false) => exit::REFUSED,
            (_, true) => exit::PARTWAY,
        }
    }
}

/// Runs every step in SCOPE 2.2's order, stopping at the first failure or signal; a held lock is released either way.
pub fn execute(steps: &mut dyn Steps, journal: &mut JournalWriter, interrupted: &dyn Fn() -> bool) -> Report {
    let mut wrote = false;
    let mut stopped: Option<(Outcome, String, String)> = None;
    for step in STEPS {
        if interrupted() {
            stopped = Some((Outcome::Interrupted, step.to_string(), "interrupted by a signal".into()));
            break;
        }
        if let Err(e) = journal.step_start(step) {
            stopped = Some((Outcome::Failed, step.to_string(), format!("{e:#}")));
            break;
        }
        match steps.run(step, journal) {
            Ok(detail) => {
                wrote |= WRITES.contains(&step);
                if let Err(e) = journal.step_end(step, true, &detail) {
                    stopped = Some((Outcome::Failed, step.to_string(), format!("{e:#}")));
                    break;
                }
            }
            Err(e) => {
                let refused = e.downcast_ref::<Refused>().is_some();
                wrote |= WRITES.contains(&step) && !refused;
                let text = format!("{e:#}");
                let _ = journal.step_end(step, false, &text);
                let outcome = match interrupted() {
                    true => Outcome::Interrupted,
                    false => Outcome::Failed,
                };
                stopped = Some((outcome, step.to_string(), text));
                break;
            }
        }
    }
    let Some((outcome, step, error)) = stopped else {
        if let Err(e) = journal.update_meta(|m| m.outcome = Outcome::Ok) {
            output::warn(&format!("warning: couldn't record the outcome in the journal: {e:#}"));
        }
        return Report {
            outcome: Outcome::Ok,
            failed_step: None,
            error: None,
            wrote,
        };
    };
    if steps.holds_lock() {
        let _ = journal.step_start("unlock");
        let released = steps.run("unlock", journal);
        let detail = match &released {
            Ok(detail) => detail.clone(),
            Err(e) => format!("{e:#}"),
        };
        let _ = journal.step_end("unlock", released.is_ok(), &detail);
        if let Err(e) = released {
            output::warn(&format!("error: releasing pacman's lock: {e:#}"));
        }
    }
    let failed = step.clone();
    if let Err(e) = journal.update_meta(|m| {
        m.outcome = outcome;
        m.failed_step = Some(failed);
    }) {
        output::warn(&format!("warning: couldn't record the outcome in the journal: {e:#}"));
    }
    Report {
        outcome,
        failed_step: Some(step),
        error: Some(error),
        wrote,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::journal::{Event, Meta, read_lines};

    #[derive(Default)]
    struct Fake {
        calls: Vec<String>,
        fail_at: Option<(&'static str, bool)>,
        locked: bool,
    }

    impl Steps for Fake {
        fn run(&mut self, step: &str, _: &mut JournalWriter) -> Result<String> {
            self.calls.push(step.to_string());
            match step {
                "lock" => self.locked = true,
                "unlock" => self.locked = false,
                _ => {}
            }
            if let Some((at, refused)) = self.fail_at
                && at == step
            {
                if step == "lock" {
                    self.locked = false;
                }
                return match refused {
                    true => Err(Refused(format!("{step} refused")).into()),
                    false => Err(anyhow::anyhow!("{step} broke")),
                };
            }
            Ok(format!("{step} done"))
        }
        fn holds_lock(&self) -> bool {
            self.locked
        }
    }

    struct Run {
        _root: tempfile::TempDir,
        report: Report,
        calls: Vec<String>,
        events: Vec<Event>,
        meta: Meta,
    }

    fn run(fake: Fake, interrupted: &dyn Fn() -> bool) -> Run {
        let root = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let mut journal = JournalWriter::create(root.path(), "{}", tree.path()).unwrap();
        let mut fake = fake;
        let report = execute(&mut fake, &mut journal, interrupted);
        let j = journal.journal().clone();
        Run {
            report,
            calls: fake.calls,
            events: read_lines(&j).unwrap(),
            meta: serde_json::from_str(&std::fs::read_to_string(j.meta_json()).unwrap()).unwrap(),
            _root: root,
        }
    }

    fn never() -> bool {
        false
    }

    #[test]
    fn every_step_runs_once_in_scopes_order_with_a_start_and_end_record() {
        let r = run(Fake::default(), &never);
        assert_eq!(r.calls, STEPS);
        assert_eq!(r.report.exit_code(), exit::OK);
        assert_eq!(r.meta.outcome, Outcome::Ok);
        let records: Vec<String> = r
            .events
            .iter()
            .map(|e| match e {
                Event::StepStart { step, .. } => format!("start {step}"),
                Event::StepEnd { step, ok, .. } => format!("end {step} {ok}"),
                Event::Action { .. } => "action".into(),
            })
            .collect();
        let expected: Vec<String> = STEPS
            .iter()
            .flat_map(|s| [format!("start {s}"), format!("end {s} true")])
            .collect();
        assert_eq!(records, expected);
        let lock = STEPS.iter().position(|s| *s == "lock").unwrap();
        let install = STEPS.iter().position(|s| *s == "install").unwrap();
        let remove = STEPS.iter().position(|s| *s == "remove").unwrap();
        let unlock = STEPS.iter().position(|s| *s == "unlock").unwrap();
        let pre = STEPS.iter().position(|s| *s == "snapshot-pre").unwrap();
        let builds = STEPS.iter().position(|s| *s == "builds").unwrap();
        assert!(builds < lock && lock < pre && pre < install && install < remove && remove < unlock);
    }

    #[test]
    fn a_refusal_before_any_write_exits_2_and_releases_a_held_lock() {
        for (at, refused) in [("builds", false), ("lock", true), ("snapshot-pre", true)] {
            let r = run(
                Fake {
                    fail_at: Some((at, refused)),
                    ..Fake::default()
                },
                &never,
            );
            assert_eq!(r.report.exit_code(), exit::REFUSED, "{at}");
            assert_eq!(r.meta.outcome, Outcome::Failed);
            assert_eq!(r.meta.failed_step.as_deref(), Some(at));
            assert!(
                !r.calls.iter().any(|c| WRITES.contains(&c.as_str())),
                "{at}: nothing that writes ran: {:?}",
                r.calls
            );
            let released = r.calls.last().map(String::as_str) == Some("unlock");
            assert_eq!(released, at == "snapshot-pre", "{at}: {:?}", r.calls);
        }
    }

    #[test]
    fn a_transaction_refused_before_commit_still_exits_2() {
        let r = run(
            Fake {
                fail_at: Some(("install", true)),
                ..Fake::default()
            },
            &never,
        );
        assert_eq!(r.report.exit_code(), exit::REFUSED);
        assert_eq!(r.calls.last().map(String::as_str), Some("unlock"));
    }

    #[test]
    fn a_failure_partway_exits_3_stops_there_and_releases_the_lock() {
        let r = run(
            Fake {
                fail_at: Some(("services", false)),
                ..Fake::default()
            },
            &never,
        );
        assert_eq!(r.report.exit_code(), exit::PARTWAY);
        assert_eq!(
            r.calls,
            [
                "plan",
                "builds",
                "lock",
                "snapshot-pre",
                "install",
                "files",
                "users",
                "services",
                "unlock"
            ]
        );
        assert_eq!(r.meta.failed_step.as_deref(), Some("services"));
        let failed = r
            .events
            .iter()
            .find(|e| matches!(e, Event::StepEnd { ok: false, .. }))
            .unwrap();
        assert!(
            matches!(failed, Event::StepEnd { step, detail, .. } if step == "services" && detail == "services broke")
        );
    }

    #[test]
    fn a_signal_stops_between_steps_as_interrupted() {
        let count = Cell::new(0);
        let after_install = || {
            count.set(count.get() + 1);
            count.get() > 6
        };
        let r = run(Fake::default(), &after_install);
        assert_eq!(r.report.outcome, Outcome::Interrupted);
        assert_eq!(r.report.exit_code(), exit::PARTWAY);
        assert_eq!(r.meta.outcome, Outcome::Interrupted);
        assert_eq!(r.calls.last().map(String::as_str), Some("unlock"));
        assert!(!r.calls.contains(&"users".to_string()), "{:?}", r.calls);
        let early = run(Fake::default(), &|| true);
        assert_eq!(early.report.exit_code(), exit::REFUSED);
        assert!(early.calls.is_empty());
    }
}

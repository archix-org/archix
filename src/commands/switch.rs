use std::io::BufRead;
use std::path::{Path, PathBuf};

use anyhow::Result;

use archix::exec::live::{Inputs, Live, Options};
use archix::exec::output::{raw, say, warn};
use archix::exec::{PendingAur, PendingHome, dblock, exit, failure, signals, snapshot};
use archix::journal::{JournalWriter, Outcome};
use archix::plan::fingerprint::{self, SavedPlan};
use archix::plan::{Plan, record, render};

use super::diff::{Computed, compute};

pub const REFUSED: u8 = exit::REFUSED;

pub struct SwitchArgs {
    pub noconfirm: bool,
    pub no_snapshot: bool,
    pub plan: Option<PathBuf>,
}

fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

fn refuse(text: &str) -> u8 {
    warn(&format!("archix switch refuses: {text}"));
    exit::REFUSED
}

/// Why the recomputed plan can't be executed in place of the one saved at `path`, if it can't.
fn saved_plan_moved(path: &Path, computed: &Computed) -> Result<Vec<String>> {
    let saved = SavedPlan::load(path)?;
    let mut moved = fingerprint::compare(&saved.fingerprint, &computed.fingerprint);
    if moved.is_empty() && saved.plan != computed.plan {
        moved.push("the plan computed now differs from the saved one".into());
    }
    Ok(moved)
}

/// Whether the user answered yes; anything else, end of input included, is no.
fn confirmed(input: &mut dyn BufRead) -> bool {
    raw("Apply this plan? [y/N] ");
    let mut answer = String::new();
    if input.read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// What stops a plan from being executed at all: refusals, unchecked items, a missing snapshot setup.
fn blockers(plan: &Plan, snapper_configured: bool, no_snapshot: bool) -> Vec<String> {
    let mut out: Vec<String> = plan.refusals.clone();
    out.extend(
        plan.unchecked
            .iter()
            .map(|item| format!("not checked, so the plan is incomplete: {item}")),
    );
    if plan.has_changes() && !snapper_configured && !no_snapshot {
        out.push(
            "snapper isn't set up, so there would be no pre snapshot; switch with --no-snapshot to go without one"
                .into(),
        );
    }
    out
}

/// `archix switch`: the plan diff prints, asked once (or checked against a saved plan), then executed (SCOPE 2.2, 3.4); returns 0, 2 (refused, nothing written) or 3 (failed partway).
pub fn switch(config_dir: &Path, host: &str, args: &SwitchArgs) -> Result<u8> {
    if !is_root() {
        return Ok(refuse("it changes the system, so it needs root; run it with sudo"));
    }
    let computed = match compute(config_dir, host, false) {
        Ok(computed) => computed,
        Err(e) => return Ok(refuse(&format!("{e:#}; nothing was changed"))),
    };
    let plan = &computed.plan;
    match &args.plan {
        Some(path) => {
            let moved = match saved_plan_moved(path, &computed) {
                Ok(moved) => moved,
                Err(e) => return Ok(refuse(&format!("{e:#}"))),
            };
            if !moved.is_empty() {
                let list: Vec<String> = moved.iter().map(|m| format!("  {m}")).collect();
                return Ok(refuse(&format!(
                    "{} no longer holds:\n{}\nrun diff again; nothing was changed",
                    path.display(),
                    list.join("\n")
                )));
            }
        }
        None => raw(&render::human(plan)),
    }
    let blocked = blockers(plan, computed.snapper_configured, args.no_snapshot);
    if !blocked.is_empty() {
        return Ok(refuse(&format!("{}\nnothing was changed", blocked.join("\n"))));
    }
    if !plan.has_changes() {
        say("nothing to do");
        return Ok(exit::OK);
    }
    if args.plan.is_none() && !args.noconfirm && !confirmed(&mut std::io::stdin().lock()) {
        say("nothing was changed");
        return Ok(exit::REFUSED);
    }
    execute(computed, args)
}

fn execute(computed: Computed, args: &SwitchArgs) -> Result<u8> {
    let _plan_dbs = match dblock::hold_plan_dbpath(&computed.dbpath) {
        Ok(file) => file,
        Err(e) => return Ok(refuse(&format!("{e:#}"))),
    };
    signals::install();
    // Set before any thread exists; snap-pac's hooks then leave the snapshots to archix's pair.
    unsafe { std::env::set_var("SNAP_PAC_SKIP", "y") };
    let paths = archix::paths::Paths::system();
    let mut journal = match JournalWriter::create(&paths.root, &render::json(&computed.plan), &computed.evaluation.tree)
    {
        Ok(journal) => journal,
        Err(e) => return Ok(refuse(&format!("opening the journal: {e:#}"))),
    };
    let config = computed.evaluation.config.clone();
    let esp = archix::system::fstab::esp_of(&config).to_string();
    let fstab = archix::system::fstab::render(&config, &esp)?.lines;
    let inputs = Inputs {
        fingerprint: computed.fingerprint,
        conf: computed.conf,
        plan_dbpath: computed.dbpath,
        rendered: computed.rendered,
        last: computed.last,
        record: record::default_path(),
        flatpak: computed.lock.map(|l| l.flatpak).unwrap_or_default(),
        archive_url: computed.archive_url,
        secret_hmacs: computed.secret_hmacs.unwrap_or_default(),
        fstab,
        esp,
        options: Options {
            no_snapshot: args.no_snapshot,
            snapper_configured: computed.snapper_configured,
            entry_timeout: snapshot::ENTRY_TIMEOUT,
        },
        plan: computed.plan,
        config,
        paths,
    };
    let mut live = Live::new(inputs, Box::new(PendingAur), Box::new(PendingHome));
    let report = archix::exec::execute(&mut live, &mut journal, &signals::interrupted);
    let dir = journal.journal().dir.clone();
    match (report.outcome, report.wrote) {
        (Outcome::Ok, _) => say(&format!(
            "switch {} done; its journal is {}",
            journal.journal().n,
            dir.display()
        )),
        (_, false) => warn(&format!(
            "archix switch refuses: {}\nnothing was changed; the journal is {}",
            report.error.as_deref().unwrap_or_default(),
            dir.display()
        )),
        (outcome, true) => warn(&failure::message(&failure::Failure {
            step: report.failed_step.as_deref().unwrap_or_default(),
            error: report.error.as_deref().unwrap_or_default(),
            interrupted: outcome == Outcome::Interrupted,
            snapper_pre: journal.meta().snapper_pre,
            journal: &dir,
        })),
    }
    Ok(report.exit_code())
}

#[cfg(test)]
mod tests {
    use super::*;
    use archix::alpm::PinMode;
    use archix::plan::{Header, LineItem};

    fn plan() -> Plan {
        Plan::new(Header {
            mode: PinMode::Live,
            date: None,
            moved_from: None,
            fingerprint: String::new(),
            cached_dbs: false,
        })
    }

    #[test]
    fn only_yes_confirms() {
        for (input, yes) in [
            ("y\n", true),
            ("YES\n", true),
            ("n\n", false),
            ("\n", false),
            ("", false),
        ] {
            assert_eq!(confirmed(&mut input.as_bytes()), yes, "{input:?}");
        }
    }

    #[test]
    fn refusals_unchecked_items_and_a_missing_snapper_block_the_switch() {
        let mut p = plan();
        assert!(blockers(&p, false, false).is_empty(), "nothing to do needs no snapshot");
        p.flatpak.push(LineItem {
            line: "install x".into(),
            reason: "declared".into(),
            risk: Vec::new(),
        });
        assert_eq!(blockers(&p, false, false).len(), 1);
        assert!(
            blockers(&p, false, true).is_empty(),
            "--no-snapshot is a flag, not a refusal"
        );
        assert!(blockers(&p, true, false).is_empty());
        p.refusals.push("no kernel would be installed".into());
        p.unchecked.push("flatpak: installed apps unknown".into());
        let all = blockers(&p, true, false);
        assert_eq!(
            all,
            [
                "no kernel would be installed",
                "not checked, so the plan is incomplete: flatpak: installed apps unknown"
            ]
        );
    }
}

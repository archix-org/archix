use std::ffi::OsStr;
use std::fs;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::generations::Store;
use crate::journal::{Journal, Step, Transaction};
use crate::pacman::read_local_db;
use crate::paths::Paths;

/// Set for every pacman run archix starts, so archix's own pacman hook can tell them apart from manual ones.
pub const TRANSACTION_ENV: &str = "ARCHIX_TRANSACTION";
/// pacman's `--ask` bit that answers "remove the conflicting package?" with yes (ALPM_QUESTION_CONFLICT_PKG).
const ASK_REMOVE_CONFLICTS: &str = "--ask=4";
/// archix may have restored an old kernel's modules itself, so reinstalling that kernel may overwrite them.
const KERNEL_MODULES: &str = "/usr/lib/modules/*";

#[derive(Debug, PartialEq)]
pub enum PacmanLock {
    Free,
    Running,
    Stale,
}

pub fn pacman_lock(paths: &Paths) -> PacmanLock {
    if !paths.pacman_lock().exists() {
        PacmanLock::Free
    } else if pacman_running() {
        PacmanLock::Running
    } else {
        PacmanLock::Stale
    }
}

fn pacman_running() -> bool {
    let Ok(entries) = fs::read_dir("/proc") else {
        return false;
    };
    entries
        .flatten()
        .any(|entry| fs::read_to_string(entry.path().join("comm")).is_ok_and(|comm| comm.trim() == "pacman"))
}

/// Runs the journal's remaining steps, saving progress after each; `reinstall` redoes installs after a crash.
pub fn run(paths: &Paths, journal: &mut Journal, reinstall: bool) -> Result<Vec<String>> {
    let path = paths.journal();
    let mut soft_failures = Vec::new();
    while let Some(step) = journal.next_step() {
        eprintln!(":: {}", step.describe());
        soft_failures.extend(
            run_step(paths, &journal.transaction, journal.generation, step, reinstall).with_context(|| {
                format!(
                    "{} failed; the switch is unfinished, run `archix recover`",
                    step.describe()
                )
            })?,
        );
        journal.done.push(step);
        journal.save(&path)?;
    }
    Journal::delete(&path)?;
    Ok(soft_failures)
}

fn run_step(
    paths: &Paths,
    tx: &Transaction,
    generation: Option<u32>,
    step: Step,
    reinstall: bool,
) -> Result<Vec<String>> {
    let system = tx.system.as_ref();
    match step {
        Step::Users => system.map_or(Ok(()), crate::system::apply::users).map(|()| Vec::new()),
        Step::Files => system
            .map_or(Ok(()), |change| crate::system::apply::files(paths, change))
            .map(|()| Vec::new()),
        Step::Reload => Ok(system.map_or_else(Vec::new, crate::system::apply::reload)),
        Step::StopServices => Ok(system.map_or_else(Vec::new, crate::system::apply::stop_services)),
        Step::Services => Ok(system.map_or_else(Vec::new, crate::system::apply::start_services)),
        Step::Boot => match &tx.boot {
            Some(plan) => crate::system::boot::write_entry(paths, plan).map(|_| Vec::new()),
            None => Ok(Vec::new()),
        },
        step => run_package_step(paths, tx, generation, step, reinstall).map(|()| Vec::new()),
    }
}

fn run_package_step(
    paths: &Paths,
    tx: &Transaction,
    generation: Option<u32>,
    step: Step,
    reinstall: bool,
) -> Result<()> {
    match step {
        Step::Users | Step::StopServices | Step::Files | Step::Reload | Step::Services | Step::Boot => Ok(()),
        Step::Activate => match generation {
            Some(number) => Store::new(paths).activate(number),
            None => Ok(()),
        },
        Step::Install if tx.install.is_empty() => Ok(()),
        Step::Install => {
            let mut args = vec![
                OsStr::new("-U"),
                OsStr::new("--noconfirm"),
                OsStr::new(ASK_REMOVE_CONFLICTS),
                OsStr::new("--overwrite"),
                OsStr::new(KERNEL_MODULES),
            ];
            if !reinstall {
                args.push(OsStr::new("--needed"));
            }
            args.extend(tx.install.iter().map(|path| path.as_os_str()));
            pacman(&args)
        }
        Step::Remove => {
            let installed = read_local_db(&paths.local_db())?;
            let remove: Vec<&str> = tx
                .remove
                .iter()
                .filter(|name| installed.contains_key(*name))
                .map(String::as_str)
                .collect();
            if remove.is_empty() {
                return Ok(());
            }
            // -dd: what still depends on these is either removed too or replaced by the Install step that follows.
            let mut args = vec!["-Rdd", "--noconfirm"];
            args.extend(remove);
            pacman(&args)
        }
        Step::Reasons => {
            let installed = read_local_db(&paths.local_db())?;
            for (flag, explicit) in [("--asexplicit", true), ("--asdeps", false)] {
                let names: Vec<&str> = tx
                    .reasons
                    .iter()
                    .filter(|(name, want)| {
                        **want == explicit && installed.get(*name).is_some_and(|p| p.explicit != explicit)
                    })
                    .map(|(name, _)| name.as_str())
                    .collect();
                if !names.is_empty() {
                    let mut args = vec!["-D", flag];
                    args.extend(names);
                    pacman(&args)?;
                }
            }
            Ok(())
        }
        Step::Verify => {
            let problems = mismatches(paths, tx)?;
            if problems.is_empty() {
                Ok(())
            } else {
                bail!("the system does not match the target:\n  {}", problems.join("\n  "))
            }
        }
    }
}

/// Every way the installed packages differ from the transaction's end state.
pub fn mismatches(paths: &Paths, tx: &Transaction) -> Result<Vec<String>> {
    let installed = read_local_db(&paths.local_db())?;
    let mut problems = Vec::new();
    for (name, version) in &tx.versions {
        match installed.get(name) {
            None => problems.push(format!("{name} {version} is not installed")),
            Some(pkg) if pkg.version != *version => {
                problems.push(format!("{name} is {} instead of {version}", pkg.version))
            }
            Some(_) => {}
        }
    }
    for name in &tx.remove {
        if installed.contains_key(name) {
            problems.push(format!("{name} is still installed"));
        }
    }
    for (name, explicit) in &tx.reasons {
        if installed.get(name).is_some_and(|pkg| pkg.explicit != *explicit) {
            let reason = if *explicit {
                "explicitly installed"
            } else {
                "a dependency"
            };
            problems.push(format!("{name} should be marked as {reason}"));
        }
    }
    Ok(problems)
}

fn pacman<S: AsRef<OsStr>>(args: &[S]) -> Result<()> {
    let status = Command::new("pacman")
        .args(args)
        .env(TRANSACTION_ENV, "1")
        .status()
        .context("running pacman")?;
    if !status.success() {
        bail!("pacman exited with {status}");
    }
    Ok(())
}

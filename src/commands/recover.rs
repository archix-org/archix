use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::process::Command;

use anyhow::{Context, Result, bail};

use archix::apply::{self, PacmanLock};
use archix::archive::{ARCHIVE_URL, Archive};
use archix::fetch::{Wanted, fetch_all};
use archix::generations::Store;
use archix::journal::{Journal, Kind, Step, Transaction};
use archix::pacman::{LocalPkg, read_local_db};
use archix::paths::Paths;
use archix::system::apply::SystemChange;

use super::{RecoverMode, describe_unfinished, kind_name, require_root};

pub fn recover(mode: Option<RecoverMode>) -> Result<()> {
    require_root("recover")?;
    let paths = Paths::system();
    let Some(mut journal) = Journal::load(&paths.journal())? else {
        println!("nothing to recover: no switch is unfinished");
        return Ok(());
    };
    println!("{}", describe_unfinished(&journal));
    let mode = match mode {
        Some(mode) => mode,
        None => ask_mode()?,
    };
    match apply::pacman_lock(&paths) {
        PacmanLock::Running => bail!("pacman is running; wait for it to finish, then run `archix recover` again"),
        PacmanLock::Stale => {
            fs::remove_file(paths.pacman_lock()).context("removing pacman's stale lock")?;
            eprintln!("removed pacman's stale lock {}", paths.pacman_lock().display());
        }
        PacmanLock::Free => {}
    }
    let kind = kind_name(journal.kind);
    match mode {
        RecoverMode::Continue => {
            let install_interrupted = !journal.done.contains(&Step::Install);
            apply::run(&paths, &mut journal, install_interrupted)?;
            println!("finished the interrupted {kind} to {}", journal.target);
        }
        RecoverMode::Rollback => {
            let archive = Archive::new(ARCHIVE_URL);
            let installed = read_local_db(&paths.local_db())?;
            let transaction = rollback_transaction(&archive, &paths, &journal, &installed)?;
            let target = format!("the packages from before the {kind} to {}", journal.target);
            let mut rollback = Journal::new(Kind::Rollback, &journal.host, target, installed, transaction);
            rollback.save(&paths.journal())?;
            apply::run(&paths, &mut rollback, true)?;
            forget_unactivated(&paths, &journal);
            println!("rolled back to the packages from before the {kind}");
        }
        RecoverMode::Abandon => {
            let problems = apply::mismatches(&paths, &journal.transaction)?;
            if problems.is_empty() {
                println!("the installed packages already match {}", journal.target);
            } else {
                println!("differences from {}:", journal.target);
                for problem in &problems {
                    println!("  {problem}");
                }
            }
            check_files(&touched(&journal))?;
            Journal::delete(&paths.journal())?;
            forget_unactivated(&paths, &journal);
            println!("closed the journal without changes; `archix diff` shows how the system differs from the lock");
        }
    }
    Ok(())
}

/// Deletes the generation an abandoned or rolled-back switch created but never made current.
fn forget_unactivated(paths: &Paths, journal: &Journal) {
    let store = Store::new(paths);
    if let Some(number) = journal.generation
        && journal.kind == Kind::Switch
        && store.current().ok().flatten() != Some(number)
        && store.get(number).is_ok_and(|g| g.created >= journal.started)
    {
        let _ = store.delete(number);
    }
}

/// Packages the interrupted transaction meant to change, which pacman may have left half-installed.
fn touched(journal: &Journal) -> Vec<String> {
    journal
        .transaction
        .versions
        .iter()
        .filter(|(name, version)| journal.before.get(*name).is_none_or(|pkg| pkg.version != **version))
        .map(|(name, _)| name.clone())
        .chain(journal.transaction.remove.iter().cloned())
        .collect()
}

fn rollback_transaction(
    archive: &Archive,
    paths: &Paths,
    journal: &Journal,
    installed: &BTreeMap<String, LocalPkg>,
) -> Result<Transaction> {
    let touched: BTreeSet<String> = touched(journal).into_iter().collect();
    let mut wanted = Vec::new();
    for (name, pkg) in &journal.before {
        let changed = installed.get(name).is_none_or(|now| now.version != pkg.version);
        if changed || touched.contains(name) {
            let file = format!("{name}-{}-{}.pkg.tar.zst", pkg.version, pkg.arch);
            wanted.push(Wanted {
                name: name.clone(),
                urls: vec![archive.package_url(name, &file)],
                file,
                sha256: None,
            });
        }
    }
    let install = fetch_all(
        archive,
        &paths.packages(),
        std::slice::from_ref(&paths.pacman_cache),
        &wanted,
    )?;
    Ok(Transaction {
        system: journal.transaction.system.as_ref().map(SystemChange::reversed),
        install,
        remove: installed
            .keys()
            .filter(|name| !journal.before.contains_key(*name))
            .cloned()
            .collect(),
        versions: journal
            .before
            .iter()
            .map(|(name, pkg)| (name.clone(), pkg.version.clone()))
            .collect(),
        reasons: journal
            .before
            .iter()
            .map(|(name, pkg)| (name.clone(), pkg.explicit))
            .collect(),
    })
}

fn check_files(names: &[String]) -> Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    println!(
        "checking the files of {} packages the transaction touched:",
        names.len()
    );
    let status = Command::new("pacman")
        .arg("-Qk")
        .args(names)
        .status()
        .context("running pacman -Qk")?;
    if !status.success() {
        println!(
            "some packages are missing files or aren't installed; `archix switch` will reinstall what the lock needs"
        );
    }
    Ok(())
}

fn ask_mode() -> Result<RecoverMode> {
    eprint!("[c]ontinue it, [r]oll back, [a]bandon (close the journal without changes), or [q]uit? ");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    match answer.trim().to_lowercase().as_str() {
        "c" | "continue" => Ok(RecoverMode::Continue),
        "r" | "rollback" => Ok(RecoverMode::Rollback),
        "a" | "abandon" => Ok(RecoverMode::Abandon),
        _ => bail!("nothing done"),
    }
}

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::process::Command;

use anyhow::{Context, Result, bail};

use archix::apply::{self, PacmanLock};
use archix::archive::Archive;
use archix::fetch::{Wanted, fetch_all};
use archix::generations::Store;
use archix::journal::{Journal, Kind, Step, Transaction};
use archix::pacman::{LocalPkg, read_local_db};
use archix::paths::Paths;
use archix::system::apply::SystemChange;

use archix::downgrade::{merged, unsafe_downgrades};
use archix::plan::Change;

use super::switch::{guard_downgrades, reboot_notice, report_soft_failures, run_hooks};
use super::{RecoverMode, count, describe_unfinished, kind_name, require_root};

pub fn recover(mode: Option<RecoverMode>, allow_unsafe: bool) -> Result<()> {
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
    let mut torn = Vec::new();
    if !matches!(mode, RecoverMode::Abandon) {
        reregister_torn(&paths, &journal)?;
        let aside = paths.state.join("torn-db").join(journal.started.to_string());
        torn = archix::pacman::set_aside_torn(&paths.local_db(), &aside)?;
    }
    if !torn.is_empty() {
        println!(
            "pacman's database entries for {} were torn by the crash; moved them to {} and reinstalling those packages over their files",
            torn.join(", "),
            paths.state.join("torn-db").display()
        );
        journal.transaction.repair = true;
        journal.done.retain(|step| *step != Step::Install);
        journal.save(&paths.journal())?;
    }
    let kind = kind_name(journal.kind);
    match mode {
        RecoverMode::Continue => {
            let install_interrupted = !journal.done.contains(&Step::Install);
            let failures = apply::run(&paths, &mut journal, install_interrupted)?;
            println!("finished the interrupted {kind} to {}", journal.target);
            return finish_with_hooks(&paths, &journal, failures, &format!("{} is current", journal.target));
        }
        RecoverMode::Rollback => {
            let archive = Archive::new(&archix::archive::configured(&paths.state));
            let installed = read_local_db(&paths.local_db())?;
            let changes: Vec<Change> = journal
                .before
                .iter()
                .filter_map(|(name, before)| {
                    let now = installed.get(name)?;
                    (now.version != before.version).then(|| Change::Downgrade {
                        name: name.clone(),
                        from: now.version.clone(),
                        to: before.version.clone(),
                    })
                })
                .collect();
            let configured = journal
                .transaction
                .system
                .as_ref()
                .map(|change| merged(&change.from.unsafe_downgrades, &change.to.unsafe_downgrades))
                .unwrap_or_default();
            guard_downgrades(&unsafe_downgrades(&changes, &configured), false, allow_unsafe)?;
            let mut transaction = rollback_transaction(&archive, &paths, &journal, &installed)?;
            transaction.repair = !torn.is_empty();
            apply::check_packages(&transaction)?;
            let target = format!("the packages from before the {kind} to {}", journal.target);
            let mut rollback = Journal::new(Kind::Rollback, &journal.host, target, installed, transaction);
            rollback.save(&paths.journal())?;
            let failures = apply::run(&paths, &mut rollback, true)?;
            forget_unactivated(&paths, &journal);
            println!("rolled back to the packages from before the {kind}");
            return finish_with_hooks(&paths, &rollback, failures, "the rollback finished");
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

/// Runs the hooks the interrupted switch never got to, and every on_change script, since which files it rewrote is unknown.
fn finish_with_hooks(paths: &Paths, journal: &Journal, mut failures: Vec<String>, state: &str) -> Result<()> {
    if let Some(change) = &journal.transaction.system {
        let every_file: BTreeSet<String> = change.to.on_change.keys().cloned().collect();
        failures.extend(run_hooks(paths, change, &every_file));
    }
    reboot_notice();
    report_soft_failures(&failures, state)
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
        reinstall: false,
        at_boot: false,
        repair: false,
        system: journal.transaction.system.as_ref().map(SystemChange::reversed),
        boot: None,
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
        "checking the files of the {} the transaction touched:",
        count(names.len(), "package", "packages")
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

/// Rewrites the pacman database entries a crash tore from the package files the interrupted switch was installing, so pacman can finish or undo them.
fn reregister_torn(paths: &Paths, journal: &Journal) -> Result<()> {
    let Ok(entries) = fs::read_dir(paths.local_db()) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let desc = archix::pacman::parse_desc(&fs::read_to_string(entry.path().join("desc")).unwrap_or_default());
        if desc.contains_key("NAME") && desc.contains_key("VERSION") {
            continue;
        }
        let dir_name = entry.file_name().to_string_lossy().into_owned();
        let name = archix::pacman::name_from_entry(&dir_name);
        let prefix = format!("{dir_name}-");
        let named = |path: &std::path::Path| {
            path.file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .is_some_and(|file| {
                    file.ends_with(".pkg.tar.zst")
                        && (file.starts_with(&prefix) || file.get(13..).is_some_and(|rest| rest.starts_with(&prefix)))
                })
        };
        let cached = || {
            [paths.packages(), paths.pacman_cache.clone()]
                .into_iter()
                .filter_map(|dir| fs::read_dir(dir).ok())
                .flat_map(|dir| dir.flatten().map(|e| e.path()))
                .find(|path| named(path))
        };
        let Some(file) = journal
            .transaction
            .install
            .iter()
            .find(|path| named(path))
            .cloned()
            .or_else(cached)
        else {
            continue;
        };
        let explicit = journal
            .transaction
            .reasons
            .get(name)
            .copied()
            .or_else(|| journal.before.get(name).map(|pkg| pkg.explicit))
            .unwrap_or(true);
        archix::system::rescue::register(std::path::Path::new("/"), &file, explicit)
            .with_context(|| format!("rewriting the torn database entry {dir_name}"))?;
        println!(
            "pacman's database entry for {dir_name} was torn by the crash; rewrote it from {}",
            file.display()
        );
    }
    Ok(())
}

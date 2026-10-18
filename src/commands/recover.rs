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

/// A keyring package's install script rewrites pacman's gpg keyring, which a power cut can leave unreadable, and pacman then rejects every package; this sets it aside and rebuilds it from the keyrings on disk.
fn repair_keyring(paths: &Paths, journal: &Journal) -> Result<()> {
    let gnupg = paths.pacman_conf.with_file_name("pacman.d").join("gnupg");
    if !gnupg.is_dir() {
        return Ok(());
    }
    let readable = Command::new("gpg")
        .arg("--homedir")
        .arg(&gnupg)
        .args(["--batch", "--list-keys"])
        .output()
        .is_ok_and(|output| output.status.success());
    if readable {
        return Ok(());
    }
    let aside = paths.state.join("torn-gnupg").join(journal.started.to_string());
    fs::create_dir_all(aside.parent().expect("under the state dir"))?;
    let _ = Command::new("gpgconf")
        .arg("--homedir")
        .arg(&gnupg)
        .args(["--kill", "all"])
        .output();
    if fs::rename(&gnupg, &aside).is_err() {
        apply::copy_regular(&gnupg, &aside)?;
        fs::remove_dir_all(&gnupg).with_context(|| format!("removing {}", gnupg.display()))?;
    }
    for args in [&["--init"][..], &["--populate"][..]] {
        let status = Command::new("pacman-key")
            .args(args)
            .status()
            .context("running pacman-key")?;
        if !status.success() {
            bail!(
                "pacman's keyring was torn by the crash, and `pacman-key {}` failed rebuilding it",
                args.join(" ")
            );
        }
    }
    println!(
        "pacman's keyring was torn by the crash; rebuilt it from /usr/share/pacman/keyrings, and moved the torn one to {}",
        aside.display()
    );
    Ok(())
}

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
            "pacman's database entries for {} were torn by the crash; moved them to {} and reinstalling those packages",
            torn.join(", "),
            paths.state.join("torn-db").display()
        );
        journal.done.retain(|step| *step != Step::Install);
        journal.save(&paths.journal())?;
    }
    if !matches!(mode, RecoverMode::Abandon) {
        set_aside_leftovers(&paths, &journal.transaction.install, &journal)?;
        repair_keyring(&paths, &journal)?;
    }
    let kind = kind_name(journal.kind);
    if !matches!(mode, RecoverMode::Abandon) {
        archix::system::btrfs::ensure_metadata_room(std::path::Path::new("/"))?;
    }
    match mode {
        RecoverMode::Continue => {
            let install_interrupted = !journal.done.contains(&Step::Install);
            make_pacman_run(&journal.transaction)?;
            let failures = apply::run(&paths, &mut journal, install_interrupted)?;
            println!("finished the interrupted {kind} to {}", journal.target);
            return finish_with_hooks(&paths, &journal, failures, &format!("{} is current", journal.target));
        }
        RecoverMode::Rollback => {
            let archive = archix::archive::for_system(&paths.state);
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
            let transaction = rollback_transaction(&archive, &paths, &journal, &installed)?;
            make_pacman_run(&transaction)?;
            set_aside_leftovers(&paths, &transaction.install, &journal)?;
            archix::conflicts::check(&transaction.install, &transaction.remove, &paths.local_db())?;
            apply::check_packages(&transaction)?;
            let target = format!("the packages from before the {kind} to {}", journal.target);
            let mut rollback = Journal::new(Kind::Rollback, &journal.host, target, installed, transaction);
            rollback.begin(&paths.journal())?;
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

/// pacman loads its libraries from the packages it installs, so a transaction cut off halfway can leave them at mixed versions and pacman unable to start.
fn make_pacman_run(transaction: &Transaction) -> Result<()> {
    let runs = || {
        Command::new("pacman")
            .args(["-Qq", "pacman"])
            .output()
            .is_ok_and(|o| o.status.success())
    };
    if runs() {
        return Ok(());
    }
    let restored =
        archix::system::rescue::restore_pacman(std::path::Path::new("/"), &transaction.install, &transaction.reasons)?;
    println!(
        "pacman couldn't start, its libraries left at mixed versions by the interruption; unpacked the {} packages it runs on without it",
        restored.len()
    );
    if !runs() {
        bail!(
            "pacman still can't start; boot the current generation's rescue entry, which restores it without pacman, then run `archix recover` again"
        );
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
    // What was installed before the switch is the current generation, which records each file and its sha256.
    let store = Store::new(paths);
    let current = store.current()?.and_then(|number| store.get(number).ok());
    let repos = super::third_party_repos(paths);
    let mut wanted = Vec::new();
    let mut verified = Vec::new();
    for (name, pkg) in &journal.before {
        let changed = installed.get(name).is_none_or(|now| now.version != pkg.version);
        if !changed && !touched.contains(name) {
            continue;
        }
        let recorded = current
            .as_ref()
            .and_then(|generation| generation.packages.get(name))
            .filter(|locked| locked.version == pkg.version && !locked.sha256.is_empty());
        let (file, sha256) = match recorded {
            Some(locked) => (locked.file.clone(), Some(locked.sha256.clone())),
            None => (format!("{name}-{}-{}.pkg.tar.zst", pkg.version, pkg.arch), None),
        };
        if sha256.is_some() {
            verified.push(file.clone());
        }
        let mut urls: Vec<String> = recorded
            .and_then(|locked| repos.iter().find(|repo| repo.name == locked.repo))
            .map(|repo| repo.urls(&file))
            .unwrap_or_default();
        urls.push(archive.package_url(name, &file));
        wanted.push(Wanted {
            name: name.clone(),
            urls,
            file,
            sha256,
        });
    }
    let fetched = fetch_all(
        archive,
        &paths.packages(),
        std::slice::from_ref(&paths.pacman_cache),
        &wanted,
    )?;
    // Only files whose sha256 the generation pinned may skip their signature; pacman checks the rest against the keyring.
    let (pinned, unpinned): (Vec<_>, Vec<_>) = fetched.into_iter().partition(|path| {
        verified
            .iter()
            .any(|file| path.file_name().is_some_and(|name| name == file.as_str()))
    });
    let mut install = apply::without_signatures(paths, &pinned)?;
    install.extend(unpinned);
    Ok(Transaction {
        config: Vec::new(),
        reinstall: false,
        at_boot: false,
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
        // A package file the same crash tore can't be read; the entry is then moved aside like one without a file.
        if let Err(e) = archix::system::rescue::register(std::path::Path::new("/"), &file, explicit) {
            println!(
                "could not rewrite the torn entry {dir_name} from {}: {e:#}",
                file.display()
            );
            continue;
        }
        println!(
            "pacman's database entry for {dir_name} was torn by the crash; rewrote it from {}",
            file.display()
        );
    }
    Ok(())
}

/// Moves aside unowned files the given packages ship; switches refuse such files beforehand, so these are leftovers of the pacman run a crash cut off.
fn set_aside_leftovers(paths: &Paths, packages: &[std::path::PathBuf], journal: &Journal) -> Result<()> {
    let root = std::path::Path::new("/");
    let owned = archix::conflicts::owned_files(&paths.local_db());
    let aside = paths.state.join("crash-leftovers").join(journal.started.to_string());
    let mut moved = 0;
    for package in packages.iter().filter(|path| path.is_file()) {
        let contents = archix::conflicts::contents(package)?;
        for file in archix::conflicts::unowned_on_disk(root, &contents.files, &owned) {
            let dest = aside.join(file);
            if let Some(dir) = dest.parent() {
                fs::create_dir_all(dir)?;
            }
            fs::rename(root.join(file), &dest).with_context(|| format!("moving the leftover /{file} aside"))?;
            moved += 1;
        }
    }
    if moved > 0 {
        println!(
            "moved {} left behind by the interrupted pacman run to {}",
            count(moved, "file", "files"),
            aside.display()
        );
    }
    Ok(())
}

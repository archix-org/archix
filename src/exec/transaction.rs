//! SCOPE 2.5's two libalpm transactions, committed under archix's db.lck with ALPM_TRANS_FLAG_NOLOCK, each compared with the plan before commit.
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};

use alpm::{Alpm, Event, LogLevel, PackageOperation, PackageReason, TransFlag};
use anyhow::{Context, Result, anyhow};

use super::Refused;
use super::compare::{Expected, differences};
use super::output::{raw, say, warn};
use super::signals::Committing;
use crate::alpm::Answers;
use crate::alpm::Prepared;
use crate::alpm::handle::{collect, install_questions, prepare_error};

/// Sends libalpm's events and log lines to the terminal through writes that drop their errors; a panic never crosses into libalpm.
pub fn set_output(handle: &Alpm) {
    handle.set_event_cb((), |event, _| {
        let _ = catch_unwind(AssertUnwindSafe(|| match event.event() {
            Event::PackageOperationStart(op) => match op.operation() {
                PackageOperation::Install(p) => say(&format!("installing {} ({})", p.name(), p.version())),
                PackageOperation::Upgrade(new, old) => say(&format!(
                    "upgrading {} ({} -> {})",
                    new.name(),
                    old.version(),
                    new.version()
                )),
                PackageOperation::Downgrade(new, old) => say(&format!(
                    "downgrading {} ({} -> {})",
                    new.name(),
                    old.version(),
                    new.version()
                )),
                PackageOperation::Reinstall(new, _) => say(&format!("reinstalling {} ({})", new.name(), new.version())),
                PackageOperation::Remove(p) => say(&format!("removing {} ({})", p.name(), p.version())),
            },
            Event::ScriptletInfo(info) => raw(info.line()),
            Event::HookRunStart(hook) => say(&format!(
                "({}/{}) {}",
                hook.position(),
                hook.total(),
                hook.desc().unwrap_or(hook.name())
            )),
            Event::PacnewCreated(e) => warn(&format!("warning: {} installed as {}.pacnew", e.file(), e.file())),
            Event::PacsaveCreated(e) => warn(&format!("warning: {} saved as {}.pacsave", e.file(), e.file())),
            Event::RetrieveStart => say("downloading packages"),
            Event::PkgRetrieveFailed(_) => warn("error: a package failed to download"),
            _ => {}
        }));
    });
    handle.set_log_cb((), |level, message, _| {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            if level.contains(LogLevel::ERROR) {
                warn(&format!("error: {message}"));
            } else if level.contains(LogLevel::WARNING) {
                warn(&format!("warning: {message}"));
            }
        }));
    });
}

fn commit_error(e: alpm::CommitError) -> anyhow::Error {
    let detail = match e.data() {
        Some(alpm::CommitData::PkgInvalid(files)) => files.iter().collect::<Vec<_>>().join(", "),
        _ => String::new(),
    };
    match detail.is_empty() {
        true => anyhow!("libalpm's commit failed: {}", e.error()),
        false => anyhow!("libalpm's commit failed: {}: {detail}", e.error()),
    }
}

fn mismatch(what: &str, diffs: &[String]) -> anyhow::Error {
    Refused(format!(
        "the {what} transaction libalpm prepared isn't the plan, so it wasn't committed: {}; run diff again",
        diffs.join("; ")
    ))
    .into()
}

/// Commits `body` (adding the transaction's packages) after the prepared sets are compared with `expected` and `before_commit` journaled them.
fn commit(
    handle: &mut Alpm,
    flags: TransFlag,
    what: &str,
    expected: &Expected,
    body: impl FnOnce(&Alpm) -> Result<()>,
    before_commit: &mut dyn FnMut(&Prepared) -> Result<()>,
) -> Result<Prepared> {
    let replaced = install_questions(handle, Answers::Only(expected.replaced.clone()));
    handle
        .trans_init(flags | TransFlag::NO_LOCK)
        .map_err(|e| anyhow!("starting the {what} transaction: {e}"))?;
    let result = (|| {
        body(handle)?;
        handle.trans_prepare().map_err(prepare_error)?;
        let prepared = collect(handle, replaced.borrow().clone());
        let diffs = differences(expected, &prepared);
        if !diffs.is_empty() {
            return Err(mismatch(what, &diffs));
        }
        before_commit(&prepared)?;
        let committing = Committing::on(handle);
        let done = handle.trans_commit().map_err(commit_error);
        drop(committing);
        done?;
        Ok(prepared)
    })();
    let _ = handle.trans_release();
    result
}

/// The install transaction: a full upgrade plus `targets` and the built AUR files, with `skip` held back as IgnorePkg holds packages.
pub fn install(
    handle: &mut Alpm,
    targets: &[String],
    skip: &[String],
    aur_files: &[PathBuf],
    expected: &Expected,
    before_commit: &mut dyn FnMut(&Prepared) -> Result<()>,
) -> Result<Prepared> {
    let added: Vec<String> = skip
        .iter()
        .filter(|name| !handle.ignorepkgs().iter().any(|i| i == name.as_str()))
        .cloned()
        .collect();
    for name in &added {
        handle
            .add_ignorepkg(name.as_str())
            .map_err(|e| anyhow!("holding back {name}: {e}"))?;
    }
    let body = |handle: &Alpm| -> Result<()> {
        handle
            .sync_sysupgrade(false)
            .map_err(|e| anyhow!("full upgrade: {e}"))?;
        for target in targets {
            let pkg = handle
                .syncdbs()
                .iter()
                .find_map(|db| db.pkg(target.as_str()).ok())
                .ok_or_else(|| anyhow!("{target} is in no repo"))?;
            match handle.trans_add_pkg(pkg) {
                Ok(()) => {}
                Err(e) if e.error == alpm::Error::TransDupTarget => {}
                Err(e) => return Err(anyhow!("adding {target}: {}", e.error)),
            }
        }
        for file in aur_files {
            let path = file.to_string_lossy().into_owned();
            let pkg = handle
                .pkg_load(path.as_str(), true, handle.local_file_siglevel())
                .map_err(|e| anyhow!("loading {path}: {e}"))?;
            handle
                .trans_add_pkg(pkg)
                .map_err(|e| anyhow!("adding {path}: {}", e.error))?;
        }
        Ok(())
    };
    let result = commit(handle, TransFlag::NONE, "install", expected, body, before_commit);
    for name in added {
        let _ = handle.remove_ignorepkg(name.as_str());
    }
    result
}

/// The removal transaction, as `pacman -Rs` (never -n, so changed configs become .pacsave).
pub fn remove(
    handle: &mut Alpm,
    targets: &[String],
    expected: &Expected,
    before_commit: &mut dyn FnMut(&Prepared) -> Result<()>,
) -> Result<Prepared> {
    let body = |handle: &Alpm| -> Result<()> {
        for target in targets {
            let pkg = handle
                .localdb()
                .pkg(target.as_str())
                .map_err(|_| anyhow!("{target} is not installed"))?;
            handle
                .trans_remove_pkg(pkg)
                .map_err(|e| anyhow!("removing {target}: {e}"))?;
        }
        Ok(())
    };
    commit(handle, TransFlag::RECURSE, "removal", expected, body, before_commit)
}

/// Sets each package's install reason as the plan's mark lines say.
pub fn set_reasons(handle: &Alpm, reasons: &BTreeMap<String, PackageReason>) -> Result<()> {
    for (name, reason) in reasons {
        let pkg = handle
            .localdb()
            .pkg(name.as_str())
            .map_err(|_| anyhow!("{name} is not installed"))?;
        pkg.set_reason(*reason).map_err(|e| anyhow!("marking {name}: {e}"))?;
    }
    Ok(())
}

/// N2: the system's sync dbs become the plan's (and their signatures, a stale one removed), leaving the state `pacman -Syu` would.
pub fn replace_sync_dbs(
    plan_dbpath: &Path,
    real_dbpath: &Path,
    repos: &[String],
    before_each: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<()> {
    for repo in repos {
        let from = plan_dbpath.join("sync").join(format!("{repo}.db"));
        if !from.exists() {
            continue;
        }
        before_each(repo)?;
        let to = real_dbpath.join("sync").join(format!("{repo}.db"));
        crate::durable::copy(&from, &to)?;
        let (sig_from, sig_to) = (from.with_extension("db.sig"), to.with_extension("db.sig"));
        if sig_from.exists() {
            crate::durable::copy(&sig_from, &sig_to)?;
        } else if sig_to.exists() {
            std::fs::remove_file(&sig_to).with_context(|| format!("removing the stale {}", sig_to.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alpm::testdb::{TestRoot, pkg};
    use std::fs;

    #[test]
    fn the_systems_sync_dbs_become_the_plans_and_a_stale_signature_goes() {
        let t = TestRoot::new();
        t.repo("core", &[pkg("a", "2-1")]);
        t.repo("extra", &[pkg("b", "1-1")]);
        let plan = t.plan_dbpath();
        fs::write(plan.join("sync/core.db.sig"), "sig").unwrap();
        let real = t.real_dbpath();
        fs::create_dir_all(real.join("sync")).unwrap();
        fs::write(real.join("sync/core.db"), "old").unwrap();
        fs::write(real.join("sync/extra.db.sig"), "stale").unwrap();
        let mut seen = Vec::new();
        replace_sync_dbs(
            &plan,
            &real,
            &["core".into(), "extra".into(), "multilib".into()],
            &mut |repo| {
                seen.push(repo.to_string());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(seen, ["core", "extra"], "a repo the plan has no db for is left alone");
        for name in ["core.db", "core.db.sig", "extra.db"] {
            assert_eq!(
                fs::read(real.join("sync").join(name)).unwrap(),
                fs::read(plan.join("sync").join(name)).unwrap(),
                "{name}"
            );
        }
        assert!(!real.join("sync/extra.db.sig").exists());
    }

    #[test]
    fn a_transaction_that_differs_from_the_plan_is_never_committed() {
        let t = TestRoot::new();
        t.install(&[pkg("glibc", "2.40-1").dep()]);
        t.repo("core", &[pkg("glibc", "2.41-1")]);
        let mut handle = crate::alpm::open(&t.conf(&["core"]), &t.plan_dbpath()).unwrap();
        let mut journaled = 0;
        let err = install(&mut handle, &[], &[], &[], &Expected::default(), &mut |_| {
            journaled += 1;
            Ok(())
        })
        .unwrap_err();
        assert!(err.downcast_ref::<Refused>().is_some(), "{err:#}");
        assert!(
            format!("{err}").contains("libalpm would also install glibc 2.41-1"),
            "{err}"
        );
        assert_eq!(journaled, 0, "nothing reached the journal's commit record");
        assert!(handle.localdb().pkg("glibc").unwrap().version().as_str() == "2.40-1");
        assert!(!t.real_dbpath().join("db.lck").exists(), "NOLOCK took no lock");
    }

    #[test]
    fn marks_set_install_reasons_in_the_local_db() {
        let t = TestRoot::new();
        t.install(&[pkg("vim", "9-1").dep(), pkg("lib", "1-1")]);
        let handle = crate::alpm::open(&t.conf(&[]), &t.real_dbpath()).unwrap();
        set_reasons(
            &handle,
            &BTreeMap::from([
                ("vim".to_string(), PackageReason::Explicit),
                ("lib".to_string(), PackageReason::Depend),
            ]),
        )
        .unwrap();
        drop(handle);
        let handle = crate::alpm::open(&t.conf(&[]), &t.real_dbpath()).unwrap();
        assert_eq!(handle.localdb().pkg("vim").unwrap().reason(), PackageReason::Explicit);
        assert_eq!(handle.localdb().pkg("lib").unwrap().reason(), PackageReason::Depend);
    }
}

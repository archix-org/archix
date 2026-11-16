//! The one way diff and switch evaluate the config (SCOPE 9): as root, captured as the config dir's owner and evaluated under a transient uid; as anyone else, in this process as that user.
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};

use crate::captured::{self, Captured, Work};
use crate::value::Value;

/// A config evaluated for a plan, with the tree its files, lock and recipes are read from.
pub struct Evaluated {
    pub config: Value,
    /// Root's own copy of the captured tree, or the config directory itself when not root.
    pub tree: PathBuf,
    pub captured: Option<Captured>,
    _work: Option<Work>,
}

impl Evaluated {
    /// archix.lock as this evaluation sees it; refuses one a .gitignore kept out of root's capture rather than reading it as no lock.
    pub fn lock_path(&self, config_dir: &Path) -> Result<Option<PathBuf>> {
        let path = self.tree.join(crate::lock::LOCK_FILE);
        if path.exists() {
            return Ok(Some(path));
        }
        let live = config_dir.join(crate::lock::LOCK_FILE);
        if self.captured.is_some() && live.symlink_metadata().is_ok() {
            bail!(
                "{} exists but isn't in the capture of the config, so it must be in a .gitignore; the lock is part of the plan, so stop ignoring it",
                live.display()
            );
        }
        Ok(None)
    }
}

/// How a config is evaluated, chosen only by whether archix runs as root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    InProcess,
    Isolated,
}

pub fn route(is_root: bool) -> Route {
    match is_root {
        true => Route::Isolated,
        false => Route::InProcess,
    }
}

fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// Evaluates `config_dir` for `host` the way the switch will: root never evaluates in-process.
pub fn evaluate_for_plan(config_dir: &Path, host: &str) -> Result<Evaluated> {
    evaluate_routed(config_dir, host, is_root(), &isolated)
}

pub(crate) fn evaluate_routed(
    config_dir: &Path,
    host: &str,
    is_root: bool,
    isolated: &dyn Fn(&Path, &str) -> Result<Evaluated>,
) -> Result<Evaluated> {
    match route(is_root) {
        Route::Isolated => isolated(config_dir, host),
        Route::InProcess => in_process(config_dir, host),
    }
}

fn in_process(config_dir: &Path, host: &str) -> Result<Evaluated> {
    let evaluation = crate::eval::evaluate(&crate::eval::Input {
        config_dir: config_dir.to_path_buf(),
        hostname: host.to_string(),
        overlay: Default::default(),
    })
    .map_err(|errors| anyhow!("{errors}"))?;
    Ok(Evaluated {
        config: evaluation.config,
        tree: config_dir.to_path_buf(),
        captured: None,
        _work: None,
    })
}

fn isolated(config_dir: &Path, host: &str) -> Result<Evaluated> {
    let work = Work::new(Path::new(captured::CAPTURES))?;
    let captured = captured::capture(config_dir, &work)?;
    crate::testhooks::after_capture();
    let config = crate::eval::isolated::evaluate(&captured.dir, host, &Default::default(), &Default::default())
        .map_err(|e| anyhow!("{}", captured.explain(format!("{e:#}"))))?;
    Ok(Evaluated {
        config,
        tree: captured.dir.clone(),
        captured: Some(captured),
        _work: Some(work),
    })
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;

    use super::*;

    fn config(init: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("init.lua"), init).unwrap();
        dir
    }

    fn marked(config_dir: &Path) -> Evaluated {
        Evaluated {
            config: Value::Str("isolated".into()),
            tree: config_dir.join("captured"),
            captured: None,
            _work: None,
        }
    }

    #[test]
    fn root_evaluates_only_through_the_isolated_path() {
        assert_eq!(route(true), Route::Isolated);
        assert_eq!(route(false), Route::InProcess);
        let dir = config("error('evaluated in-process as root')\n");
        let called = Cell::new(0);
        let isolated = |d: &Path, host: &str| {
            called.set(called.get() + 1);
            assert_eq!(host, "box");
            Ok(marked(d))
        };
        let got = evaluate_routed(dir.path(), "box", true, &isolated).unwrap();
        assert_eq!(called.get(), 1);
        assert_eq!(got.config, Value::Str("isolated".into()));
        assert_eq!(got.tree, dir.path().join("captured"));
    }

    #[test]
    fn a_normal_user_evaluates_in_process_as_themselves() {
        let dir = config("return { packages = { 'vim' } }\n");
        let isolated =
            |_: &Path, _: &str| -> Result<Evaluated> { panic!("a normal user's diff went through the isolated path") };
        let got = evaluate_routed(dir.path(), "box", false, &isolated).unwrap();
        assert_eq!(got.tree, dir.path());
        assert!(got.captured.is_none());
        let Value::Map(map) = &got.config else {
            panic!("{:?}", got.config)
        };
        assert_eq!(map["packages"], Value::List(vec![Value::Str("vim".into())]));
    }

    #[test]
    fn in_process_errors_name_the_config() {
        let dir = config("error('boom')\n");
        let isolated = |_: &Path, _: &str| -> Result<Evaluated> { unreachable!() };
        let error = format!(
            "{:#}",
            evaluate_routed(dir.path(), "box", false, &isolated).err().unwrap()
        );
        assert!(error.contains("boom"), "{error}");
    }

    #[test]
    fn a_lock_missing_from_the_capture_but_on_disk_is_refused() {
        let dir = config("return {}\n");
        let tree = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(crate::lock::LOCK_FILE), "").unwrap();
        let mut got = Evaluated {
            config: Value::Null,
            tree: tree.path().to_path_buf(),
            captured: None,
            _work: None,
        };
        assert_eq!(got.lock_path(dir.path()).unwrap(), None);
        got.captured = Some(Captured {
            tree: "t".into(),
            dir: tree.path().to_path_buf(),
            ignored: vec![crate::lock::LOCK_FILE.into()],
        });
        let error = format!("{:#}", got.lock_path(dir.path()).unwrap_err());
        assert!(error.contains("gitignore"), "{error}");
    }
}

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use super::view::{Layout, VAR_BINDS, VIEW_DIR};
use super::{Backend, COPIED, OVERLAID, Store, TREES, Transaction};
use crate::testhooks::mark;

/// A deployment being built in `deploy/.staging-<N>` from the current one; deleted at the next switch unless committed.
#[derive(Debug)]
pub struct Staging {
    root: PathBuf,
    backend: Backend,
    number: u32,
    base: PathBuf,
    dir: PathBuf,
}

/// A staging tree complete on disk, ready for `commit`.
#[derive(Debug)]
pub struct Staged {
    pub number: u32,
    pub dir: PathBuf,
}

impl Staging {
    /// Stages deployment `number` from the current one, first deleting whatever staging a crash left behind.
    pub fn new(store: &Store, number: u32) -> Result<Staging> {
        mark("deploy-stage");
        store.remove_leftovers()?;
        let current = store
            .current()?
            .context("there's no current deployment to stage from")?;
        let base = store.path(current);
        if store.path(number).symlink_metadata().is_ok() {
            bail!("deployment {number} already exists");
        }
        let dir = store.staging_path(number);
        match store.backend() {
            Backend::Snapshot => {
                run(Command::new("btrfs")
                    .args(["-q", "subvolume", "snapshot"])
                    .arg(&base)
                    .arg(&dir))?;
                remove_any(&dir.join("var-upper"))?;
            }
            Backend::Overlay => {
                fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
                for tree in COPIED {
                    if base.join(tree).is_dir() {
                        run(Command::new("cp")
                            .args(["-a", "--no-target-directory"])
                            .arg(base.join(tree))
                            .arg(dir.join(tree)))?;
                    }
                }
                for tree in OVERLAID {
                    layer_dirs(&dir, tree, &base.join(tree))?;
                }
            }
        }
        for tree in COPIED {
            if !dir.join(tree).is_dir() {
                fs::create_dir(dir.join(tree)).with_context(|| format!("creating {}", dir.join(tree).display()))?;
            }
        }
        layer_dirs(&dir, "var", &store.root().join("var"))?;
        Ok(Staging {
            root: store.root().to_path_buf(),
            backend: store.backend(),
            number,
            base,
            dir,
        })
    }

    pub fn number(&self) -> u32 {
        self.number
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// A tree that is a plain directory throughout staging, for archix's own writes: etc, pacman or dkms.
    pub fn tree(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// The staging root to run pacman, mkinitcpio and the like in.
    pub fn transaction(&self) -> Result<Transaction> {
        mark("deploy-transact");
        Transaction::new(&Layout {
            backend: self.backend,
            root: &self.root,
            base: &self.base,
            staging: &self.dir,
        })
    }

    /// Makes the staging tree a complete deployment: overlays flattened, /var's upper layer kept as var-upper, scratch removed.
    pub fn finish(self) -> Result<Staged> {
        mark("deploy-flatten");
        let view = self.dir.join(VIEW_DIR);
        match fs::remove_dir(&view) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("removing {}", view.display()));
            }
            _ => {}
        }
        if self.backend == Backend::Overlay {
            for tree in OVERLAID {
                super::flatten::flatten(
                    &self.base.join(tree),
                    &self.dir.join(".upper").join(tree),
                    &self.dir.join(tree),
                )?;
            }
        }
        let var = self.dir.join(".upper/var");
        for bind in VAR_BINDS {
            remove_any(&var.join(bind))?;
        }
        let var_upper = self.dir.join("var-upper");
        fs::rename(&var, &var_upper).with_context(|| format!("moving {} to {}", var.display(), var_upper.display()))?;
        for scratch in [".upper", ".work"] {
            remove_any(&self.dir.join(scratch))?;
        }
        let staged = Staged {
            number: self.number,
            dir: self.dir,
        };
        staged.check()?;
        Ok(staged)
    }
}

impl Staged {
    /// Refuses a tree that lacks part of a deployment or still holds staging scratch.
    pub fn check(&self) -> Result<()> {
        for tree in TREES {
            if !self.dir.join(tree).is_dir() {
                bail!("{} has no {tree}/; it isn't a complete deployment", self.dir.display());
            }
        }
        for scratch in [".upper", ".work", VIEW_DIR] {
            if self.dir.join(scratch).symlink_metadata().is_ok() {
                bail!(
                    "{} still has {scratch}; it isn't a complete deployment",
                    self.dir.display()
                );
            }
        }
        Ok(())
    }
}

/// An overlay's upper and work directories for `tree`, the upper root shaped like the lower's so an untouched tree merges to the lower exactly.
fn layer_dirs(dir: &Path, tree: &str, lower: &Path) -> Result<()> {
    let upper = dir.join(".upper").join(tree);
    fs::create_dir_all(&upper).with_context(|| format!("creating {}", upper.display()))?;
    fs::create_dir_all(dir.join(".work").join(tree))
        .with_context(|| format!("creating {}", dir.join(".work").display()))?;
    let meta = fs::symlink_metadata(lower).with_context(|| format!("stat {}", lower.display()))?;
    std::os::unix::fs::lchown(&upper, Some(meta.uid()), Some(meta.gid()))
        .with_context(|| format!("chown {}", upper.display()))?;
    fs::set_permissions(
        &upper,
        std::os::unix::fs::PermissionsExt::from_mode(meta.mode() & 0o7777),
    )?;
    super::flatten::set_times(&upper, &meta)
}

fn remove_any(path: &Path) -> Result<()> {
    let result = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    };
    result.with_context(|| format!("removing {}", path.display()))
}

fn run(command: &mut Command) -> Result<()> {
    let output = command.output().with_context(|| format!("running {command:?}"))?;
    if !output.status.success() {
        bail!("{command:?} failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

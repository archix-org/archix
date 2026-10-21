use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use super::store::{StoreLock, incomplete};
use super::view::{Layout, VAR_BINDS, VIEW_DIR, mount_points};
use super::{Backend, COPIED, OVERLAID, Store, Transaction};
use crate::testhooks::mark;

/// A deployment being built in `deploy/.staging-<N>` from a committed one; deleted at the next switch unless committed.
#[derive(Debug)]
pub struct Staging {
    store: Store,
    number: u32,
    base_number: u32,
    base: PathBuf,
    dir: PathBuf,
    lock: StoreLock,
}

/// A staging tree complete on disk, ready for `commit`.
#[derive(Debug)]
pub struct Staged {
    pub number: u32,
    pub dir: PathBuf,
    _lock: StoreLock,
}

impl Staging {
    /// Stages deployment `number` from the current one, first deleting whatever staging a crash left behind; holds the store's lock until committed or dropped.
    pub fn new(store: &Store, number: u32) -> Result<Staging> {
        let lock = store.lock()?;
        let current = store
            .current()?
            .context("there's no current deployment to stage from")?;
        Staging::stage(store, number, current, lock, store.staging_path(number))
    }

    /// Stages deployment `number` from the committed deployment `base`, as a rollback to a dropped generation does.
    pub fn from(store: &Store, number: u32, base: u32) -> Result<Staging> {
        let lock = store.lock()?;
        Staging::stage(store, number, base, lock, store.staging_path(number))
    }

    /// A staging root identical to deployment `base`, never committed, for building that deployment's boot entry; `discard` deletes it.
    pub fn scratch(store: &Store, base: u32) -> Result<Staging> {
        let lock = store.lock()?;
        let dir = store.scratch_path();
        Staging::stage(store, base, base, lock, dir)
    }

    fn stage(store: &Store, number: u32, base_number: u32, lock: StoreLock, dir: PathBuf) -> Result<Staging> {
        mark("deploy-stage");
        store.remove_leftovers()?;
        if !store.deployments()?.contains(&base_number) {
            bail!("deployment {base_number} isn't a complete committed deployment to stage from");
        }
        let base = store.path(base_number);
        if number != base_number && store.path(number).symlink_metadata().is_ok() {
            bail!("deployment {number} already exists");
        }
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
            store: store.clone(),
            number,
            base_number,
            base,
            dir,
            lock,
        })
    }

    pub fn number(&self) -> u32 {
        self.number
    }

    /// The committed deployment this one is staged from.
    pub fn base(&self) -> u32 {
        self.base_number
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// A tree that is a plain directory throughout staging, for archix's own writes: etc, pacman or dkms.
    pub fn tree(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Where the file at the absolute `path` of the staging root is on disk, for reading from outside the view: on the overlay backend, the upper layer's copy if it has one, else the base deployment's.
    pub fn read_path(&self, path: &Path) -> Result<PathBuf> {
        let (tree, rest) = split_tree(path)?;
        if self.store.backend() == Backend::Snapshot || !OVERLAID.contains(&tree) {
            return Ok(self.dir.join(tree).join(rest));
        }
        let upper = self.dir.join(".upper").join(tree).join(rest);
        Ok(match fs::symlink_metadata(&upper) {
            Ok(_) => upper,
            Err(_) => self.base.join(tree).join(rest),
        })
    }

    /// Deletes this staging tree without committing it.
    pub fn discard(self) -> Result<()> {
        self.store.remove_tree(&self.dir)
    }

    /// The staging root to run pacman, mkinitcpio and the like in.
    pub fn transaction(&self) -> Result<Transaction> {
        self.transaction_with(&[])
    }

    /// The staging root with live paths bound in, each at an absolute path of the view.
    pub fn transaction_with(&self, binds: &[(PathBuf, PathBuf)]) -> Result<Transaction> {
        mark("deploy-transact");
        Transaction::new(&Layout {
            backend: self.store.backend(),
            root: self.store.root(),
            base: &self.base,
            staging: &self.dir,
            binds,
        })
    }

    /// Makes the staging tree a complete deployment: overlays flattened, /var's upper layer kept as var-upper, scratch removed.
    pub fn finish(self) -> Result<Staged> {
        mark("deploy-flatten");
        let view = self.dir.join(VIEW_DIR);
        let mountinfo = fs::read("/proc/self/mountinfo").context("reading /proc/self/mountinfo")?;
        if let Some(mounted) = mount_points(&mountinfo)
            .into_iter()
            .find(|point| point.starts_with(&view))
        {
            bail!("{} is mounted; the staging root is still in use", mounted.display());
        }
        remove_any(&view)?;
        if self.number == self.base_number {
            bail!("a scratch staging root can't become a deployment");
        }
        if self.store.backend() == Backend::Overlay {
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
            _lock: self.lock,
        };
        staged.check()?;
        Ok(staged)
    }
}

impl Staged {
    /// Refuses a tree that lacks part of a deployment or still holds staging scratch.
    pub fn check(&self) -> Result<()> {
        match incomplete(&self.dir) {
            Some(why) => bail!("{why}; it isn't a complete deployment"),
            None => Ok(()),
        }
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

/// The deployment tree an absolute path is in, and the rest of it: /etc/x is ("etc", "x").
fn split_tree(path: &Path) -> Result<(&'static str, &Path)> {
    let rel = path
        .strip_prefix("/")
        .with_context(|| format!("{} isn't an absolute path", path.display()))?;
    for tree in ["usr", "etc", "opt", "boot"] {
        if let Ok(rest) = rel.strip_prefix(tree) {
            return Ok((tree, rest));
        }
    }
    bail!("{} isn't in a deployment's usr, etc, opt or boot", path.display())
}

pub(super) fn remove_any(path: &Path) -> Result<()> {
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

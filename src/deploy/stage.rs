use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use super::store::{StoreLock, incomplete};
use super::view::{Layout, VAR_BINDS, VAR_LIVE_IF_PRESENT, VIEW_DIR, mount_points};
use super::{Backend, COPIED, OVERLAID, Store, Transaction};
use crate::system::activate::Owned;
use crate::testhooks::mark;

/// The view's /var lower layer in a staging tree: a private copy of what the base deployment's packages own in the live /var.
pub(super) const VAR_LOWER: &str = ".var-lower";

/// On btrfs, the base deployment's own var-upper, kept from its snapshot when its first boot hasn't applied it yet.
pub(super) const VAR_BASE: &str = ".var-base";

/// A deployment being built in `deploy/.staging-<N>` from a committed one; deleted at the next switch unless committed.
#[derive(Debug)]
pub struct Staging {
    store: Store,
    number: u32,
    base_number: u32,
    base: PathBuf,
    dir: PathBuf,
    lock: StoreLock,
    var_lowers: Vec<PathBuf>,
    /// The base deployment's var-upper when no boot applied it yet, which this deployment's var-upper must carry.
    carried: Option<PathBuf>,
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
        let unapplied = !var_applied(store, &base, base_number) && has_entries(&base.join("var-upper"));
        let mut carried = None;
        match store.backend() {
            Backend::Snapshot => {
                run(Command::new("btrfs")
                    .args(["-q", "subvolume", "snapshot"])
                    .arg(&base)
                    .arg(&dir))?;
                if unapplied {
                    fs::rename(dir.join("var-upper"), dir.join(VAR_BASE))
                        .with_context(|| format!("keeping deployment {base_number}'s /var changes"))?;
                    carried = Some(dir.join(VAR_BASE));
                } else {
                    remove_any(&dir.join("var-upper"))?;
                }
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
                if unapplied {
                    carried = Some(base.join("var-upper"));
                }
            }
        }
        for tree in COPIED {
            if !dir.join(tree).is_dir() {
                fs::create_dir(dir.join(tree)).with_context(|| format!("creating {}", dir.join(tree).display()))?;
            }
        }
        let var_lower = dir.join(VAR_LOWER);
        private_var(
            &store.root().join("var"),
            &crate::system::activate::package_owned(&base.join("pacman")),
            &var_lower,
        )?;
        layer_dirs(&dir, "var", &var_lower)?;
        let var_lowers = carried.iter().cloned().chain([var_lower]).collect();
        Ok(Staging {
            store: store.clone(),
            number,
            base_number,
            base,
            dir,
            lock,
            var_lowers,
            carried,
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
        self.view(binds, false)
    }

    /// The staging root with the signing keys readable, for the boot build's ukify; never for package scriptlets.
    pub fn signing_transaction(&self) -> Result<Transaction> {
        self.view(&[], true)
    }

    fn view(&self, binds: &[(PathBuf, PathBuf)], signing: bool) -> Result<Transaction> {
        mark("deploy-transact");
        Transaction::new(&Layout {
            signing,
            backend: self.store.backend(),
            root: self.store.root(),
            base: &self.base,
            staging: &self.dir,
            var_lowers: &self.var_lowers,
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
        for bind in VAR_BINDS.iter().chain(VAR_LIVE_IF_PRESENT) {
            remove_any(&var.join(bind))?;
        }
        let var_upper = self.dir.join("var-upper");
        match &self.carried {
            Some(base_upper) => super::flatten::flatten(base_upper, &var, &var_upper)?,
            None => fs::rename(&var, &var_upper)
                .with_context(|| format!("moving {} to {}", var.display(), var_upper.display()))?,
        }
        for scratch in [".upper", ".work", VAR_LOWER, VAR_BASE] {
            remove_any(&self.dir.join(scratch))?;
        }
        let stray = crate::system::usr_local::staged_files(&self.dir);
        if !stray.is_empty() {
            let shown: Vec<&str> = stray.iter().take(20).map(String::as_str).collect();
            bail!(
                "the new deployment's packages or their install scripts put files into /usr/local, which every deployment shares from /var/usrlocal and which no package may install to: {}{}; nothing was changed. Build such a package to install under /usr or /opt instead",
                shown.join(", "),
                match stray.len() - shown.len() {
                    0 => String::new(),
                    n => format!(" and {n} more"),
                }
            );
        }
        if !self.dir.join("usr/local").is_dir() {
            fs::create_dir(self.dir.join("usr/local"))
                .context("creating the new deployment's /usr/local mountpoint")?;
        }
        let carried_record = self.base.join(crate::system::activate::VAR_BACKUP);
        crate::system::activate::write_var_backup(
            &self.dir,
            &self.base.join("pacman"),
            self.carried.is_some().then_some(carried_record.as_path()),
        )?;
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
    )
    .with_context(|| format!("chmod {}", upper.display()))?;
    super::flatten::set_times(&upper, &meta)
}

/// Whether a boot of deployment `number` applied its var-upper to the live /var.
fn var_applied(store: &Store, deployment: &Path, number: u32) -> bool {
    let state = crate::paths::Paths::under(store.root()).state;
    crate::system::activate::deployment_id(deployment, number).is_ok_and(|id| state.join("activated").join(id).exists())
}

fn has_entries(dir: &Path) -> bool {
    fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some())
}

/// Builds `out`, the view's /var lower layer: a private copy of the entries `owned` names under the live /var and of their parents, so the overlay's lower never changes while it's mounted, as overlayfs requires.
pub fn private_var(live: &Path, owned: &Owned, out: &Path) -> Result<()> {
    fs::create_dir(out).with_context(|| format!("creating {}", out.display()))?;
    copy_entry_meta(live, out)?;
    for rel in owned.keys() {
        let mut at = PathBuf::new();
        for part in rel.iter() {
            at.push(part);
            let (from, to) = (live.join(&at), out.join(&at));
            if let Ok(meta) = fs::symlink_metadata(&to) {
                if meta.is_dir() {
                    continue;
                }
                break;
            }
            let Ok(meta) = fs::symlink_metadata(&from) else { break };
            let kind = meta.file_type();
            if kind.is_dir() {
                fs::create_dir(&to).with_context(|| format!("creating {}", to.display()))?;
            } else if at != *rel {
                break;
            } else if kind.is_symlink() {
                let target = fs::read_link(&from).with_context(|| format!("reading {}", from.display()))?;
                std::os::unix::fs::symlink(target, &to).with_context(|| format!("creating {}", to.display()))?;
            } else if kind.is_file() {
                fs::copy(&from, &to).with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
            } else {
                break;
            }
            copy_entry_meta(&from, &to)?;
        }
    }
    Ok(())
}

/// Owner, mode, extended attributes and times of `from` onto `to`, neither followed if a symlink.
fn copy_entry_meta(from: &Path, to: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(from).with_context(|| format!("stat {}", from.display()))?;
    std::os::unix::fs::lchown(to, Some(meta.uid()), Some(meta.gid()))
        .with_context(|| format!("chown {}", to.display()))?;
    if !meta.file_type().is_symlink() {
        fs::set_permissions(to, std::os::unix::fs::PermissionsExt::from_mode(meta.mode() & 0o7777))
            .with_context(|| format!("chmod {}", to.display()))?;
    }
    for name in super::xattr::list(from).unwrap_or_default() {
        if super::xattr::is_overlay_private(&name) {
            continue;
        }
        if let Ok(Some(value)) = super::xattr::get(from, &name) {
            let _ = super::xattr::set(to, &name, &value);
        }
    }
    super::flatten::set_times(to, &meta)
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

pub(crate) fn remove_any(path: &Path) -> Result<()> {
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

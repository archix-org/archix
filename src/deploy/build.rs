use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use super::{Keep, Staging, Store};
use crate::generations::Generation;
use crate::packages::Packages;
use crate::paths::Paths;
use crate::repos::Repo;
use crate::system::apply::{Accounts, SystemChange, deployment_tree};
use crate::system::boot::{self, BootPlan, Entries};

/// Everything one deployment is built from, prepared before anything is staged.
pub struct Plan<'a> {
    pub paths: &'a Paths,
    pub number: u32,
    /// The committed deployment it's staged from.
    pub base: u32,
    pub packages: &'a Packages,
    pub system: &'a SystemChange,
    pub boot: &'a BootPlan,
    pub repos: &'a [Repo],
    /// Its generation record, kept in the tree and saved to the store once the commit succeeded, unless the store has it, as for a rebuilt rollback.
    pub record: &'a Generation,
    /// `deployments.keep`, which decides whose boot entries may go when the ESP is short of room.
    pub keep: usize,
    /// Puts the config as this switch leaves it into the staging tree at the given directory, before the commit.
    pub config: &'a dyn Fn(&Path) -> Result<()>,
}

/// What a committed deployment left for after its commit.
pub struct Built {
    /// Regular users it created, whose homes go on the live system.
    pub users: Vec<String>,
    /// Units it couldn't enable or disable.
    pub failures: Vec<String>,
}

/// Stages, builds and commits one deployment: the package changes, users, /etc files and services, archix's integration files and the boot entry go into the staging root, /usr and /opt files into the finished tree; nothing outside it changes before the commit, and a failure deletes the staging tree.
pub fn build(store: &Store, plan: &Plan) -> Result<Built> {
    let built = build_staged(store, plan);
    if built.is_err() {
        remove_staging(store, &store.staging_path(plan.number));
    }
    built
}

/// Deletes a staging tree a failed build left, so a full disk gets its space back now rather than at the next switch.
fn remove_staging(store: &Store, staging: &Path) {
    if staging.symlink_metadata().is_err() {
        return;
    }
    let removed = store.lock().and_then(|_lock| store.remove_tree(staging));
    if let Err(e) = removed {
        eprintln!(
            "warning: couldn't remove {}: {e:#}; the next switch does",
            staging.display()
        );
    }
}

fn build_staged(store: &Store, plan: &Plan) -> Result<Built> {
    let paths = plan.paths;
    let staging = Staging::from(store, plan.number, plan.base)?;
    let tx = staging.transaction()?;
    let declared = crate::etcmerge::ids::Declared {
        names: plan
            .system
            .to
            .users
            .keys()
            .chain(plan.system.to.groups.keys())
            .cloned()
            .collect(),
        uids: plan.system.to.users.values().filter_map(|user| user.uid).collect(),
        gids: plan.system.to.groups.values().filter_map(|group| group.gid).collect(),
    };
    crate::etcmerge::ids::hold(&paths.state, &staging.tree("etc"), &declared)?;
    let log = store.root().join("var/log/pacman.log");
    let logged = std::fs::metadata(&log).map_or(0, |meta| meta.len());
    crate::packages::transact(&tx, plan.packages, &staging.tree("pacman")).map_err(|e| exdev_hint(e, &log, logged))?;
    let run = |argv: &[String]| -> Result<()> {
        let output = tx.output(argv)?;
        if !output.status.success() {
            bail!(
                "{} failed in the new deployment: {}",
                argv.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    };
    let etc = staging.tree("etc");
    let users = crate::system::apply::users(plan.system, &etc, &run, &paths.state)?;
    crate::etcmerge::ids::release(&paths.state, &etc)?;
    let accounts = Accounts::read(&etc);
    let mut failures = crate::system::apply::disable_services(plan.system, &run);
    let in_etc = |path: &str| (deployment_tree(path) == Some("etc")).then(|| etc.join(&path["/etc/".len()..]));
    crate::system::apply::files(paths, plan.system, &in_etc, &accounts)?;
    failures.extend(crate::system::apply::enable_services(plan.system, &run));
    crate::integration::write_staged(&staging, &crate::integration::own_path()?)?;
    crate::integration::sync_pacman_conf(&etc.join("pacman.conf"), plan.repos)?;
    (plan.config)(staging.dir())?;
    let built = boot::build(paths, plan.boot, plan.number, &staging, &tx)?;
    drop(tx);
    crate::generations::write_into(staging.dir(), plan.record)?;
    let staged = staging.finish()?;
    let dir = staged.dir.clone();
    let in_trees = |path: &str| matches!(deployment_tree(path), Some("usr" | "opt")).then(|| dir.join(&path[1..]));
    crate::system::apply::files(paths, plan.system, &in_trees, &accounts)?;
    let esp = PathBuf::from(&plan.boot.esp);
    let mut pins = Keep::read(plan.keep, store, &esp)?;
    pins.current = Some(plan.number);
    pins.chosen = Some(plan.number);
    let mut committed = store.deployments()?;
    committed.push(plan.number);
    let entries = Entries {
        paths,
        plan: plan.boot,
        built: &built,
        droppable: pins.to_delete(&committed),
    };
    staged.commit(store, &esp, &entries)?;
    if let Err(e) = store.choose(plan.number) {
        eprintln!("warning: couldn't record deployment {} as chosen: {e:#}", plan.number);
    }
    let records = crate::generations::Store::new(paths);
    if !records.has(plan.number) {
        records.save(plan.record)?;
    }
    Ok(Built { users, failures })
}

/// Explains a failed transaction whose scriptlet output in pacman's log, from byte `from` on, shows a rename refused across the staging overlay.
pub fn exdev_hint(error: anyhow::Error, log: &Path, from: u64) -> anyhow::Error {
    let text = std::fs::read(log).unwrap_or_default();
    let new = String::from_utf8_lossy(text.get(from as usize..).unwrap_or_default());
    if !new.contains("Invalid cross-device link") {
        return error;
    }
    error.context(
        "a package's install script renamed a directory under /usr, /opt or /boot, which the staging overlay refuses on this filesystem (EXDEV, see pacman.log); its package needs to copy instead, or the switch runs on btrfs",
    )
}

/// Builds and writes the boot entry of the committed deployment `number`, in a scratch staging root identical to it, as for deployment 1 after the first layout or a current deployment whose entry is gone; it becomes the boot default.
pub fn entry_for(store: &Store, paths: &Paths, plan: &BootPlan, number: u32) -> Result<()> {
    let written = write_entry(store, paths, plan, number);
    if written.is_err() {
        remove_staging(store, &store.scratch_path());
    }
    written
}

fn write_entry(store: &Store, paths: &Paths, plan: &BootPlan, number: u32) -> Result<()> {
    let staging = Staging::scratch(store, number)?;
    let tx = staging.transaction()?;
    let built = boot::build(paths, plan, number, &staging, &tx)?;
    drop(tx);
    let source = staging.read_path(Path::new("/boot/archix"))?;
    let root = source.ancestors().nth(2).unwrap_or(&source).to_path_buf();
    let entries = Entries {
        paths,
        plan,
        built: &built,
        droppable: Vec::new(),
    };
    super::EspStep::clean(&entries)?;
    super::EspStep::install(&entries, number, &root)?;
    crate::durable::sync_filesystems_checked(&[Path::new(&plan.esp)])?;
    staging.discard()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rename_the_overlay_refused_in_this_run_is_named_in_the_error() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("pacman.log");
        std::fs::write(&log, "[ALPM-SCRIPTLET] mv: Invalid cross-device link\n").unwrap();
        let from = std::fs::metadata(&log).unwrap().len();
        let plain = format!("{:#}", exdev_hint(anyhow::anyhow!("pacman failed"), &log, from));
        assert_eq!(plain, "pacman failed", "an earlier run's line doesn't count");
        std::fs::write(
            &log,
            "[ALPM-SCRIPTLET] mv: Invalid cross-device link\n[ALPM-SCRIPTLET] rename: Invalid cross-device link\n",
        )
        .unwrap();
        let hinted = format!("{:#}", exdev_hint(anyhow::anyhow!("pacman failed"), &log, from));
        assert!(
            hinted.contains("renamed a directory under /usr, /opt or /boot") && hinted.ends_with("pacman failed"),
            "{hinted}"
        );
    }
}

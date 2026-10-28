//! Ejecting a system that boots through deployments back to plain Arch: one deployment's trees are laid out as the root's own, the ESP is handed back to a plain UKI, then /archix goes, each step recorded so a crash leaves a bootable system and a rerun finishes.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use super::{Backend, Store};
use crate::paths::Paths;
use crate::system::deploy_boot::TREES;
use crate::system::{SystemState, boot};
use crate::testhooks::mark;

/// How far an eject got, in order; each is recorded only once it's on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    /// The root's own trees are being filled from the deployment; deployments still boot.
    Copying,
    /// The root's own trees hold the deployment, without archix's integration; deployments still boot.
    Copied,
    /// The ESP boots the root's own trees and nothing of archix's; /archix may still be there.
    Plain,
}

#[derive(Debug, Clone, PartialEq)]
pub struct State {
    pub deployment: u32,
    pub phase: Phase,
    pub restore_originals: bool,
}

fn state_path(state_dir: &Path) -> PathBuf {
    state_dir.join("eject")
}

/// The eject under way, if one is.
pub fn read_state(state_dir: &Path) -> Result<Option<State>> {
    let path = state_path(state_dir);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let field = |key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix(' '))
            .map(str::trim)
    };
    let phase = match field("phase") {
        Some("copying") => Phase::Copying,
        Some("copied") => Phase::Copied,
        Some("plain") => Phase::Plain,
        _ => bail!("{} names no phase archix knows", path.display()),
    };
    let deployment = field("deployment")
        .and_then(|n| n.parse().ok())
        .with_context(|| format!("{} names no deployment", path.display()))?;
    Ok(Some(State {
        deployment,
        phase,
        restore_originals: field("restore-originals") == Some("yes"),
    }))
}

pub fn write_state(state_dir: &Path, state: &State) -> Result<()> {
    let phase = match state.phase {
        Phase::Copying => "copying",
        Phase::Copied => "copied",
        Phase::Plain => "plain",
    };
    let text = format!(
        "deployment {}\nphase {phase}\nrestore-originals {}\n",
        state.deployment,
        if state.restore_originals { "yes" } else { "no" }
    );
    crate::durable::write(&state_path(state_dir), text.as_bytes())
}

pub fn clear_state(state_dir: &Path) -> Result<()> {
    match fs::remove_file(state_path(state_dir)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", state_path(state_dir).display()))
        }
        _ => Ok(()),
    }
}

fn identity(path: &Path) -> Option<(u64, u64)> {
    fs::metadata(path).ok().map(|meta| (meta.dev(), meta.ino()))
}

fn c_path(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).with_context(|| format!("{} has a NUL byte", path.display()))
}

/// Whether `root`'s trees are bound from a deployment in this mount namespace, as on a system booted through one.
pub fn running_on_deployment(root: &Path) -> bool {
    let Ok(store) = Store::open(root) else {
        return false;
    };
    let trees = deployment_trees(&store);
    TREES
        .iter()
        .any(|(_, at)| identity(&root.join(at)).is_some_and(|id| trees.contains(&id)))
}

fn deployment_trees(store: &Store) -> Vec<(u64, u64)> {
    let names: Vec<PathBuf> = fs::read_dir(store.deploy_dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .collect();
    names
        .iter()
        .flat_map(|dir| TREES.iter().filter_map(move |(tree, _)| identity(&dir.join(tree))))
        .collect()
}

/// Detaches every deployment bind over `root`'s trees in this mount namespace, so its paths reach the root's own directories; processes keep what they have open.
pub fn unbind(root: &Path) -> Result<()> {
    let Ok(store) = Store::open(root) else {
        return Ok(());
    };
    let trees = deployment_trees(&store);
    for (_, at) in TREES {
        let target = root.join(at);
        for _ in 0..8 {
            if !identity(&target).is_some_and(|id| trees.contains(&id)) {
                break;
            }
            let c = c_path(&target)?;
            if unsafe { libc::umount2(c.as_ptr(), libc::MNT_DETACH) } != 0 {
                return Err(io::Error::last_os_error()).with_context(|| format!("unmounting {}", target.display()));
            }
        }
        if identity(&target).is_some_and(|id| trees.contains(&id)) {
            bail!("{} stays bound from a deployment", target.display());
        }
    }
    Ok(())
}

fn cp(args: &[&str], from: &Path, to: &Path) -> Result<()> {
    let output = Command::new("cp")
        .args(args)
        .arg("--no-target-directory")
        .arg(from)
        .arg(to)
        .output()
        .context("running cp")?;
    if !output.status.success() {
        bail!(
            "copying {} to {} failed: {}",
            from.display(),
            to.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Fills the usr, etc, opt, boot, pacman database and DKMS state under `root`, a bind of the root filesystem without its submounts, from deployment `number` there, emptying them first: hardlinks for usr, opt and boot on ext4 (the deployment's files outlive /archix through them) and real copies of the rest, reflinks on btrfs; then /usr/local from the shared /var/usrlocal, the same way; archix's built UKIs stay behind.
fn copy_trees(root: &Path, store: &Store, number: u32) -> Result<()> {
    let deployment = root.join("archix/deploy").join(number.to_string());
    for (i, (tree, at)) in TREES.iter().enumerate() {
        let target = root.join(at);
        fs::create_dir_all(&target).with_context(|| format!("creating {}", target.display()))?;
        for entry in fs::read_dir(&target).with_context(|| format!("reading {}", target.display()))? {
            super::stage::remove_any(&entry.with_context(|| format!("reading {}", target.display()))?.path())?;
        }
        let how: &[&str] = match (store.backend(), ["usr", "opt", "boot"].contains(tree)) {
            (Backend::Snapshot, _) => &["-a", "--reflink=always"],
            (Backend::Overlay, true) => &["-al"],
            (Backend::Overlay, false) => &["-a"],
        };
        cp(how, &deployment.join(tree), &target)?;
        if i == 0 {
            mark("eject-copying");
        }
    }
    let shared = crate::system::usr_local::dir(root);
    if shared.is_dir() {
        let local = root.join("usr/local");
        for entry in fs::read_dir(&local).with_context(|| format!("reading {}", local.display()))? {
            super::stage::remove_any(&entry.with_context(|| format!("reading {}", local.display()))?.path())?;
        }
        let how: &[&str] = match store.backend() {
            Backend::Snapshot => &["-a", "--reflink=auto"],
            Backend::Overlay => &["-al"],
        };
        cp(how, &shared, &local)?;
    }
    super::stage::remove_any(&root.join("boot/archix"))
}

/// Deletes the shared /var/usrlocal once the root's own /usr/local holds its files and nothing boots deployments any more.
fn remove_shared_usr_local(root: &Path) -> Result<()> {
    super::stage::remove_any(&crate::system::usr_local::dir(root))
}

/// Takes archix's integration off the root's own trees and puts back or unlocks the files generation `number` manages, as the eject of a plain system does.
fn strip(root: &Path, paths: &Paths, number: u32, restore_originals: bool) -> Result<()> {
    let under = Paths {
        root: root.to_path_buf(),
        ..paths.clone()
    };
    crate::integration::remove_from(&under)?;
    let Ok(generation) = crate::generations::Store::new(paths).get(number) else {
        return Ok(());
    };
    let mut files = generation.system.files.clone();
    // mkinitcpio reads the kernel command line for UKIs from here.
    files.remove("/etc/kernel/cmdline");
    let inside = |path: &str| root.join(path.trim_start_matches('/'));
    if restore_originals {
        let change = crate::system::apply::SystemChange {
            from: SystemState {
                files,
                ..SystemState::default()
            },
            to: SystemState::default(),
        };
        let live = |path: &str| Some(inside(path));
        let accounts = crate::system::apply::Accounts::read(&root.join("etc"));
        crate::system::apply::files(&under, &change, &live, &accounts)?;
    } else {
        for (path, file) in &files {
            let writable = file.mode | 0o200;
            if file.link.is_none() && file.merge.is_none() && writable != file.mode {
                let _ = fs::set_permissions(inside(path), std::os::unix::fs::PermissionsExt::from_mode(writable));
            }
        }
    }
    Ok(())
}

/// Mounts proc, sys and dev into a root that isn't this namespace's own, for mkinitcpio run chrooted there.
pub fn mount_api_filesystems(root: &Path) -> Result<()> {
    for (dir, source, fstype, flags) in [
        ("proc", "proc", Some("proc"), 0),
        ("sys", "/sys", None, libc::MS_BIND | libc::MS_REC),
        ("dev", "/dev", None, libc::MS_BIND | libc::MS_REC),
    ] {
        let target = root.join(dir);
        fs::create_dir_all(&target).with_context(|| format!("creating {}", target.display()))?;
        if fs::read_dir(&target).is_ok_and(|mut entries| entries.next().is_some()) {
            continue;
        }
        let (source_c, target_c) = (CString::new(source)?, c_path(&target)?);
        let fstype = fstype.map(CString::new).transpose()?;
        let done = unsafe {
            libc::mount(
                source_c.as_ptr(),
                target_c.as_ptr(),
                fstype.as_ref().map_or(std::ptr::null(), |f| f.as_ptr()),
                flags,
                std::ptr::null(),
            )
        };
        if done != 0 {
            return Err(io::Error::last_os_error()).with_context(|| format!("mounting {}", target.display()));
        }
    }
    Ok(())
}

/// Hands the ESP to a plain UKI of the root's own trees, chrooted into `root` in a child unless it's this namespace's root.
fn hand_over(root: &Path, esp: &Path, default: &str) -> Result<Vec<String>> {
    if root == Path::new("/") {
        let esp = esp.to_str().context("the ESP's path isn't UTF-8")?;
        return boot::hand_over(esp, &boot::installed_kernels(), default, boot::sbctl_has_keys());
    }
    let inside = Path::new("/").join(esp.strip_prefix(root).with_context(|| {
        format!(
            "the ESP at {} isn't under the root at {}",
            esp.display(),
            root.display()
        )
    })?);
    let sign = root.join("var/lib/sbctl/keys/db/db.key").is_file();
    mount_api_filesystems(root)?;
    let lines = root.join(".archix-eject-lines");
    let root_c = c_path(root)?;
    match unsafe { libc::fork() } {
        -1 => Err(io::Error::last_os_error()).context("forking to hand the ESP over"),
        0 => {
            let code = (|| -> Result<()> {
                if unsafe { libc::chroot(root_c.as_ptr()) } != 0 || unsafe { libc::chdir(c"/".as_ptr()) } != 0 {
                    return Err(io::Error::last_os_error()).context("chrooting");
                }
                let esp = inside.to_str().context("the ESP's path isn't UTF-8")?;
                let written = boot::hand_over(esp, &boot::installed_kernels(), default, sign)?;
                fs::write("/.archix-eject-lines", written.join("\n"))?;
                Ok(())
            })()
            .map_or_else(
                |e| {
                    eprintln!("{e:#}");
                    1
                },
                |()| 0,
            );
            unsafe { libc::_exit(code) }
        }
        child => {
            let mut status = 0;
            unsafe { libc::waitpid(child, &mut status, 0) };
            let written = fs::read_to_string(&lines).unwrap_or_default();
            let _ = fs::remove_file(&lines);
            if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
                bail!("handing the ESP over to the plain system failed");
            }
            Ok(written.lines().map(str::to_string).collect())
        }
    }
}

/// The ESP archix writes boot entries to on the system with state in `paths`.
pub fn esp(paths: &Paths) -> Option<PathBuf> {
    fs::read_to_string(paths.state.join("boot/esp"))
        .ok()
        .map(|esp| PathBuf::from(esp.trim()))
}

/// The kernel package whose UKI becomes the default, from the deployment's generation.
fn default_kernel(paths: &Paths, number: u32) -> String {
    crate::generations::Store::new(paths)
        .get(number)
        .ok()
        .and_then(|generation| generation.settings.boot)
        .map_or_else(|| "linux".to_string(), |boot| boot.kernel)
}

/// The work of `archix eject-here`, in a private mount namespace: the deployment binds come off there, the root's own trees are filled and stripped, then the ESP is handed over; returns what the handover wrote.
pub fn here(root: &Path, paths: &Paths) -> Result<Vec<String>> {
    let Some(mut state) = read_state(&paths.state)? else {
        bail!("no eject is under way");
    };
    let store = Store::open(root)?;
    if state.phase < Phase::Copied {
        if !running_on_deployment(root) {
            bail!(
                "{}'s trees aren't bound from a deployment, so they may be what's running; nothing was copied over them",
                root.display()
            );
        }
        // The deployment's binds stay while the copy runs, since cp and its libraries come from them; the root's own /usr may be empty.
        in_plain_view(root, |view| {
            copy_trees(view, &store, state.deployment)?;
            strip(view, paths, state.deployment, state.restore_originals)
        })?;
        crate::durable::sync_filesystems_checked(&[root])?;
        state.phase = Phase::Copied;
        write_state(&paths.state, &state)?;
        mark("eject-copied");
    }
    unbind(root)?;
    let mut written = Vec::new();
    if state.phase < Phase::Plain {
        let esp = esp(paths).context("archix doesn't know the ESP, so there's no boot entry to hand over")?;
        written = hand_over(root, &esp, &default_kernel(paths, state.deployment))?;
        state.phase = Phase::Plain;
        write_state(&paths.state, &state)?;
        mark("eject-plain");
    }
    Ok(written)
}

/// Runs `archix` (`eject-here`) in a private mount namespace, so the running system keeps its deployment binds while the root's own trees are filled.
fn run_here(root: &Path, paths: &Paths, archix: &Path) -> Result<Vec<String>> {
    let mut command = Command::new(archix);
    command.arg("eject-here").arg(root).arg("--state").arg(&paths.state);
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut command, || {
            if libc::unshare(libc::CLONE_NEWNS) != 0 {
                return Err(io::Error::last_os_error());
            }
            let flags = libc::MS_REC | libc::MS_PRIVATE;
            if libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                flags,
                std::ptr::null(),
            ) != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = command.output().context("running archix eject-here")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect())
}

/// Deletes /archix: `current` first, so nothing takes the root for one laid out any more, then each deployment and the rest.
fn remove_store(root: &Path) -> Result<()> {
    let dir = root.join("archix");
    if dir.symlink_metadata().is_err() {
        return Ok(());
    }
    // The boot binds /archix read-only over itself, in the system's namespace and so in archix's copy of it.
    detach(&dir)?;
    in_init_namespace(|| detach(&dir)).context("unmounting /archix")?;
    let store = Store::open(root)?;
    match fs::remove_file(dir.join("current")) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e).context("removing /archix/current"),
        _ => {}
    }
    crate::durable::sync_dir(&dir.join("current"))?;
    let mut names: Vec<PathBuf> = fs::read_dir(store.deploy_dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .collect();
    names.sort();
    for (i, path) in names.iter().enumerate() {
        store.remove_tree(path)?;
        if i == 0 {
            mark("eject-deleting");
        }
    }
    fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))
}

/// Runs `work` on a bind of `root`'s filesystem without its submounts, so it reaches the root's own trees under the deployment binds and /archix's self-bind, on one mount that hardlinks and reflinks can't cross out of.
fn in_plain_view(root: &Path, work: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    let view = std::env::temp_dir().join(format!("archix-eject-root-{}", std::process::id()));
    fs::create_dir_all(&view).with_context(|| format!("creating {}", view.display()))?;
    let (root_c, view_c) = (c_path(root)?, c_path(&view)?);
    if unsafe {
        libc::mount(
            root_c.as_ptr(),
            view_c.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    } != 0
    {
        let e = io::Error::last_os_error();
        let _ = fs::remove_dir(&view);
        return Err(e).with_context(|| format!("binding {} to {}", root.display(), view.display()));
    }
    let done = work(&view);
    unsafe { libc::umount2(view_c.as_ptr(), libc::MNT_DETACH) };
    let _ = fs::remove_dir(&view);
    done
}

/// Detaches every mount at `path` in this mount namespace.
fn detach(path: &Path) -> Result<()> {
    let c = c_path(path)?;
    for _ in 0..8 {
        // unshare(CLONE_NEWNS) moves only the calling thread, and /proc/self is the main thread's.
        let mountinfo = fs::read("/proc/thread-self/mountinfo").context("reading /proc/thread-self/mountinfo")?;
        if !super::view::mount_points(&mountinfo).contains(&path.to_path_buf()) {
            return Ok(());
        }
        if unsafe { libc::umount2(c.as_ptr(), libc::MNT_DETACH) } != 0 {
            return Err(io::Error::last_os_error()).with_context(|| format!("unmounting {}", path.display()));
        }
    }
    bail!("{} stays mounted", path.display())
}

/// Runs `work` in the mount namespace of PID 1, where the running system's mounts are, since archix works in a copy of its own; in this process when it's that one.
fn in_init_namespace(work: impl FnOnce() -> Result<()>) -> Result<()> {
    let ns = |pid: &str| fs::read_link(format!("/proc/{pid}/ns/mnt")).ok();
    if ns("1").is_none() || ns("1") == ns("thread-self") {
        return work();
    }
    let init = fs::File::open("/proc/1/ns/mnt").context("opening PID 1's mount namespace")?;
    match unsafe { libc::fork() } {
        -1 => Err(io::Error::last_os_error()).context("forking"),
        0 => {
            let fd = std::os::fd::AsRawFd::as_raw_fd(&init);
            let code = if unsafe { libc::setns(fd, libc::CLONE_NEWNS) } != 0 {
                eprintln!("joining PID 1's mount namespace: {}", io::Error::last_os_error());
                1
            } else {
                work().map_or_else(
                    |e| {
                        eprintln!("{e:#}");
                        1
                    },
                    |()| 0,
                )
            };
            unsafe { libc::_exit(code) }
        }
        child => {
            let mut status = 0;
            unsafe { libc::waitpid(child, &mut status, 0) };
            if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
                bail!("it failed in PID 1's mount namespace");
            }
            Ok(())
        }
    }
}

/// What a fresh eject would lay out: the deployment the system runs from, which has to be the current one.
pub fn deployment_to_eject(root: &Path) -> Result<u32> {
    let store = Store::open(root)?;
    let current = store.current()?.context("there's no current deployment")?;
    let Some(booted) = boot::booted_deployment(root) else {
        bail!("this boot didn't come from a deployment, so there's none running to eject to");
    };
    if booted != current {
        bail!(
            "deployment {current} is current, but this boot runs deployment {booted}; restart into {current} (or `archix rollback {booted}`) and eject then; nothing was changed"
        );
    }
    if !store.deployments()?.contains(&booted) {
        bail!("deployment {booted} isn't complete; nothing was changed");
    }
    Ok(booted)
}

/// Ejects the system under `paths.root` from its deployments, resuming any eject under way; `archix` is the binary that runs `eject-here`. Returns what the ESP handover wrote.
pub fn eject(paths: &Paths, archix: &Path, restore_originals: bool) -> Result<Vec<String>> {
    let root = &paths.root;
    let lock = match root.join("archix").exists() {
        true => Some(Store::open(root)?.lock()?),
        false => None,
    };
    let state = match read_state(&paths.state)? {
        Some(state) if state.phase < Phase::Copied && running_on_deployment(root) => {
            // A switch since an unfinished eject started may have made another deployment current; the copy is redone anyway.
            let deployment = deployment_to_eject(root)?;
            State { deployment, ..state }
        }
        Some(state) => state,
        None => State {
            deployment: deployment_to_eject(root)?,
            phase: Phase::Copying,
            restore_originals,
        },
    };
    write_state(&paths.state, &state)?;
    let mut written = Vec::new();
    if state.phase < Phase::Plain {
        written = run_here(root, paths, archix)?;
    }
    unbind(root)?;
    in_init_namespace(|| unbind(root)).context("switching the running system to the root's own trees")?;
    let record = root.join(boot::DEPLOYMENT_RECORD.trim_start_matches('/'));
    match fs::remove_file(&record) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("removing {}", record.display()));
        }
        _ => {}
    }
    mark("eject-unbound");
    drop(lock);
    remove_store(root)?;
    remove_shared_usr_local(root)?;
    crate::durable::sync_filesystems_checked(&[root])?;
    mark("eject-removed");
    Ok(written)
}

/// Ejects the root mounted at `paths.root` from the rescue boot, where nothing runs from it: deployment `number` (current by default) becomes the root's own trees, the ESP at `esp`, mounted under the root, boots them through a plain UKI, and /archix goes; each phase is recorded, so a rerun, from the rescue or the booted system, finishes an eject a crash cut off.
pub fn offline(paths: &Paths, number: Option<u32>, esp: &Path) -> Result<Vec<String>> {
    let root = &paths.root;
    let store = Store::open(root)?;
    let lock = store.lock()?;
    let mut state = match read_state(&paths.state)? {
        Some(state) => state,
        None => {
            let deployment = match number {
                Some(number) => number,
                None => store.current()?.context("there's no current deployment")?,
            };
            if !store.deployments()?.contains(&deployment) {
                bail!("deployment {deployment} isn't complete; nothing was changed");
            }
            State {
                deployment,
                phase: Phase::Copying,
                restore_originals: false,
            }
        }
    };
    write_state(&paths.state, &state)?;
    if state.phase < Phase::Copied {
        in_plain_view(root, |view| {
            copy_trees(view, &store, state.deployment)?;
            strip(view, paths, state.deployment, state.restore_originals)
        })?;
        crate::durable::sync_filesystems_checked(&[root])?;
        state.phase = Phase::Copied;
        write_state(&paths.state, &state)?;
        mark("eject-copied");
    }
    let mut written = Vec::new();
    if state.phase < Phase::Plain {
        written = hand_over(root, esp, &default_kernel(paths, state.deployment))?;
        state.phase = Phase::Plain;
        write_state(&paths.state, &state)?;
        mark("eject-plain");
    }
    drop(lock);
    remove_store(root)?;
    remove_shared_usr_local(root)?;
    crate::durable::sync_filesystems_checked(&[root])?;
    clear_state(&paths.state)?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_eject_state_round_trips_and_phases_are_ordered() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_state(dir.path()).unwrap(), None);
        let state = State {
            deployment: 7,
            phase: Phase::Copied,
            restore_originals: true,
        };
        write_state(dir.path(), &state).unwrap();
        assert_eq!(read_state(dir.path()).unwrap(), Some(state));
        assert!(Phase::Copying < Phase::Copied && Phase::Copied < Phase::Plain);
        fs::write(state_path(dir.path()), "deployment 7\nphase sideways\n").unwrap();
        assert!(read_state(dir.path()).is_err());
        clear_state(dir.path()).unwrap();
        clear_state(dir.path()).unwrap();
        assert_eq!(read_state(dir.path()).unwrap(), None);
    }
}

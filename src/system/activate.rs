use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::{self, Metadata};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::paths::Paths;

/// Upper-layer paths, relative to /var, that activation never writes.
const KEEP_LIVE: &[&str] = &["log/pacman.log", ".updated"];

/// Trees under /var that belong to the deployment and are bind-mounted from it at boot.
const DEPLOYMENT_TREES: &[&str] = &["lib/pacman/local", "lib/dkms"];

/// What one upper-layer entry is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Dir,
    File,
    Symlink,
    /// A FIFO or device node other than a whiteout.
    Special,
    Socket,
    Whiteout,
}

/// What is at the same path under the live /var.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Live {
    Missing,
    Dir,
    LinkToDir,
    Other,
}

#[derive(Debug, PartialEq)]
pub enum Action {
    Keep,
    IgnoreWhiteout,
    MakeDir { aside: bool },
    UpdateDir,
    Through,
    Replace { aside: bool },
}

/// How one upper-layer entry is applied: create or replace, never delete; what's in the way of a type change is moved aside.
pub fn decide(rel: &Path, upper: Kind, live: Live) -> Action {
    if KEEP_LIVE.iter().any(|keep| rel == Path::new(keep))
        || DEPLOYMENT_TREES.iter().any(|tree| rel.starts_with(tree))
        || upper == Kind::Socket
    {
        return Action::Keep;
    }
    match (upper, live) {
        (Kind::Whiteout, _) => Action::IgnoreWhiteout,
        (Kind::Dir, Live::Missing) => Action::MakeDir { aside: false },
        (Kind::Dir, Live::Dir) => Action::UpdateDir,
        (Kind::Dir, Live::LinkToDir) => Action::Through,
        (Kind::Dir, Live::Other) => Action::MakeDir { aside: true },
        (_, Live::Dir) => Action::Replace { aside: true },
        _ => Action::Replace { aside: false },
    }
}

#[derive(Debug, Default)]
pub struct Applied {
    pub written: usize,
    pub whiteouts: usize,
    pub kept: Vec<PathBuf>,
    pub moved_aside: Vec<PathBuf>,
}

fn c_path(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).with_context(|| format!("{} has a NUL byte", path.display()))
}

fn os_error<T>(what: &str, path: &Path) -> Result<T> {
    Err(io::Error::last_os_error()).with_context(|| format!("{what} {}", path.display()))
}

fn xattr_names(path: &Path) -> Result<Vec<CString>> {
    let path_c = c_path(path)?;
    let size = unsafe { libc::llistxattr(path_c.as_ptr(), std::ptr::null_mut(), 0) };
    if size < 0 {
        return match io::Error::last_os_error().raw_os_error() {
            Some(libc::ENOTSUP) => Ok(Vec::new()),
            _ => os_error("listing the xattrs of", path),
        };
    }
    let mut buf = vec![0u8; size as usize];
    let size = unsafe { libc::llistxattr(path_c.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    if size < 0 {
        return os_error("listing the xattrs of", path);
    }
    buf.truncate(size as usize);
    Ok(buf
        .split(|b| *b == 0)
        .filter(|name| !name.is_empty())
        .filter_map(|name| CString::new(name).ok())
        .collect())
}

fn xattr(path: &Path, name: &CString) -> Result<Option<Vec<u8>>> {
    let path_c = c_path(path)?;
    let size = unsafe { libc::lgetxattr(path_c.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) };
    if size < 0 {
        return match io::Error::last_os_error().raw_os_error() {
            Some(libc::ENODATA | libc::ENOTSUP) => Ok(None),
            _ => os_error("reading an xattr of", path),
        };
    }
    let mut buf = vec![0u8; size as usize];
    let size = unsafe { libc::lgetxattr(path_c.as_ptr(), name.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    if size < 0 {
        return os_error("reading an xattr of", path);
    }
    buf.truncate(size as usize);
    Ok(Some(buf))
}

/// Copies every xattr but overlayfs's own, ACLs and file capabilities included.
fn copy_xattrs(from: &Path, to: &Path) -> Result<()> {
    let to_c = c_path(to)?;
    for name in xattr_names(from)? {
        if name.as_bytes().starts_with(b"trusted.overlay.") {
            continue;
        }
        let Some(value) = xattr(from, &name)? else { continue };
        if unsafe { libc::lsetxattr(to_c.as_ptr(), name.as_ptr(), value.as_ptr().cast(), value.len(), 0) } != 0 {
            return os_error(&format!("setting {} on", name.to_string_lossy()), to);
        }
    }
    Ok(())
}

pub fn kind_of(path: &Path, meta: &Metadata) -> Result<Kind> {
    let file_type = meta.file_type();
    Ok(if file_type.is_dir() {
        Kind::Dir
    } else if file_type.is_symlink() {
        Kind::Symlink
    } else if file_type.is_char_device() && meta.rdev() == 0 {
        Kind::Whiteout
    } else if file_type.is_file() {
        // Overlayfs may also mark a whiteout with an xattr on an empty file.
        let marked = meta.len() == 0 && xattr(path, &CString::from(c"trusted.overlay.whiteout"))?.is_some();
        if marked { Kind::Whiteout } else { Kind::File }
    } else if file_type.is_socket() {
        Kind::Socket
    } else {
        Kind::Special
    })
}

fn live_kind(path: &Path) -> Live {
    match fs::symlink_metadata(path) {
        Err(_) => Live::Missing,
        Ok(meta) if meta.is_dir() => Live::Dir,
        Ok(meta) if meta.file_type().is_symlink() && fs::metadata(path).is_ok_and(|m| m.is_dir()) => Live::LinkToDir,
        Ok(_) => Live::Other,
    }
}

/// Owner first, as chown drops setuid bits and file capabilities, then mode and xattrs.
fn set_meta(meta: &Metadata, to: &Path, from: &Path) -> Result<()> {
    std::os::unix::fs::lchown(to, Some(meta.uid()), Some(meta.gid()))
        .with_context(|| format!("chown {}", to.display()))?;
    if !meta.file_type().is_symlink() {
        fs::set_permissions(to, fs::Permissions::from_mode(meta.mode() & 0o7777))
            .with_context(|| format!("chmod {}", to.display()))?;
    }
    copy_xattrs(from, to)
}

fn set_times(meta: &Metadata, path: &Path) -> Result<()> {
    let path_c = c_path(path)?;
    let times = [
        libc::timespec {
            tv_sec: meta.atime(),
            tv_nsec: meta.atime_nsec(),
        },
        libc::timespec {
            tv_sec: meta.mtime(),
            tv_nsec: meta.mtime_nsec(),
        },
    ];
    if unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            path_c.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return os_error("setting the times of", path);
    }
    Ok(())
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// Renames what's at `path` to a free `<name>.archix-old[.N]`.
fn move_aside(path: &Path) -> Result<PathBuf> {
    let mut aside = with_suffix(path, ".archix-old");
    let mut n = 1;
    while fs::symlink_metadata(&aside).is_ok() {
        n += 1;
        aside = with_suffix(path, &format!(".archix-old.{n}"));
    }
    fs::rename(path, &aside).with_context(|| format!("moving {} aside", path.display()))?;
    Ok(aside)
}

/// Writes a copy of `from` next to `to` and renames it over, so `to` is never missing or torn.
fn replace(from: &Path, meta: &Metadata, kind: Kind, to: &Path) -> Result<()> {
    let tmp = with_suffix(to, ".archix-new");
    let _ = fs::remove_file(&tmp);
    match kind {
        Kind::File => {
            fs::copy(from, &tmp).with_context(|| format!("copying {} to {}", from.display(), tmp.display()))?;
        }
        Kind::Symlink => {
            std::os::unix::fs::symlink(fs::read_link(from)?, &tmp)
                .with_context(|| format!("creating {}", tmp.display()))?;
        }
        _ => {
            let tmp_c = c_path(&tmp)?;
            if unsafe { libc::mknod(tmp_c.as_ptr(), meta.mode(), meta.rdev()) } != 0 {
                return os_error("creating", &tmp);
            }
        }
    }
    set_meta(meta, &tmp, from)?;
    set_times(meta, &tmp)?;
    if kind == Kind::File {
        fs::File::open(&tmp)?.sync_all()?;
    }
    fs::rename(&tmp, to).with_context(|| format!("replacing {}", to.display()))
}

fn walk(upper: &Path, rel: &Path, var: &Path, applied: &mut Applied) -> Result<()> {
    let dir = upper.join(rel);
    let mut names: Vec<_> = fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<_>>()?;
    names.sort();
    for name in names {
        let rel = rel.join(name);
        let from = upper.join(&rel);
        let to = var.join(&rel);
        let meta = fs::symlink_metadata(&from).with_context(|| format!("reading {}", from.display()))?;
        let kind = kind_of(&from, &meta)?;
        match decide(&rel, kind, live_kind(&to)) {
            Action::Keep => applied.kept.push(rel),
            Action::IgnoreWhiteout => applied.whiteouts += 1,
            Action::MakeDir { aside } => {
                if aside {
                    applied.moved_aside.push(move_aside(&to)?);
                }
                fs::create_dir(&to).with_context(|| format!("creating {}", to.display()))?;
                set_meta(&meta, &to, &from)?;
                applied.written += 1;
                walk(upper, &rel, var, applied)?;
            }
            Action::UpdateDir => {
                set_meta(&meta, &to, &from)?;
                walk(upper, &rel, var, applied)?;
            }
            Action::Through => walk(upper, &rel, var, applied)?,
            Action::Replace { aside } => {
                if aside {
                    applied.moved_aside.push(move_aside(&to)?);
                }
                replace(&from, &meta, kind, &to)?;
                applied.written += 1;
            }
        }
    }
    Ok(())
}

/// Applies a deployment's /var upper layer onto the live /var with owners, modes and xattrs.
pub fn apply_upper(upper: &Path, var: &Path) -> Result<Applied> {
    let mut applied = Applied::default();
    walk(upper, Path::new(""), var, &mut applied)?;
    crate::durable::sync_filesystems(&[var]);
    Ok(applied)
}

/// Names that share an ID, or appear twice, in a passwd- or group-style file.
fn clashes(text: &str, file: &str, id: &str) -> Vec<String> {
    let mut by_id: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut names: BTreeMap<&str, usize> = BTreeMap::new();
    for line in text.lines() {
        if line.is_empty() || line.starts_with(['#', '+', '-']) {
            continue;
        }
        let fields: Vec<&str> = line.split(':').collect();
        if let (Some(name), Some(number)) = (fields.first(), fields.get(2)) {
            by_id.entry(number).or_default().push(name);
            *names.entry(name).or_default() += 1;
        }
    }
    let shared = by_id
        .iter()
        .filter(|(_, names)| names.len() > 1)
        .map(|(number, names)| format!("{id} {number} is shared by {} in {file}", names.join(", ")));
    let twice = names
        .iter()
        .filter(|(_, count)| **count > 1)
        .map(|(name, _)| format!("{name} is listed more than once in {file}"));
    shared.chain(twice).collect()
}

/// Stops activation when two accounts share a UID or GID, so files owned by one never become the other's.
pub fn check_ids(passwd: &str, group: &str) -> Result<()> {
    let found: Vec<String> = clashes(passwd, "/etc/passwd", "UID")
        .into_iter()
        .chain(clashes(group, "/etc/group", "GID"))
        .collect();
    if !found.is_empty() {
        bail!(
            "{}; services aren't started. Fix the account files and reboot, or boot the previous generation",
            found.join("; ")
        );
    }
    Ok(())
}

fn run_tool(root: &Path, program: &str, args: &[&str]) {
    let mut command = Command::new(program);
    command.args(args);
    if root != Path::new("/") {
        command.arg(format!("--root={}", root.display()));
    }
    match command.status() {
        Ok(status) if status.success() => {}
        Ok(status) => eprintln!("archix: warning: {program} {} failed ({status})", args.join(" ")),
        Err(e) => eprintln!("archix: warning: couldn't run {program}: {e}"),
    }
}

/// Early at boot: on a deployment's first boot, checks account IDs and applies its /var changes; refreshes the journal catalog whenever the booted deployment changes.
pub fn activate(root: &Path) -> Result<()> {
    let record = root.join("run/archix/deployment");
    let Ok(text) = fs::read_to_string(&record) else {
        return Ok(());
    };
    let generation: u32 = text
        .trim()
        .parse()
        .with_context(|| format!("{} doesn't name a deployment: {text:?}", record.display()))?;
    let state = Paths::under(root).state;
    let marker = state.join("activated").join(generation.to_string());
    if !marker.exists() {
        let read = |path: &str| fs::read_to_string(root.join(path)).with_context(|| format!("reading /{path}"));
        check_ids(&read("etc/passwd")?, &read("etc/group")?)?;
        run_tool(root, "systemd-sysusers", &[]);
        let upper = root
            .join("archix/deploy")
            .join(generation.to_string())
            .join("var-upper");
        if upper.is_dir() {
            let applied = apply_upper(&upper, &root.join("var"))?;
            println!(
                "archix: applied deployment {generation}'s /var changes: {} written, {} deletions left alone",
                applied.written, applied.whiteouts
            );
            for aside in &applied.moved_aside {
                println!(
                    "archix: moved {} aside, as a type change was in the way",
                    aside.display()
                );
            }
        }
        crate::durable::write(&marker, b"")?;
    }
    let last = state.join("booted-deployment");
    if fs::read_to_string(&last).ok().as_deref().map(str::trim) != Some(generation.to_string().as_str()) {
        // The shared /var/.updated stamp keeps systemd's own catalog update from running after a rollback.
        run_tool(root, "journalctl", &["--update-catalog"]);
        crate::durable::write(&last, format!("{generation}\n").as_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whiteouts_pacman_log_and_deployment_trees_are_left_alone() {
        let rel = Path::new;
        assert_eq!(
            decide(rel("lib/foo"), Kind::Whiteout, Live::Other),
            Action::IgnoreWhiteout
        );
        assert_eq!(
            decide(rel("lib/foo"), Kind::Whiteout, Live::Dir),
            Action::IgnoreWhiteout
        );
        assert_eq!(decide(rel("log/pacman.log"), Kind::File, Live::Other), Action::Keep);
        assert_eq!(decide(rel(".updated"), Kind::File, Live::Missing), Action::Keep);
        assert_eq!(decide(rel("lib/dkms"), Kind::Dir, Live::Dir), Action::Keep);
        assert_eq!(
            decide(rel("lib/dkms/nvidia/kernel-6.0"), Kind::Whiteout, Live::Dir),
            Action::Keep
        );
        assert_eq!(
            decide(rel("lib/pacman/local/foo-1-1/desc"), Kind::File, Live::Other),
            Action::Keep
        );
        assert_eq!(decide(rel("lib/pacman"), Kind::Dir, Live::Dir), Action::UpdateDir);
        assert_eq!(
            decide(rel("log/pacman.log.1"), Kind::File, Live::Other),
            Action::Replace { aside: false }
        );
        assert_eq!(decide(rel("run/foo.sock"), Kind::Socket, Live::Missing), Action::Keep);
    }

    #[test]
    fn package_files_are_created_or_replaced_and_type_changes_move_the_old_aside() {
        let rel = Path::new("lib/x");
        assert_eq!(decide(rel, Kind::Dir, Live::Missing), Action::MakeDir { aside: false });
        assert_eq!(decide(rel, Kind::Dir, Live::Dir), Action::UpdateDir);
        assert_eq!(decide(rel, Kind::Dir, Live::LinkToDir), Action::Through);
        assert_eq!(decide(rel, Kind::Dir, Live::Other), Action::MakeDir { aside: true });
        assert_eq!(decide(rel, Kind::File, Live::Missing), Action::Replace { aside: false });
        assert_eq!(decide(rel, Kind::File, Live::Other), Action::Replace { aside: false });
        assert_eq!(
            decide(rel, Kind::Symlink, Live::LinkToDir),
            Action::Replace { aside: false }
        );
        assert_eq!(decide(rel, Kind::File, Live::Dir), Action::Replace { aside: true });
        assert_eq!(
            decide(rel, Kind::Special, Live::Missing),
            Action::Replace { aside: false }
        );
    }

    #[test]
    fn applying_an_upper_layer_keeps_modes_times_and_live_state() {
        let dir = tempfile::tempdir().unwrap();
        let (upper, var) = (dir.path().join("upper"), dir.path().join("var"));
        for tree in [&upper, &var] {
            fs::create_dir_all(tree.join("log")).unwrap();
            fs::create_dir_all(tree.join("lib/dkms")).unwrap();
        }
        fs::create_dir_all(upper.join("spool/cups/tmp")).unwrap();
        fs::set_permissions(upper.join("spool/cups/tmp"), fs::Permissions::from_mode(0o1770)).unwrap();
        fs::create_dir_all(upper.join("lib/app")).unwrap();
        fs::write(upper.join("lib/app/seed"), "new").unwrap();
        fs::set_permissions(upper.join("lib/app/seed"), fs::Permissions::from_mode(0o640)).unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        fs::File::options()
            .write(true)
            .open(upper.join("lib/app/seed"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        std::os::unix::fs::symlink("../run/lock", upper.join("lock")).unwrap();
        fs::write(upper.join("log/pacman.log"), "only what staging logged\n").unwrap();
        fs::write(upper.join("lib/dkms/stale"), "x").unwrap();
        fs::create_dir_all(var.join("lib/app")).unwrap();
        fs::write(var.join("lib/app/seed"), "old").unwrap();
        fs::write(var.join("lib/app/data"), "user state").unwrap();
        fs::write(var.join("log/pacman.log"), "everything logged live\n").unwrap();
        fs::create_dir_all(var.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink("elsewhere", var.join("spool")).unwrap();

        let applied = apply_upper(&upper, &var).unwrap();

        let mode = |path: &str| fs::symlink_metadata(var.join(path)).unwrap().mode() & 0o7777;
        assert_eq!(fs::read_to_string(var.join("lib/app/seed")).unwrap(), "new");
        assert_eq!(mode("lib/app/seed"), 0o640);
        assert_eq!(fs::metadata(var.join("lib/app/seed")).unwrap().modified().unwrap(), old);
        assert_eq!(fs::read_to_string(var.join("lib/app/data")).unwrap(), "user state");
        assert_eq!(fs::read_link(var.join("lock")).unwrap(), Path::new("../run/lock"));
        assert_eq!(
            fs::read_to_string(var.join("log/pacman.log")).unwrap(),
            "everything logged live\n"
        );
        assert!(
            !var.join("lib/dkms/stale").exists(),
            "DKMS state comes from the deployment"
        );
        assert!(var.join("spool").is_symlink(), "a live symlink to a directory stays");
        assert_eq!(mode("elsewhere/cups/tmp"), 0o1770);
        assert!(applied.kept.contains(&PathBuf::from("log/pacman.log")), "{applied:?}");
        assert!(
            fs::read_dir(var.join("lib/app")).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("archix-new"))
        );
    }

    #[test]
    fn a_type_change_moves_the_live_entry_aside() {
        let dir = tempfile::tempdir().unwrap();
        let (upper, var) = (dir.path().join("upper"), dir.path().join("var"));
        fs::create_dir_all(upper.join("lib/became-dir")).unwrap();
        fs::write(upper.join("lib/became-file"), "file").unwrap();
        fs::create_dir_all(var.join("lib/became-file")).unwrap();
        fs::write(var.join("lib/became-file/state"), "kept").unwrap();
        fs::write(var.join("lib/became-dir"), "was a file").unwrap();
        let applied = apply_upper(&upper, &var).unwrap();
        assert!(var.join("lib/became-dir").is_dir());
        assert_eq!(
            fs::read_to_string(var.join("lib/became-dir.archix-old")).unwrap(),
            "was a file"
        );
        assert_eq!(fs::read_to_string(var.join("lib/became-file")).unwrap(), "file");
        assert_eq!(
            fs::read_to_string(var.join("lib/became-file.archix-old/state")).unwrap(),
            "kept"
        );
        assert_eq!(applied.moved_aside.len(), 2);
    }

    #[test]
    fn two_names_sharing_an_id_stop_activation() {
        let passwd = "root:x:0:0::/root:/bin/bash\nalpm:x:946:946::/:/usr/bin/nologin\n";
        let group = "root:x:0:root\nalpm:x:946:\n";
        check_ids(passwd, group).unwrap();
        let clash = format!("{passwd}cups:x:946:209::/:/usr/bin/nologin\n");
        let err = check_ids(&clash, group).unwrap_err().to_string();
        assert!(err.contains("UID 946 is shared by alpm, cups in /etc/passwd"), "{err}");
        let err = check_ids(passwd, &format!("{group}cups:x:946:\n"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("GID 946 is shared by alpm, cups in /etc/group"), "{err}");
        let err = check_ids(&format!("{passwd}alpm:x:947:947::/:/usr/bin/nologin\n"), group)
            .unwrap_err()
            .to_string();
        assert!(err.contains("alpm is listed more than once"), "{err}");
        check_ids(&format!("# comment\n\n{passwd}+@nis::::::\n"), group).unwrap();
    }
}

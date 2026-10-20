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

/// Paths under /var that activation never writes, nor anything below them; pacman's database and DKMS state are bound from the deployment.
const NEVER: &[&str] = &[
    "log/pacman.log",
    ".updated",
    "lib/archix",
    "log/journal",
    "lib/systemd/random-seed",
    "lib/systemd/timers",
    "lib/pacman/local",
    "lib/dkms",
];

/// The mode and owner a package's mtree gives a directory.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PackageMeta {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

/// Paths under /var, relative to it, that the deployment's packages own, with the mtree's mode and owner for directories.
pub type Owned = BTreeMap<PathBuf, Option<PackageMeta>>;

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

/// How one upper-layer entry is applied: created when missing, replaced only when a package of the deployment owns it, never deleted; otherwise the live copy wins.
pub fn decide(rel: &Path, upper: Kind, live: Live, owned: bool) -> Action {
    if NEVER.iter().any(|never| rel.starts_with(never)) || upper == Kind::Socket {
        return Action::Keep;
    }
    match (upper, live, owned) {
        (Kind::Whiteout, _, _) => Action::IgnoreWhiteout,
        (Kind::Dir, Live::Missing, _) => Action::MakeDir { aside: false },
        (Kind::Dir, Live::Dir, true) => Action::UpdateDir,
        (Kind::Dir, Live::Dir, false) | (Kind::Dir, Live::LinkToDir, _) => Action::Through,
        (Kind::Dir, Live::Other, true) => Action::MakeDir { aside: true },
        (_, Live::Missing, _) => Action::Replace { aside: false },
        (_, Live::Dir, true) => Action::Replace { aside: true },
        (_, _, true) => Action::Replace { aside: false },
        (_, _, false) => Action::Keep,
    }
}

/// What the deployment's packages own under /var; a package whose entry can't be read owns nothing, so the live copy wins.
pub fn package_owned(local: &Path) -> Owned {
    let mut owned = Owned::new();
    for entry in fs::read_dir(local).into_iter().flatten().flatten() {
        let dir = entry.path();
        let files = crate::pacman::parse_desc(&fs::read_to_string(dir.join("files")).unwrap_or_default());
        let mut any = false;
        for path in files.get("FILES").into_iter().flatten() {
            if let Some(rel) = path.strip_prefix("var/").map(|rel| rel.trim_end_matches('/'))
                && !rel.is_empty()
            {
                owned.entry(PathBuf::from(rel)).or_insert(None);
                any = true;
            }
        }
        let mut mtree = String::new();
        if !any
            || fs::File::open(dir.join("mtree"))
                .and_then(|file| io::Read::read_to_string(&mut flate2::read::GzDecoder::new(file), &mut mtree))
                .is_err()
        {
            continue;
        }
        for entry in crate::repack::parse_mtree(&mtree) {
            if entry.kind == crate::repack::Kind::Dir
                && let Some(rel) = entry.path.strip_prefix("var/").map(|rel| rel.trim_end_matches('/'))
                && !rel.is_empty()
            {
                let meta = PackageMeta {
                    mode: entry.mode,
                    uid: entry.uid as u32,
                    gid: entry.gid as u32,
                };
                owned.insert(PathBuf::from(rel), Some(meta));
            }
        }
    }
    owned
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

/// A directory's owner and mode: the package's when its mtree gives them, else the upper layer's.
fn set_dir_meta(meta: &Metadata, to: &Path, from: &Path, package: Option<&PackageMeta>) -> Result<()> {
    let Some(package) = package else {
        return set_meta(meta, to, from);
    };
    std::os::unix::fs::lchown(to, Some(package.uid), Some(package.gid))
        .with_context(|| format!("chown {}", to.display()))?;
    fs::set_permissions(to, fs::Permissions::from_mode(package.mode & 0o7777))
        .with_context(|| format!("chmod {}", to.display()))
}

fn walk(upper: &Path, rel: &Path, var: &Path, owned: &Owned, applied: &mut Applied) -> Result<()> {
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
        let package = owned.get(&rel);
        match decide(&rel, kind, live_kind(&to), package.is_some()) {
            Action::Keep => applied.kept.push(rel),
            Action::IgnoreWhiteout => applied.whiteouts += 1,
            Action::MakeDir { aside } => {
                if aside {
                    applied.moved_aside.push(move_aside(&to)?);
                }
                fs::create_dir(&to).with_context(|| format!("creating {}", to.display()))?;
                set_dir_meta(&meta, &to, &from, package.and_then(Option::as_ref))?;
                applied.written += 1;
                walk(upper, &rel, var, owned, applied)?;
            }
            Action::UpdateDir => {
                set_dir_meta(&meta, &to, &from, package.and_then(Option::as_ref))?;
                walk(upper, &rel, var, owned, applied)?;
            }
            Action::Through => walk(upper, &rel, var, owned, applied)?,
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

/// Applies a deployment's /var upper layer onto the live /var with owners, modes and xattrs, as `decide` rules with `owned` from its packages.
pub fn apply_upper(upper: &Path, var: &Path, owned: &Owned) -> Result<Applied> {
    let mut applied = Applied::default();
    walk(upper, Path::new(""), var, owned, &mut applied)?;
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
    let deployment = root.join("archix/deploy").join(generation.to_string());
    let id = deployment_id(&deployment, generation)?;
    let marker = state.join("activated").join(&id);
    if !marker.exists() {
        let read = |path: &str| fs::read_to_string(root.join(path)).with_context(|| format!("reading /{path}"));
        check_ids(&read("etc/passwd")?, &read("etc/group")?)?;
        run_tool(root, "systemd-sysusers", &[]);
        let upper = deployment.join("var-upper");
        if upper.is_dir() {
            let owned = package_owned(&deployment.join("pacman"));
            let applied = apply_upper(&upper, &root.join("var"), &owned)?;
            println!(
                "archix: applied deployment {generation}'s /var changes: {} written, {} kept as they are live, {} deletions left alone",
                applied.written,
                applied.kept.len(),
                applied.whiteouts
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
    if fs::read_to_string(&last).ok().as_deref().map(str::trim) != Some(id.as_str()) {
        // The shared /var/.updated stamp keeps systemd's own catalog update from running after a rollback.
        run_tool(root, "journalctl", &["--update-catalog"]);
        crate::durable::write(&last, format!("{id}\n").as_bytes())?;
    }
    Ok(())
}

/// Tells deployment `generation` from an earlier one with its number: the directory's inode and birth time, and its package list.
pub fn deployment_id(deployment: &Path, generation: u32) -> Result<String> {
    let meta = fs::metadata(deployment).with_context(|| format!("reading {}", deployment.display()))?;
    // std reads birth times only through glibc's statx; elsewhere the inode and packages have to do.
    let born = meta
        .created()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_nanos());
    let mut packages: Vec<String> = fs::read_dir(deployment.join("pacman"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    packages.sort();
    let digest = crate::fetch::sha256_bytes(packages.join("\n").as_bytes());
    Ok(format!("{generation}-{}-{born}-{}", meta.ino(), &digest[..16]))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn whiteouts_and_what_belongs_to_the_live_system_are_left_alone() {
        let rel = Path::new;
        for owned in [false, true] {
            assert_eq!(
                decide(rel("lib/foo"), Kind::Whiteout, Live::Other, owned),
                Action::IgnoreWhiteout
            );
            assert_eq!(
                decide(rel("lib/foo"), Kind::Whiteout, Live::Dir, owned),
                Action::IgnoreWhiteout
            );
            for never in [
                "log/pacman.log",
                ".updated",
                "lib/archix",
                "lib/archix/booted-deployment",
                "log/journal/0123/system.journal",
                "lib/systemd/random-seed",
                "lib/systemd/timers/stamp-fstrim.timer",
                "lib/dkms",
                "lib/dkms/nvidia/kernel-6.0",
                "lib/pacman/local/foo-1-1/desc",
            ] {
                for live in [Live::Missing, Live::Other, Live::Dir] {
                    assert_eq!(
                        decide(rel(never), Kind::File, live, owned),
                        Action::Keep,
                        "{never} {live:?}"
                    );
                }
            }
        }
        assert_eq!(
            decide(rel("log/pacman.log.1"), Kind::File, Live::Other, true),
            Action::Replace { aside: false }
        );
        assert_eq!(
            decide(rel("run/foo.sock"), Kind::Socket, Live::Missing, false),
            Action::Keep
        );
    }

    #[test]
    fn a_path_both_wrote_is_replaced_only_when_a_package_owns_it() {
        let rel = Path::new("lib/x");
        for owned in [false, true] {
            assert_eq!(
                decide(rel, Kind::Dir, Live::Missing, owned),
                Action::MakeDir { aside: false }
            );
            assert_eq!(
                decide(rel, Kind::File, Live::Missing, owned),
                Action::Replace { aside: false }
            );
            assert_eq!(
                decide(rel, Kind::Special, Live::Missing, owned),
                Action::Replace { aside: false }
            );
            assert_eq!(decide(rel, Kind::Dir, Live::LinkToDir, owned), Action::Through);
        }
        assert_eq!(decide(rel, Kind::Dir, Live::Dir, true), Action::UpdateDir);
        assert_eq!(decide(rel, Kind::Dir, Live::Other, true), Action::MakeDir { aside: true });
        assert_eq!(decide(rel, Kind::File, Live::Other, true), Action::Replace { aside: false });
        assert_eq!(
            decide(rel, Kind::Symlink, Live::LinkToDir, true),
            Action::Replace { aside: false }
        );
        assert_eq!(decide(rel, Kind::File, Live::Dir, true), Action::Replace { aside: true });

        assert_eq!(decide(rel, Kind::Dir, Live::Dir, false), Action::Through);
        assert_eq!(decide(rel, Kind::Dir, Live::Other, false), Action::Keep);
        assert_eq!(decide(rel, Kind::File, Live::Other, false), Action::Keep);
        assert_eq!(decide(rel, Kind::Symlink, Live::LinkToDir, false), Action::Keep);
        assert_eq!(decide(rel, Kind::File, Live::Dir, false), Action::Keep);
    }

    fn owned(paths: &[(&str, Option<u32>)]) -> Owned {
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        paths
            .iter()
            .map(|(path, mode)| (PathBuf::from(path), mode.map(|mode| PackageMeta { mode, uid, gid })))
            .collect()
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
        fs::create_dir_all(upper.join("lib/app")).unwrap();
        fs::set_permissions(upper.join("lib/app"), fs::Permissions::from_mode(0o750)).unwrap();
        fs::write(upper.join("lib/app/seed"), "new").unwrap();
        fs::set_permissions(upper.join("lib/app/seed"), fs::Permissions::from_mode(0o640)).unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        fs::File::options()
            .write(true)
            .open(upper.join("lib/app/seed"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        fs::write(upper.join("lib/app/data"), "what staging wrote").unwrap();
        fs::write(upper.join("lib/app/created"), "only staging wrote this").unwrap();
        std::os::unix::fs::symlink("../run/lock", upper.join("lock")).unwrap();
        fs::write(upper.join("log/pacman.log"), "only what staging logged\n").unwrap();
        fs::write(upper.join("lib/dkms/stale"), "x").unwrap();
        fs::create_dir_all(var.join("lib/app")).unwrap();
        fs::set_permissions(var.join("lib/app"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(var.join("lib/app/seed"), "old").unwrap();
        fs::write(var.join("lib/app/data"), "user state").unwrap();
        fs::write(var.join("log/pacman.log"), "everything logged live\n").unwrap();
        fs::create_dir_all(var.join("elsewhere/cups/tmp")).unwrap();
        fs::set_permissions(var.join("elsewhere/cups/tmp"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("elsewhere", var.join("spool")).unwrap();
        let owned = owned(&[
            ("lib/app", None),
            ("lib/app/seed", None),
            ("spool/cups/tmp", Some(0o1770)),
        ]);

        let applied = apply_upper(&upper, &var, &owned).unwrap();

        let mode = |path: &str| fs::symlink_metadata(var.join(path)).unwrap().mode() & 0o7777;
        assert_eq!(fs::read_to_string(var.join("lib/app/seed")).unwrap(), "new");
        assert_eq!(mode("lib/app/seed"), 0o640);
        assert_eq!(fs::metadata(var.join("lib/app/seed")).unwrap().modified().unwrap(), old);
        assert_eq!(
            fs::read_to_string(var.join("lib/app/data")).unwrap(),
            "user state",
            "a file both wrote that no package owns stays the live one"
        );
        assert_eq!(
            fs::read_to_string(var.join("lib/app/created")).unwrap(),
            "only staging wrote this"
        );
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
        assert_eq!(mode("elsewhere/cups/tmp"), 0o1770, "a package's directory gets its mtree mode");
        assert_eq!(mode("lib/app"), 0o750, "an owned directory without an mtree entry gets the upper layer's");
        assert!(applied.kept.contains(&PathBuf::from("log/pacman.log")), "{applied:?}");
        assert!(applied.kept.contains(&PathBuf::from("lib/app/data")), "{applied:?}");
        assert!(
            fs::read_dir(var.join("lib/app")).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("archix-new"))
        );
    }

    #[test]
    fn a_live_directory_no_package_owns_keeps_its_mode() {
        let dir = tempfile::tempdir().unwrap();
        let (upper, var) = (dir.path().join("upper"), dir.path().join("var"));
        fs::create_dir_all(upper.join("lib/mine")).unwrap();
        fs::set_permissions(upper.join("lib/mine"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::create_dir_all(var.join("lib/mine")).unwrap();
        fs::set_permissions(var.join("lib/mine"), fs::Permissions::from_mode(0o700)).unwrap();
        apply_upper(&upper, &var, &Owned::new()).unwrap();
        assert_eq!(fs::metadata(var.join("lib/mine")).unwrap().mode() & 0o7777, 0o700);
    }

    #[test]
    fn a_type_change_of_a_package_path_moves_the_live_entry_aside() {
        let dir = tempfile::tempdir().unwrap();
        let (upper, var) = (dir.path().join("upper"), dir.path().join("var"));
        fs::create_dir_all(upper.join("lib/became-dir")).unwrap();
        fs::write(upper.join("lib/became-file"), "file").unwrap();
        fs::create_dir_all(var.join("lib/became-file")).unwrap();
        fs::write(var.join("lib/became-file/state"), "kept").unwrap();
        fs::write(var.join("lib/became-dir"), "was a file").unwrap();
        let owned = owned(&[("lib/became-dir", None), ("lib/became-file", None)]);
        let applied = apply_upper(&upper, &var, &owned).unwrap();
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
    fn package_owned_paths_come_from_the_deployments_database() {
        let dir = tempfile::tempdir().unwrap();
        let entry = dir.path().join("cups-2.4-1");
        fs::create_dir_all(&entry).unwrap();
        fs::write(
            entry.join("files"),
            "%FILES%\nusr/\nusr/bin/lp\nvar/\nvar/spool/\nvar/spool/cups/\nvar/spool/cups/tmp/\nvar/lib/cups/seed\n\n",
        )
        .unwrap();
        let mtree = "#mtree\n/set type=file uid=0 gid=0 mode=644\n./.PKGINFO time=1.0 size=1\n\
                     ./var time=1.0 mode=755 type=dir\n./var/spool/cups time=1.0 mode=710 gid=209 type=dir\n\
                     ./var/spool/cups/tmp time=1.0 mode=1770 gid=209 type=dir\n./var/lib/cups/seed time=1.0 size=3\n";
        let file = fs::File::create(entry.join("mtree")).unwrap();
        let mut gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        gz.write_all(mtree.as_bytes()).unwrap();
        gz.finish().unwrap();
        fs::create_dir_all(dir.path().join("broken-1-1")).unwrap();
        fs::write(dir.path().join("ALPM_DB_VERSION"), "9\n").unwrap();

        let meta = |mode, gid| Some(PackageMeta { mode, uid: 0, gid });
        assert_eq!(
            package_owned(dir.path()),
            Owned::from([
                (PathBuf::from("lib/cups/seed"), None),
                (PathBuf::from("spool"), None),
                (PathBuf::from("spool/cups"), meta(0o710, 209)),
                (PathBuf::from("spool/cups/tmp"), meta(0o1770, 209)),
            ])
        );
    }

    #[test]
    fn a_rebuilt_deployment_has_a_new_identity() {
        let dir = tempfile::tempdir().unwrap();
        let deployment = dir.path().join("deploy/2");
        fs::create_dir_all(deployment.join("pacman/glibc-2.42-1")).unwrap();
        let first = deployment_id(&deployment, 2).unwrap();
        assert!(first.starts_with("2-"), "{first}");
        assert_eq!(deployment_id(&deployment, 2).unwrap(), first);
        fs::rename(&deployment, dir.path().join("deploy/2.old")).unwrap();
        fs::create_dir_all(deployment.join("pacman/glibc-2.42-1")).unwrap();
        assert_ne!(
            deployment_id(&deployment, 2).unwrap(),
            first,
            "the same number and packages, rebuilt"
        );
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

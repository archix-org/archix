use std::ffi::{CStr, CString};
use std::fs::{self, File, Metadata};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::deploy::xattr;

/// What an entry under /etc is, by content for files and target for symlinks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File(String),
    Dir,
    Symlink(String),
    Other,
}

/// Everything the merge compares about one entry; times are left out, as a rewrite with the same content isn't a change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fp {
    pub kind: Kind,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub xattrs: String,
}

impl Fp {
    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }
}

pub fn describe(fp: Option<&Fp>) -> &'static str {
    match fp.map(|fp| &fp.kind) {
        None => "nothing",
        Some(Kind::File(_)) => "a file",
        Some(Kind::Dir) => "a directory",
        Some(Kind::Symlink(_)) => "a symlink",
        Some(Kind::Other) => "a socket, FIFO or device",
    }
}

fn c_path(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).with_context(|| format!("{} has a NUL byte", path.display()))
}

fn kept_xattr(name: &CStr) -> bool {
    !xattr::is_overlay_private(name)
}

fn xattr_names(path: &Path) -> Result<Vec<CString>> {
    match xattr::list(path) {
        Ok(names) => Ok(names.into_iter().filter(|name| kept_xattr(name)).collect()),
        Err(e) if e.raw_os_error() == Some(libc::ENOTSUP) => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("listing the xattrs of {}", path.display())),
    }
}

/// The xattrs of `path`, ACLs and capabilities included, as one digest.
fn xattr_digest(path: &Path) -> Result<String> {
    let mut names = xattr_names(path)?;
    if names.is_empty() {
        return Ok(String::new());
    }
    names.sort();
    let mut hasher = Sha256::new();
    for name in names {
        let value = xattr::get(path, &name)
            .with_context(|| format!("reading an xattr of {}", path.display()))?
            .unwrap_or_default();
        hasher.update(name.as_bytes_with_nul());
        hasher.update((value.len() as u64).to_le_bytes());
        hasher.update(&value);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn open_nofollow(path: &Path) -> io::Result<File> {
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

fn hash_reader(mut reader: impl Read) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0; 1 << 16];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            return Ok(hex(&hasher.finalize()));
        }
        hasher.update(&buf[..n]);
    }
}

fn link_text(path: &Path) -> io::Result<String> {
    Ok(String::from_utf8_lossy(fs::read_link(path)?.as_os_str().as_bytes()).into_owned())
}

fn fp_of(path: &Path, meta: &Metadata, kind: Kind) -> Result<Fp> {
    let symlink = matches!(kind, Kind::Symlink(_));
    Ok(Fp {
        kind,
        mode: if symlink { 0 } else { meta.mode() & 0o7777 },
        uid: meta.uid(),
        gid: meta.gid(),
        xattrs: xattr_digest(path)?,
    })
}

/// The fingerprint of what's at `path` itself, None when nothing is; symlinks are never followed.
pub fn fingerprint(path: &Path) -> Result<Option<Fp>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let file_type = meta.file_type();
    let kind = if file_type.is_dir() {
        Kind::Dir
    } else if file_type.is_symlink() {
        match link_text(path) {
            Ok(target) => Kind::Symlink(target),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        }
    } else if file_type.is_file() {
        match open_nofollow(path).and_then(hash_reader) {
            Ok(sha) => Kind::File(sha),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        }
    } else {
        Kind::Other
    };
    fp_of(path, &meta, kind).map(Some)
}

/// The first ancestor of `rel` under `top` that exists but isn't a real directory, through which `rel` must never be reached.
pub fn blocked_ancestor(top: &Path, rel: &str) -> Option<String> {
    let mut at = String::new();
    let parts: Vec<&str> = rel.split('/').collect();
    for part in &parts[..parts.len().saturating_sub(1)] {
        if !at.is_empty() {
            at.push('/');
        }
        at.push_str(part);
        match fs::symlink_metadata(top.join(&at)) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Some(at),
            Err(_) => return None,
        }
    }
    None
}

pub fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// The temporary name a write into `path` goes through; a leftover is swept at the next boot.
pub fn temp_for(path: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    name.push(TEMP_SUFFIX);
    path.with_file_name(name)
}

pub const TEMP_SUFFIX: &str = ".archix-merge";

fn remove_any(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display())),
        Ok(_) => fs::remove_file(path).with_context(|| format!("removing {}", path.display())),
        Err(_) => Ok(()),
    }
}

fn sync_parent(path: &Path) -> Result<()> {
    crate::durable::sync_dir(path).with_context(|| format!("flushing the directory of {}", path.display()))
}

/// Copies `from`'s xattrs onto `to` and drops those `from` lacks, after owner and mode, so ACLs and capabilities come out as on `from`.
fn set_xattrs(from: &Path, to: &Path) -> Result<()> {
    let wanted = xattr_names(from)?;
    for name in xattr_names(to)? {
        if !wanted.contains(&name) {
            xattr::remove(to, &name).with_context(|| format!("removing an xattr of {}", to.display()))?;
        }
    }
    for name in wanted {
        let Some(value) = xattr::get(from, &name).with_context(|| format!("reading an xattr of {}", from.display()))?
        else {
            continue;
        };
        xattr::set(to, &name, &value)
            .with_context(|| format!("setting {} on {}", name.to_string_lossy(), to.display()))?;
    }
    Ok(())
}

fn set_times(meta: &Metadata, path: &Path) -> Result<()> {
    let c = c_path(path)?;
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
    if unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(io::Error::last_os_error()).with_context(|| format!("setting the times of {}", path.display()));
    }
    Ok(())
}

/// Owner first, as chown clears setuid bits and capabilities, then mode, then xattrs, whose ACL agrees with that mode.
fn set_meta(meta: &Metadata, from: &Path, to: &Path) -> Result<()> {
    std::os::unix::fs::lchown(to, Some(meta.uid()), Some(meta.gid()))
        .with_context(|| format!("chown {}", to.display()))?;
    if !meta.file_type().is_symlink() {
        fs::set_permissions(to, fs::Permissions::from_mode(meta.mode() & 0o7777))
            .with_context(|| format!("chmod {}", to.display()))?;
    }
    set_xattrs(from, to)
}

/// Copies the file `from` to `to` through a 0600 temporary file, fsynced and renamed over, with owner, mode, xattrs and times; returns what was written.
pub fn copy_file(from: &Path, to: &Path) -> Result<Fp> {
    let src = open_nofollow(from).with_context(|| format!("opening {}", from.display()))?;
    let meta = src.metadata()?;
    if !meta.is_file() {
        bail!("{} stopped being a file while it was copied", from.display());
    }
    write_with(to, &meta, from, |dst| {
        let mut hasher = Sha256::new();
        let mut reader = &src;
        let mut buf = vec![0; 1 << 16];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            dst.write_all(&buf[..n])?;
        }
        Ok(hex(&hasher.finalize()))
    })
}

/// Writes `contents` to `to` like `copy_file`, with the owner, mode and xattrs of `like`.
pub fn write_like(to: &Path, contents: &[u8], like: &Path) -> Result<()> {
    let meta = fs::metadata(like).with_context(|| format!("reading {}", like.display()))?;
    write_with(to, &meta, like, |dst| {
        dst.write_all(contents)?;
        Ok(String::new())
    })?;
    Ok(())
}

fn write_with(
    to: &Path,
    meta: &Metadata,
    from: &Path,
    fill: impl FnOnce(&mut File) -> io::Result<String>,
) -> Result<Fp> {
    let tmp = temp_for(to);
    remove_any(&tmp)?;
    let result = (|| -> Result<Fp> {
        let mut dst = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        let sha = fill(&mut dst).with_context(|| format!("writing {}", tmp.display()))?;
        set_meta(meta, from, &tmp)?;
        set_times(meta, &tmp)?;
        dst.sync_all().with_context(|| format!("flushing {}", tmp.display()))?;
        let fp = fp_of(&tmp, meta, Kind::File(sha))?;
        crate::testhooks::fail_at(&format!("etc-merge-rename:{}", to.display()))?;
        fs::rename(&tmp, to).with_context(|| format!("renaming {} to {}", tmp.display(), to.display()))?;
        Ok(fp)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    let fp = result?;
    sync_parent(to)?;
    Ok(fp)
}

/// Recreates the symlink `from` at `to`, target untouched wherever it points, renamed over atomically.
pub fn copy_symlink(from: &Path, to: &Path) -> Result<Fp> {
    let meta = fs::symlink_metadata(from).with_context(|| format!("reading {}", from.display()))?;
    let target = fs::read_link(from).with_context(|| format!("reading {}", from.display()))?;
    let tmp = temp_for(to);
    remove_any(&tmp)?;
    let result = (|| -> Result<()> {
        std::os::unix::fs::symlink(&target, &tmp).with_context(|| format!("creating {}", tmp.display()))?;
        set_meta(&meta, from, &tmp)?;
        set_times(&meta, &tmp)?;
        fs::rename(&tmp, to).with_context(|| format!("renaming {} to {}", tmp.display(), to.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result?;
    sync_parent(to)?;
    fp_of(
        to,
        &meta,
        Kind::Symlink(String::from_utf8_lossy(target.as_os_str().as_bytes()).into_owned()),
    )
}

/// Gives the directory `to` the owner, mode and xattrs of `from`, creating it under a temporary name first when it's missing.
pub fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(from).with_context(|| format!("reading {}", from.display()))?;
    if fs::symlink_metadata(to).is_ok_and(|have| have.is_dir()) {
        set_meta(&meta, from, to)?;
        return sync_parent(to);
    }
    let tmp = temp_for(to);
    remove_any(&tmp)?;
    let result = (|| -> Result<()> {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        set_meta(&meta, from, &tmp)?;
        fs::rename(&tmp, to).with_context(|| format!("renaming {} to {}", tmp.display(), to.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_dir(&tmp);
    }
    result?;
    sync_parent(to)
}

/// Moves what's at `path` to `<path>.pacnew`, replacing an older one.
pub fn to_pacnew(path: &Path) -> Result<PathBuf> {
    let pacnew = with_suffix(path, ".pacnew");
    remove_any(&pacnew)?;
    fs::rename(path, &pacnew).with_context(|| format!("renaming {} to {}", path.display(), pacnew.display()))?;
    sync_parent(path)?;
    Ok(pacnew)
}

/// Removes the file or symlink at `path`.
pub fn remove_entry(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e).with_context(|| format!("removing {}", path.display())),
        _ => sync_parent(path),
    }
}

/// Every entry under `top`/`rel`, relative to `top`, not following symlinks or entering what `skip` names.
pub fn walk(top: &Path, rel: &str, skip: &dyn Fn(&str) -> bool, out: &mut Vec<String>) {
    let dir = if rel.is_empty() {
        top.to_path_buf()
    } else {
        top.join(rel)
    };
    let Ok(entries) = fs::read_dir(&dir) else { return };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            eprintln!(
                "archix: /etc merge: skipping {}, whose name isn't UTF-8",
                entry.path().display()
            );
            continue;
        };
        let child = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        if skip(&child) {
            continue;
        }
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        out.push(child.clone());
        if is_dir {
            walk(top, &child, skip, out);
        }
    }
}

/// Deletes temporary files an interrupted write left under `top`.
pub fn sweep_temps(top: &Path) {
    let mut all = Vec::new();
    walk(top, "", &|_| false, &mut all);
    for rel in all.iter().filter(|rel| rel.ends_with(TEMP_SUFFIX)) {
        let _ = remove_any(&top.join(rel));
    }
}

pub fn is_special(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| {
        let t = meta.file_type();
        t.is_socket() || t.is_fifo() || t.is_block_device() || t.is_char_device()
    })
}

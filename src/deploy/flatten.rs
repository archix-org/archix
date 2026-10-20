use std::collections::BTreeSet;
use std::ffi::{CString, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result};

use super::xattr;

/// What an overlay's upper layer has at a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upper {
    Missing,
    Whiteout,
    Dir { opaque: bool },
    Other,
}

/// What the lower layer has at a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lower {
    Missing,
    Dir,
    Other,
}

/// Where the flattened tree's entry at a name comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Take {
    Nothing,
    Upper,
    Lower,
    Dir { upper: bool, lower: bool },
}

/// overlayfs's merge rule for one name, with redirects and metacopy off.
pub fn take(upper: Upper, lower: Lower) -> Take {
    match (upper, lower) {
        (Upper::Whiteout, _) | (Upper::Missing, Lower::Missing) => Take::Nothing,
        (Upper::Other, _) => Take::Upper,
        (Upper::Dir { opaque }, lower) => Take::Dir {
            upper: true,
            lower: !opaque && lower == Lower::Dir,
        },
        (Upper::Missing, Lower::Dir) => Take::Dir {
            upper: false,
            lower: true,
        },
        (Upper::Missing, Lower::Other) => Take::Lower,
    }
}

/// Builds `out` as the merged view of an unmounted overlay: upper files are moved in, unchanged lower files hardlinked, whiteouts dropped; no file data is copied.
pub fn flatten(lower: &Path, upper: &Path, out: &Path) -> Result<()> {
    // The root takes the lower's owner and mode but the upper's times, which the transaction's `touch -c /usr` set.
    build(lower, upper, out, true, true, lower, upper)
}

fn build(
    lower: &Path,
    upper: &Path,
    out: &Path,
    with_upper: bool,
    with_lower: bool,
    meta: &Path,
    times: &Path,
) -> Result<()> {
    fs::create_dir(out).with_context(|| format!("creating {}", out.display()))?;
    // Moving children out of an upper directory changes its times, so they're read first.
    let stamp = fs::symlink_metadata(times).with_context(|| format!("stat {}", times.display()))?;
    let mut names: BTreeSet<OsString> = BTreeSet::new();
    for (wanted, dir) in [(with_upper, upper), (with_lower, lower)] {
        if wanted {
            for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
                names.insert(entry?.file_name());
            }
        }
    }
    for name in names {
        let (up, lo, dst) = (upper.join(&name), lower.join(&name), out.join(&name));
        let upper_kind = if with_upper { upper_kind(&up)? } else { Upper::Missing };
        let lower_kind = if with_lower { lower_kind(&lo)? } else { Lower::Missing };
        match take(upper_kind, lower_kind) {
            Take::Nothing => {}
            Take::Upper => {
                fs::rename(&up, &dst).with_context(|| format!("moving {} to {}", up.display(), dst.display()))?;
                strip_overlay_xattrs(&dst)?;
            }
            Take::Lower => link(&lo, &dst)?,
            Take::Dir { upper: u, lower: l } => {
                let source = if u { &up } else { &lo };
                build(&lo, &up, &dst, u, l, source, source)?;
            }
        }
    }
    copy_dir_meta(meta, &stamp, out)
}

fn lstat(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("stat {}", path.display())),
    }
}

fn upper_kind(path: &Path) -> Result<Upper> {
    let Some(meta) = lstat(path)? else {
        return Ok(Upper::Missing);
    };
    let kind = meta.file_type();
    Ok(if kind.is_char_device() && meta.rdev() == 0 {
        Upper::Whiteout
    } else if kind.is_dir() {
        let opaque = xattr::get(path, c"trusted.overlay.opaque")
            .with_context(|| format!("reading {}'s xattrs", path.display()))?;
        Upper::Dir {
            opaque: opaque.as_deref() == Some(b"y"),
        }
    } else {
        Upper::Other
    })
}

fn lower_kind(path: &Path) -> Result<Lower> {
    Ok(match lstat(path)? {
        None => Lower::Missing,
        Some(meta) if meta.is_dir() => Lower::Dir,
        Some(_) => Lower::Other,
    })
}

fn c_path(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).with_context(|| format!("{} has a NUL byte", path.display()))
}

/// A hardlink to `from` itself, never to what a symlink points at.
fn link(from: &Path, to: &Path) -> Result<()> {
    let (c_from, c_to) = (c_path(from)?, c_path(to)?);
    if unsafe { libc::linkat(libc::AT_FDCWD, c_from.as_ptr(), libc::AT_FDCWD, c_to.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("linking {} to {}", to.display(), from.display()));
    }
    Ok(())
}

fn strip_overlay_xattrs(path: &Path) -> Result<()> {
    for name in xattr::list(path).with_context(|| format!("listing {}'s xattrs", path.display()))? {
        if xattr::is_overlay_private(&name) {
            xattr::remove(path, &name).with_context(|| format!("removing {}'s xattrs", path.display()))?;
        }
    }
    Ok(())
}

fn copy_dir_meta(meta: &Path, times: &fs::Metadata, dir: &Path) -> Result<()> {
    let from = fs::symlink_metadata(meta).with_context(|| format!("stat {}", meta.display()))?;
    std::os::unix::fs::lchown(dir, Some(from.uid()), Some(from.gid()))
        .with_context(|| format!("chown {}", dir.display()))?;
    for name in xattr::list(meta).with_context(|| format!("listing {}'s xattrs", meta.display()))? {
        if xattr::is_overlay_private(&name) {
            continue;
        }
        if let Some(value) = xattr::get(meta, &name)? {
            xattr::set(dir, &name, &value).with_context(|| format!("setting {}'s xattrs", dir.display()))?;
        }
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(from.mode() & 0o7777))
        .with_context(|| format!("chmod {}", dir.display()))?;
    set_times(dir, times)
}

/// Gives `path` the access and modification times of `like`.
pub fn set_times(path: &Path, like: &fs::Metadata) -> Result<()> {
    let spec = |sec: i64, nsec: i64| libc::timespec {
        tv_sec: sec as libc::time_t,
        tv_nsec: nsec as _,
    };
    let times = [
        spec(like.atime(), like.atime_nsec()),
        spec(like.mtime(), like.mtime_nsec()),
    ];
    let c = c_path(path)?;
    if unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return Err(io::Error::last_os_error()).with_context(|| format!("setting {}'s times", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whiteouts_hide_and_opaque_directories_drop_the_lower_one() {
        assert_eq!(take(Upper::Whiteout, Lower::Other), Take::Nothing);
        assert_eq!(take(Upper::Whiteout, Lower::Dir), Take::Nothing);
        assert_eq!(take(Upper::Whiteout, Lower::Missing), Take::Nothing);
        assert_eq!(
            take(Upper::Dir { opaque: true }, Lower::Dir),
            Take::Dir {
                upper: true,
                lower: false
            }
        );
        assert_eq!(
            take(Upper::Dir { opaque: false }, Lower::Dir),
            Take::Dir {
                upper: true,
                lower: true
            }
        );
    }

    #[test]
    fn upper_entries_hide_lower_ones_of_another_type() {
        assert_eq!(take(Upper::Other, Lower::Dir), Take::Upper);
        assert_eq!(take(Upper::Other, Lower::Other), Take::Upper);
        assert_eq!(
            take(Upper::Dir { opaque: false }, Lower::Other),
            Take::Dir {
                upper: true,
                lower: false
            }
        );
        assert_eq!(take(Upper::Missing, Lower::Other), Take::Lower);
        assert_eq!(
            take(Upper::Missing, Lower::Dir),
            Take::Dir {
                upper: false,
                lower: true
            }
        );
        assert_eq!(take(Upper::Missing, Lower::Missing), Take::Nothing);
    }

    #[test]
    fn upper_files_move_in_and_unchanged_lower_files_are_hardlinked() {
        let dir = tempfile::tempdir().unwrap();
        let (lower, upper, out) = (
            dir.path().join("lower"),
            dir.path().join("upper"),
            dir.path().join("out"),
        );
        for sub in ["lib/kept", "share", "old"] {
            fs::create_dir_all(lower.join(sub)).unwrap();
        }
        fs::write(lower.join("lib/kept/a"), "a").unwrap();
        fs::write(lower.join("lib/changed"), "old").unwrap();
        std::os::unix::fs::symlink("kept/a", lower.join("lib/link")).unwrap();
        fs::write(lower.join("old/x"), "x").unwrap();
        fs::create_dir_all(upper.join("lib")).unwrap();
        fs::create_dir_all(upper.join("new/deeper")).unwrap();
        fs::write(upper.join("lib/changed"), "new").unwrap();
        fs::write(upper.join("new/deeper/b"), "b").unwrap();
        fs::write(upper.join("top"), "top").unwrap();
        fs::set_permissions(upper.join("new"), fs::Permissions::from_mode(0o750)).unwrap();
        let upper_changed = fs::metadata(upper.join("lib/changed")).unwrap().ino();
        let stamp = fs::metadata(lower.join("old")).unwrap();
        set_times(&upper, &stamp).unwrap();
        fs::set_permissions(&lower, fs::Permissions::from_mode(0o751)).unwrap();

        flatten(&lower, &upper, &out).unwrap();

        let ino = |path: &Path| fs::symlink_metadata(path).unwrap().ino();
        assert_eq!(ino(&out.join("lib/kept/a")), ino(&lower.join("lib/kept/a")));
        assert_eq!(ino(&out.join("lib/link")), ino(&lower.join("lib/link")));
        assert_eq!(fs::read_link(out.join("lib/link")).unwrap(), Path::new("kept/a"));
        assert_eq!(ino(&out.join("lib/changed")), upper_changed);
        assert_eq!(fs::read_to_string(out.join("lib/changed")).unwrap(), "new");
        assert_eq!(fs::read_to_string(lower.join("lib/changed")).unwrap(), "old");
        assert_eq!(fs::read_to_string(out.join("new/deeper/b")).unwrap(), "b");
        assert_eq!(fs::read_to_string(out.join("top")).unwrap(), "top");
        assert_eq!(ino(&out.join("old/x")), ino(&lower.join("old/x")));
        assert_ne!(ino(&out.join("lib")), ino(&lower.join("lib")));
        assert_eq!(fs::metadata(out.join("new")).unwrap().mode() & 0o7777, 0o750);
        let root = fs::metadata(&out).unwrap();
        assert_eq!(root.mode() & 0o7777, 0o751);
        assert_eq!(root.mtime(), stamp.mtime());
        assert_eq!(root.mtime_nsec(), stamp.mtime_nsec());
    }
}

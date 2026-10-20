use std::ffi::{CStr, CString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

/// The names of `path`'s extended attributes, not following a symlink.
pub fn list(path: &Path) -> io::Result<Vec<CString>> {
    let c = c_path(path)?;
    loop {
        let size = unsafe { libc::llistxattr(c.as_ptr(), std::ptr::null_mut(), 0) };
        if size < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buf = vec![0u8; size as usize];
        let got = unsafe { libc::llistxattr(c.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        if got < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ERANGE) {
                continue;
            }
            return Err(err);
        }
        buf.truncate(got as usize);
        return Ok(buf
            .split(|b| *b == 0)
            .filter(|name| !name.is_empty())
            .map(|name| CString::new(name).expect("split on NUL"))
            .collect());
    }
}

/// One extended attribute's value, or None when `path` doesn't have it.
pub fn get(path: &Path, name: &CStr) -> io::Result<Option<Vec<u8>>> {
    let c = c_path(path)?;
    loop {
        let size = unsafe { libc::lgetxattr(c.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) };
        if size < 0 {
            let err = io::Error::last_os_error();
            return if err.raw_os_error() == Some(libc::ENODATA) {
                Ok(None)
            } else {
                Err(err)
            };
        }
        let mut buf = vec![0u8; size as usize];
        let got = unsafe { libc::lgetxattr(c.as_ptr(), name.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        if got < 0 {
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::ERANGE) => continue,
                Some(libc::ENODATA) => return Ok(None),
                _ => return Err(err),
            }
        }
        buf.truncate(got as usize);
        return Ok(Some(buf));
    }
}

pub fn set(path: &Path, name: &CStr, value: &[u8]) -> io::Result<()> {
    let c = c_path(path)?;
    let done = unsafe { libc::lsetxattr(c.as_ptr(), name.as_ptr(), value.as_ptr().cast(), value.len(), 0) };
    if done != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn remove(path: &Path, name: &CStr) -> io::Result<()> {
    let c = c_path(path)?;
    if unsafe { libc::lremovexattr(c.as_ptr(), name.as_ptr()) } != 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ENODATA) {
            return Err(err);
        }
    }
    Ok(())
}

/// overlayfs's own bookkeeping, which means nothing once a tree is no longer an upper layer.
pub fn is_overlay_private(name: &CStr) -> bool {
    name.to_bytes().starts_with(b"trusted.overlay.")
}

//! Where plaintext lives while archix holds it: pages of their own, locked out of swap, left out of core dumps and of forked children, and zeroed before they're unmapped.

use std::io::Read;

use anyhow::{Result, bail};

/// The most a single secret may hold.
pub const MAX_SECRET: usize = 1 << 20;
/// The most all of a deployment's secrets together may hold, as ramfs has no size limit.
pub const MAX_TOTAL: usize = 16 << 20;

/// A buffer of plaintext in an anonymous mapping of its own.
pub struct SecretBuf {
    ptr: *mut u8,
    len: usize,
    cap: usize,
}

unsafe impl Send for SecretBuf {}
unsafe impl Sync for SecretBuf {}

fn page() -> usize {
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size > 0 { size as usize } else { 4096 }
}

impl SecretBuf {
    /// Room for `cap` bytes, which `push` never grows past.
    pub fn with_capacity(cap: usize) -> Result<SecretBuf> {
        let size = cap.max(1).div_ceil(page()) * page();
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error().into());
        }
        unsafe {
            libc::madvise(ptr, size, libc::MADV_DONTDUMP);
            libc::madvise(ptr, size, libc::MADV_DONTFORK);
        }
        if unsafe { libc::mlock(ptr, size) } != 0 && unsafe { libc::geteuid() } == 0 {
            let error = std::io::Error::last_os_error();
            unsafe { libc::munmap(ptr, size) };
            bail!("locking memory for a secret failed: {error}");
        }
        Ok(SecretBuf {
            ptr: ptr.cast(),
            len: 0,
            cap: size,
        })
    }

    /// A copy of `bytes`.
    pub fn from_slice(bytes: &[u8]) -> Result<SecretBuf> {
        let mut buf = SecretBuf::with_capacity(bytes.len())?;
        buf.push(bytes)?;
        Ok(buf)
    }

    /// Everything `reader` gives, failing past `limit` bytes; `what` names the secret.
    pub fn read_all(mut reader: impl Read, limit: usize, what: &str) -> Result<SecretBuf> {
        let mut buf = SecretBuf::with_capacity(limit + 1)?;
        loop {
            let spare = unsafe { std::slice::from_raw_parts_mut(buf.ptr.add(buf.len), buf.cap - buf.len) };
            if spare.is_empty() {
                break;
            }
            match reader.read(spare) {
                Ok(0) => break,
                Ok(n) => buf.len += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => bail!("decrypting {what}: {}", describe_io(&e)),
            }
        }
        if buf.len > limit {
            bail!("{what} holds more than {} KiB, the most a secret may", limit >> 10);
        }
        Ok(buf)
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<()> {
        if self.len + bytes.len() > self.cap {
            bail!("a secret grew past the room it was given");
        }
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr.add(self.len), bytes.len()) };
        self.len += bytes.len();
        Ok(())
    }

    /// Locks its pages again, as after a `LockAll` that unlocked everything.
    pub fn relock(&self) {
        unsafe { libc::mlock(self.ptr.cast(), self.cap) };
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Drops trailing line breaks, as a hash piped from mkpasswd ends with one.
    pub fn trim_end_newlines(&mut self) {
        while self.len > 0 && matches!(self.as_slice()[self.len - 1], b'\n' | b'\r') {
            self.len -= 1;
            unsafe { std::ptr::write_volatile(self.ptr.add(self.len), 0) };
        }
    }
}

impl Drop for SecretBuf {
    fn drop(&mut self) {
        for i in 0..self.cap {
            unsafe { std::ptr::write_volatile(self.ptr.add(i), 0) };
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
        unsafe {
            libc::munlock(self.ptr.cast(), self.cap);
            libc::munmap(self.ptr.cast(), self.cap);
        }
    }
}

/// An age error without the bytes it was reading.
fn describe_io(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::InvalidData => "the ciphertext is damaged (its authentication failed)".to_string(),
        std::io::ErrorKind::UnexpectedEof => "the ciphertext is cut short".to_string(),
        _ => error.to_string(),
    }
}

/// Keeps this process's memory out of core dumps and swap and its plaintext from ptrace, for a process that holds secrets: no dumps, no core files, and with `lock_all` every page locked, now and later.
pub fn harden(lock_all: bool) -> Result<()> {
    unsafe {
        if libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let none = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::setrlimit(libc::RLIMIT_CORE, &none) != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if lock_all && libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

/// Locks every page of this process, now and later, while it lives, so buffers archix doesn't own (age's) can't be swapped out; dropping it unlocks them, after which each SecretBuf kept needs `relock`.
pub struct LockAll(());

/// How many LockAll guards live, so only the last one dropped unlocks.
static LOCKS: std::sync::Mutex<usize> = std::sync::Mutex::new(0);

impl LockAll {
    pub fn new() -> Result<LockAll> {
        let mut held = LOCKS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if *held == 0 && unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) } != 0 {
            bail!("locking this process's memory: {}", std::io::Error::last_os_error());
        }
        *held += 1;
        Ok(LockAll(()))
    }
}

impl Drop for LockAll {
    fn drop(&mut self) {
        let mut held = LOCKS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *held -= 1;
        if *held == 0 {
            unsafe { libc::munlockall() };
        }
    }
}

/// How much of this process's memory is locked, in kB, from /proc/self/status.
pub fn locked_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("VmLck:"))
                .and_then(|rest| rest.split_whitespace().next()?.parse().ok())
        })
        .unwrap_or(0)
}

/// Whether `needle` occurs in `haystack`.
pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    find(haystack, needle).is_some()
}

/// Where `needle` first occurs in `haystack`.
pub fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let (&first, rest) = needle.split_first()?;
    let mut at = 0;
    while at + needle.len() <= haystack.len() {
        let offset = haystack[at..=haystack.len() - needle.len()]
            .iter()
            .position(|&b| b == first)?;
        let start = at + offset;
        if &haystack[start + 1..start + needle.len()] == rest {
            return Some(start);
        }
        at = start + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Locked: kB of the mapping in /proc/self/smaps that holds `addr`, unlike VmLck untouched by other test threads.
    fn locked_at(addr: usize) -> u64 {
        let smaps = std::fs::read_to_string("/proc/self/smaps").unwrap();
        let mut inside = false;
        for line in smaps.lines() {
            if let Some((range, _)) = line.split_once(' ')
                && let Some((from, to)) = range.split_once('-')
                && let (Ok(from), Ok(to)) = (usize::from_str_radix(from, 16), usize::from_str_radix(to, 16))
            {
                inside = (from..to).contains(&addr);
            } else if inside && let Some(kb) = line.strip_prefix("Locked:") {
                return kb.trim().trim_end_matches(" kB").trim().parse().unwrap();
            }
        }
        panic!("no mapping holds {addr:#x}");
    }

    fn mapped(len: usize) -> usize {
        let at = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(at, libc::MAP_FAILED);
        unsafe { std::ptr::write_bytes(at as *mut u8, 1, len) };
        at as usize
    }

    #[test]
    fn lock_all_locks_every_page_while_it_lives_and_a_relocked_buffer_stays_locked() {
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        let len = 4 << 20;
        let buf = SecretBuf::from_slice(b"value").unwrap();
        let at = |buf: &SecretBuf| buf.as_slice().as_ptr() as usize;
        assert!(locked_at(at(&buf)) >= 4, "a buffer's own page is locked");
        let before = mapped(len);
        assert_eq!(locked_at(before), 0, "a plain mapping isn't locked");
        let guard = LockAll::new().unwrap();
        drop(LockAll::new().unwrap());
        // Locked mappings may merge with their locked neighbours, so a mapping's figure can exceed its own size.
        assert!(
            locked_at(before) >= (len / 1024) as u64,
            "every mapped page is locked under the guard, a nested one dropped or not"
        );
        let later = mapped(len);
        assert!(locked_at(later) >= (len / 1024) as u64, "and pages mapped later");
        drop(guard);
        assert_eq!(
            (locked_at(before), locked_at(later)),
            (0, 0),
            "the guard lets go of everything"
        );
        buf.relock();
        assert!(locked_at(at(&buf)) >= 4, "and relock locks the buffer again");
        unsafe {
            libc::munmap(before as *mut libc::c_void, len);
            libc::munmap(later as *mut libc::c_void, len);
        }
    }

    #[test]
    fn a_buffer_holds_what_it_reads_up_to_its_limit_and_finds_needles() {
        let read = SecretBuf::read_all(&b"hunter22-long-secret\n"[..], 64, "x").unwrap();
        assert_eq!(read.as_slice(), b"hunter22-long-secret\n");
        let mut trimmed = SecretBuf::from_slice(b"$y$hash\n\n").unwrap();
        trimmed.trim_end_newlines();
        assert_eq!(trimmed.as_slice(), b"$y$hash");
        let error = SecretBuf::read_all(&[7u8; 100][..], 64, "wg0")
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("wg0") && error.contains("most a secret may"), "{error}");
        assert_eq!(find(b"abcabcd", b"abcd"), Some(3));
        assert!(contains(b"xxsecretxx", b"secret") && !contains(b"secre", b"secret") && !contains(b"", b"a"));
    }
}

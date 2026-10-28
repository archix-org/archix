use std::collections::{BTreeSet, HashMap};
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::{State, deployment_etc, discard, load, merge_for, never, pending, save, snapshot};
use crate::paths::Paths;

const MASK: u32 = libc::IN_ATTRIB
    | libc::IN_CLOSE_WRITE
    | libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_DELETE_SELF
    | libc::IN_MODIFY
    | libc::IN_MOVE_SELF
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_DONT_FOLLOW
    | libc::IN_ONLYDIR
    | libc::IN_EXCL_UNLINK;

/// How long the watcher waits for /etc to go quiet before a pass, and the most a stream of events delays one.
const QUIET: Duration = Duration::from_millis(300);
const AT_MOST: Duration = Duration::from_secs(2);

fn c_path(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).with_context(|| format!("{} has a NUL byte", path.display()))
}

struct Inotify {
    fd: OwnedFd,
    dirs: HashMap<i32, String>,
    /// A debug build's stand-in for the kernel's event queue length, ARCHIX_TEST_ETC_MERGE_QUEUE, so a test can overflow it.
    queue: Option<usize>,
}

impl Inotify {
    fn new() -> Result<Inotify> {
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error()).context("starting inotify");
        }
        let queue = std::env::var("ARCHIX_TEST_ETC_MERGE_QUEUE")
            .ok()
            .filter(|_| cfg!(debug_assertions))
            .and_then(|n| n.parse().ok());
        Ok(Inotify {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            dirs: HashMap::new(),
            queue,
        })
    }

    fn add(&mut self, live: &Path, rel: &str) {
        let path = if rel.is_empty() {
            live.to_path_buf()
        } else {
            live.join(rel)
        };
        let Ok(c) = c_path(&path) else { return };
        let wd = unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), c.as_ptr(), MASK) };
        if wd >= 0 {
            self.dirs.insert(wd, rel.to_string());
        }
    }

    /// Watches `rel` and every directory below it.
    fn add_tree(&mut self, live: &Path, rel: &str) {
        self.add(live, rel);
        let mut all = Vec::new();
        super::tree::walk(live, rel, &|rel| never(rel), &mut all);
        for child in all {
            if std::fs::symlink_metadata(live.join(&child)).is_ok_and(|m| m.is_dir()) {
                self.add(live, &child);
            }
        }
    }

    /// Reads what's queued into `dirty`; an overflowed queue or a moved directory asks for a full pass instead.
    fn read(&mut self, live: &Path, dirty: &mut BTreeSet<String>, full: &mut bool) {
        let mut buf = vec![0u8; 64 * 1024];
        let mut events = Vec::new();
        loop {
            let n = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
            let mut at = 0;
            while at + 16 <= n as usize {
                let field = |i: usize| u32::from_ne_bytes(buf[at + i..at + i + 4].try_into().unwrap());
                let (wd, mask, len) = (field(0) as i32, field(4), field(12) as usize);
                let name = &buf[at + 16..at + 16 + len];
                let name = name.split(|b| *b == 0).next().unwrap_or_default();
                events.push((wd, mask, String::from_utf8_lossy(name).into_owned()));
                at += 16 + len;
            }
        }
        if self.queue.is_some_and(|queue| events.len() > queue) {
            events = vec![(-1, libc::IN_Q_OVERFLOW, String::new())];
        }
        for (wd, mask, name) in events {
            if mask & libc::IN_Q_OVERFLOW != 0 {
                println!("archix: /etc merge: the kernel's event queue overflowed; going through all of /etc");
                *full = true;
                continue;
            }
            if mask & libc::IN_IGNORED != 0 {
                self.dirs.remove(&wd);
                continue;
            }
            let Some(dir) = self.dirs.get(&wd).cloned() else {
                continue;
            };
            let rel = match (dir.is_empty(), name.is_empty()) {
                (_, true) => dir,
                (true, false) => name,
                (false, false) => format!("{dir}/{name}"),
            };
            if never(&rel) {
                continue;
            }
            let is_dir = mask & libc::IN_ISDIR != 0;
            if is_dir && mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0 {
                self.add_tree(live, &rel);
            }
            if (is_dir && mask & libc::IN_MOVED_FROM != 0) || mask & libc::IN_MOVE_SELF != 0 {
                *full = true;
            }
            if !rel.is_empty() {
                dirty.insert(rel);
            }
        }
    }
}

/// SIGTERM, SIGINT and SIGHUP as a file descriptor, so the watcher finishes its last pass instead of dying mid-write.
struct Signals {
    fd: OwnedFd,
}

impl Signals {
    fn new() -> Result<Signals> {
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
                libc::sigaddset(&mut set, signal);
            }
            if libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error()).context("blocking signals");
            }
            let fd = libc::signalfd(-1, &set, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK);
            if fd < 0 {
                return Err(io::Error::last_os_error()).context("creating a signalfd");
            }
            Ok(Signals {
                fd: OwnedFd::from_raw_fd(fd),
            })
        }
    }
}

/// Makes the watcher's mounts its own, propagation still coming in, and the deployments writable in them, as the main namespace may keep /archix read-only.
fn enter_namespace(root: &Path) -> Result<()> {
    if unsafe { libc::unshare(libc::CLONE_NEWNS) } != 0 {
        return Err(io::Error::last_os_error()).context("entering a mount namespace of its own");
    }
    let flags = libc::MS_REC | libc::MS_SLAVE;
    if unsafe {
        libc::mount(
            std::ptr::null(),
            c"/".as_ptr(),
            std::ptr::null(),
            flags,
            std::ptr::null(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error()).context("making the mount namespace a slave");
    }
    let deploy = root.join("archix/deploy");
    if !crate::system::deploy_boot::read_only(&deploy) {
        return Ok(());
    }
    let info = std::fs::read("/proc/self/mountinfo").context("reading /proc/self/mountinfo")?;
    let canonical = std::fs::canonicalize(&deploy).unwrap_or(deploy.clone());
    let Some(point) = crate::deploy::view::mount_points(&info)
        .into_iter()
        .filter(|point| canonical.starts_with(point))
        .max_by_key(|point| point.components().count())
    else {
        bail!("no mount holds {}", deploy.display());
    };
    let c = c_path(&point)?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut stat) } != 0 {
        return Err(io::Error::last_os_error()).with_context(|| format!("statvfs {}", point.display()));
    }
    let keep = [
        (libc::ST_NOSUID, libc::MS_NOSUID),
        (libc::ST_NODEV, libc::MS_NODEV),
        (libc::ST_NOEXEC, libc::MS_NOEXEC),
        (libc::ST_NOATIME, libc::MS_NOATIME),
        (libc::ST_NODIRATIME, libc::MS_NODIRATIME),
        (0x1000, libc::MS_RELATIME),
    ]
    .iter()
    .filter(|(st, _)| stat.f_flag & st != 0)
    .fold(0, |all, (_, ms)| all | ms);
    let remount = libc::MS_REMOUNT | libc::MS_BIND | keep;
    if unsafe {
        libc::mount(
            std::ptr::null(),
            c.as_ptr(),
            std::ptr::null(),
            remount,
            std::ptr::null(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("making {} writable for the /etc merge", point.display()));
    }
    Ok(())
}

enum Ended {
    Stopped,
    Retarget,
}

/// `archix etc-merge`: from a commit, or a boot with a deployment pending, until shutdown, carries each change to the live /etc into the pending deployment's within seconds, and makes a last pass when stopped.
pub fn run(root: &Path) -> Result<()> {
    let paths = Paths::under(root);
    enter_namespace(root)?;
    let signals = Signals::new()?;
    loop {
        let Some((target, booted)) = pending(root) else {
            sweep(&paths, None);
            return Ok(());
        };
        sweep(&paths, Some(target));
        let state = match load(&paths, target) {
            Some(state) if state.from == booted => state,
            _ => {
                println!(
                    "archix: /etc merge: no base was taken when deployment {target} was staged, so changes from now on are carried"
                );
                snapshot(&root.join("etc"), target, booted)
            }
        };
        match follow(&paths, state, &signals)? {
            Ended::Stopped => return Ok(()),
            Ended::Retarget => continue,
        }
    }
}

/// Drops merge state for deployments that aren't pending any more.
fn sweep(paths: &Paths, keep: Option<u32>) {
    for entry in std::fs::read_dir(super::dir(paths)).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(number) = name.strip_suffix(".json").and_then(|n| n.parse::<u32>().ok())
            && Some(number) != keep
            && crate::system::boot::booted_deployment(&paths.root) != Some(number)
        {
            discard(paths, number);
        }
    }
}

fn follow(paths: &Paths, mut state: State, signals: &Signals) -> Result<Ended> {
    let root = paths.root.clone();
    let target = state.target;
    // A watcher stopped for a build and started again isn't at shutdown any more.
    state.final_pass = false;
    state.partial = 0;
    let live = root.join("etc");
    let theirs: PathBuf = deployment_etc(&root, target);
    let mut inotify = Inotify::new()?;
    inotify.add_tree(&live, "");
    let mut merge = merge_for(paths, &mut state, live.clone(), theirs);
    merge.pass(None, None);
    save(paths, merge.state)?;
    println!("archix: carrying /etc changes into deployment {target} until shutdown");
    let mut dirty = BTreeSet::new();
    let mut full = false;
    let mut first: Option<Instant> = None;
    let mut last = Instant::now();
    loop {
        let waiting = !dirty.is_empty() || full;
        let mut fds = [
            libc::pollfd {
                fd: inotify.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: signals.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let timeout = if waiting { 100 } else { 1000 };
        unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) };
        if fds[1].revents & libc::POLLIN != 0 {
            crate::testhooks::mark("etc-merge-final-pass");
            let pass = merge.pass(None, Some(super::final_deadline()));
            merge.state.final_pass = true;
            merge.state.partial = pass.unfinished.len();
            if !pass.unfinished.is_empty() {
                println!(
                    "archix: /etc merge: the pass at shutdown ran out of time before {}; the next boot carries them from the old /etc",
                    pass.unfinished.join(", ")
                );
            }
            let saved = save(paths, merge.state);
            crate::durable::sync_filesystems(&[&merge.theirs, &paths.state]);
            if let Err(e) = saved {
                eprintln!("archix: /etc merge: recording the pass at shutdown failed: {e:#}");
            }
            println!("archix: /etc merge: last pass into deployment {target} done");
            return Ok(Ended::Stopped);
        }
        if fds[0].revents & libc::POLLIN != 0 {
            inotify.read(&live, &mut dirty, &mut full);
            last = Instant::now();
            first.get_or_insert(last);
        }
        let due = first.is_some_and(|first| last.elapsed() >= QUIET || first.elapsed() >= AT_MOST);
        if (full || !dirty.is_empty()) && due {
            if full {
                inotify.add_tree(&live, "");
                merge.pass(None, None);
            } else {
                merge.pass(Some(&dirty), None);
            }
            if let Err(e) = save(paths, merge.state) {
                eprintln!("archix: /etc merge: recording the pass failed: {e:#}");
            }
            dirty.clear();
            full = false;
            first = None;
        }
        match pending(&root) {
            Some((now, _)) if now == target => {}
            Some(_) => {
                discard(paths, target);
                return Ok(Ended::Retarget);
            }
            None => {
                discard(paths, target);
                println!("archix: /etc merge: no deployment is pending any more");
                return Ok(Ended::Stopped);
            }
        }
    }
}

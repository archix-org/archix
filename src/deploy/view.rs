use std::ffi::{CString, OsStr};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use super::{Backend, OVERLAID, OVERLAY_OPTIONS};

/// Paths inside the view's /var that are bound live or from the staging tree instead of overlaid, so its upper layer never carries them.
pub const VAR_BINDS: &[&str] = &["log/pacman.log", "cache/pacman/pkg", "lib/pacman/local", "lib/dkms"];

/// Where the staging root is assembled, inside the staging tree; only the transaction's own mount namespace ever mounts anything there.
pub const VIEW_DIR: &str = ".view";

/// Runs commands chrooted in a staging tree, each in its own mount and PID namespaces, so no mount and no process outlives it.
pub struct Transaction {
    plan: Arc<Plan>,
}

/// What a staging view is made of, for `Transaction::new`.
pub struct Layout<'a> {
    pub backend: Backend,
    /// The real root of the system: its top-level directories and symlinks are mirrored, its var/ is the lower layer of the view's /var.
    pub root: &'a Path,
    /// The deployment being staged from, the lower layer of usr, opt and boot on the overlay backend.
    pub base: &'a Path,
    pub staging: &'a Path,
}

impl Transaction {
    pub fn new(layout: &Layout) -> Result<Transaction> {
        let plan = Plan::build(layout)?;
        Ok(Transaction { plan: Arc::new(plan) })
    }

    /// A command that runs `program`, found on the view's PATH, as PID 1 of a fresh PID namespace chrooted in the view; add arguments, environment and stdio as usual.
    pub fn command<S: AsRef<OsStr>>(&self, program: S) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/bin")
            .env("HOME", "/root");
        for name in ["TERM", "LANG", "LC_ALL"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let plan = Arc::clone(&self.plan);
        unsafe {
            command.pre_exec(move || plan.enter());
        }
        command
    }

    /// Runs `argv` in the view and fails unless it exits 0.
    pub fn run<S: AsRef<OsStr>>(&self, argv: &[S]) -> Result<()> {
        let (program, args) = argv.split_first().context("no command to run")?;
        let status = self
            .command(program)
            .args(args)
            .status()
            .with_context(|| format!("running {} in the staging root", describe(argv)))?;
        if !status.success() {
            bail!("{} failed in the staging root ({status})", describe(argv));
        }
        Ok(())
    }

    /// Runs `argv` in the view, capturing its output.
    pub fn output<S: AsRef<OsStr>>(&self, argv: &[S]) -> Result<Output> {
        let (program, args) = argv.split_first().context("no command to run")?;
        self.command(program)
            .args(args)
            .output()
            .with_context(|| format!("running {} in the staging root", describe(argv)))
    }
}

fn describe<S: AsRef<OsStr>>(argv: &[S]) -> String {
    argv.iter()
        .map(|arg| arg.as_ref().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// One step of building the view, done after fork where nothing may allocate, so every string is made beforehand.
enum Op {
    Mount {
        source: CString,
        target: CString,
        fstype: Option<CString>,
        flags: libc::c_ulong,
        data: Option<CString>,
    },
    MakePrivate,
    Mkdir {
        path: CString,
        mode: libc::mode_t,
        owner: Option<(libc::uid_t, libc::gid_t)>,
    },
    Symlink {
        target: CString,
        path: CString,
    },
    Mknod {
        path: CString,
        dev: libc::dev_t,
    },
    Touch {
        path: CString,
    },
}

struct Step {
    op: Op,
    failure: Vec<u8>,
}

struct Plan {
    steps: Vec<Step>,
    view: CString,
    proc_dir: CString,
    parent: libc::pid_t,
}

fn c(path: impl AsRef<OsStr>) -> Result<CString> {
    let bytes = path.as_ref().as_bytes();
    CString::new(bytes).with_context(|| format!("{} has a NUL byte", String::from_utf8_lossy(bytes)))
}

/// An overlay lower, upper or work directory, which mount options can't escape.
fn layer(path: &Path) -> Result<String> {
    let text = path
        .to_str()
        .with_context(|| format!("{} isn't UTF-8", path.display()))?;
    if text.contains([',', ':', '\\', '=']) {
        bail!("{text} can't be an overlay layer: it has a comma, colon, backslash or equals sign");
    }
    Ok(text.to_string())
}

impl Plan {
    fn build(layout: &Layout) -> Result<Plan> {
        let view = layout.staging.join(VIEW_DIR);
        fs::create_dir_all(&view).with_context(|| format!("creating {}", view.display()))?;
        let mut plan = Plan {
            steps: Vec::new(),
            view: c(&view)?,
            proc_dir: c(view.join("proc"))?,
            parent: std::process::id() as libc::pid_t,
        };
        let at = |sub: &str| view.join(sub);
        plan.push(Op::MakePrivate, "making the mount namespace private")?;
        plan.mount("tmpfs", &view, Some("tmpfs"), libc::MS_NOSUID, Some("mode=0755"))?;
        plan.skeleton(layout.root, &view)?;

        let live_var = layout.root.join("var");
        for tree in OVERLAID {
            match layout.backend {
                Backend::Snapshot => plan.bind(&layout.staging.join(tree), &at(tree))?,
                Backend::Overlay => plan.overlay(&layout.base.join(tree), layout.staging, tree, &at(tree))?,
            }
        }
        plan.bind(&layout.staging.join("etc"), &at("etc"))?;
        plan.overlay(&live_var, layout.staging, "var", &at("var"))?;

        prepare_live_var(&live_var)?;
        for bind in VAR_BINDS {
            let inside = at("var").join(bind);
            let mut dir = at("var");
            let parents: Vec<&OsStr> = Path::new(bind).iter().collect();
            for part in &parents[..parents.len() - 1] {
                dir.push(part);
                plan.mkdir(&dir, 0o755, None)?;
            }
            let source = match *bind {
                "lib/pacman/local" => layout.staging.join("pacman"),
                "lib/dkms" => layout.staging.join("dkms"),
                live => live_var.join(live),
            };
            if source.is_dir() {
                plan.mkdir(&inside, 0o755, None)?;
            } else {
                plan.push(
                    Op::Touch { path: c(&inside)? },
                    &format!("creating {}", inside.display()),
                )?;
            }
            plan.bind(&source, &inside)?;
        }

        plan.mount(
            "tmpfs",
            &at("run"),
            Some("tmpfs"),
            libc::MS_NOSUID | libc::MS_NODEV,
            Some("mode=0755"),
        )?;
        plan.mount(
            "tmpfs",
            &at("tmp"),
            Some("tmpfs"),
            libc::MS_NOSUID | libc::MS_NODEV,
            Some("mode=1777"),
        )?;
        plan.mount(
            "sysfs",
            &at("sys"),
            Some("sysfs"),
            libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            None,
        )?;
        plan.dev(&at("dev"))?;
        Ok(plan)
    }

    fn push(&mut self, op: Op, what: &str) -> Result<()> {
        self.steps.push(Step {
            op,
            failure: format!("archix: setting up the staging root failed: {what}\n").into_bytes(),
        });
        Ok(())
    }

    fn mount(
        &mut self,
        source: &str,
        target: &Path,
        fstype: Option<&str>,
        flags: libc::c_ulong,
        data: Option<&str>,
    ) -> Result<()> {
        let op = Op::Mount {
            source: c(source)?,
            target: c(target)?,
            fstype: fstype.map(c).transpose()?,
            flags,
            data: data.map(c).transpose()?,
        };
        self.push(op, &format!("mounting {source} on {}", target.display()))
    }

    fn bind(&mut self, source: &Path, target: &Path) -> Result<()> {
        let op = Op::Mount {
            source: c(source)?,
            target: c(target)?,
            fstype: None,
            flags: libc::MS_BIND,
            data: None,
        };
        self.push(op, &format!("binding {} on {}", source.display(), target.display()))
    }

    fn overlay(&mut self, lower: &Path, staging: &Path, tree: &str, target: &Path) -> Result<()> {
        let data = format!(
            "lowerdir={},upperdir={},workdir={},{OVERLAY_OPTIONS}",
            layer(lower)?,
            layer(&staging.join(".upper").join(tree))?,
            layer(&staging.join(".work").join(tree))?
        );
        self.mount("overlay", target, Some("overlay"), 0, Some(&data))
    }

    fn mkdir(&mut self, path: &Path, mode: libc::mode_t, owner: Option<(libc::uid_t, libc::gid_t)>) -> Result<()> {
        let op = Op::Mkdir {
            path: c(path)?,
            mode,
            owner,
        };
        self.push(op, &format!("creating {}", path.display()))
    }

    fn symlink(&mut self, target: &Path, path: &Path) -> Result<()> {
        let op = Op::Symlink {
            target: c(target)?,
            path: c(path)?,
        };
        self.push(op, &format!("creating {}", path.display()))
    }

    /// The live root's top level as empty directories and copied symlinks, so nothing of it is reachable but what's bound in.
    fn skeleton(&mut self, root: &Path, view: &Path) -> Result<()> {
        let mut entries: Vec<_> = fs::read_dir(root)
            .with_context(|| format!("reading {}", root.display()))?
            .flatten()
            .collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            if name == "archix" || name == "lost+found" {
                continue;
            }
            let meta = entry.path().symlink_metadata()?;
            if meta.file_type().is_symlink() {
                self.symlink(&fs::read_link(entry.path())?, &view.join(&name))?;
            } else if meta.is_dir() {
                self.mkdir(&view.join(&name), meta.mode() & 0o7777, Some((meta.uid(), meta.gid())))?;
            }
        }
        for (name, mode) in [
            ("usr", 0o755),
            ("etc", 0o755),
            ("opt", 0o755),
            ("boot", 0o755),
            ("var", 0o755),
            ("proc", 0o555),
            ("sys", 0o555),
            ("dev", 0o755),
            ("run", 0o755),
            ("tmp", 0o1777),
        ] {
            self.mkdir(&view.join(name), mode, None)?;
        }
        Ok(())
    }

    /// A private /dev with its own device nodes, so a chmod there can't reach the host's.
    fn dev(&mut self, dev: &Path) -> Result<()> {
        self.mount("tmpfs", dev, Some("tmpfs"), libc::MS_NOSUID, Some("mode=0755"))?;
        for (name, major, minor) in [
            ("null", 1, 3),
            ("zero", 1, 5),
            ("full", 1, 7),
            ("random", 1, 8),
            ("urandom", 1, 9),
            ("tty", 5, 0),
        ] {
            let path = dev.join(name);
            let op = Op::Mknod {
                path: c(&path)?,
                dev: libc::makedev(major, minor),
            };
            self.push(op, &format!("creating {}", path.display()))?;
        }
        self.mkdir(&dev.join("pts"), 0o755, None)?;
        self.mkdir(&dev.join("shm"), 0o1777, None)?;
        self.mount(
            "devpts",
            &dev.join("pts"),
            Some("devpts"),
            libc::MS_NOSUID | libc::MS_NOEXEC,
            Some("newinstance,ptmxmode=0666,mode=0620"),
        )?;
        self.mount(
            "tmpfs",
            &dev.join("shm"),
            Some("tmpfs"),
            libc::MS_NOSUID | libc::MS_NODEV,
            Some("mode=1777"),
        )?;
        for (name, target) in [
            ("ptmx", "pts/ptmx"),
            ("fd", "/proc/self/fd"),
            ("stdin", "/proc/self/fd/0"),
            ("stdout", "/proc/self/fd/1"),
            ("stderr", "/proc/self/fd/2"),
        ] {
            self.symlink(Path::new(target), &dev.join(name))?;
        }
        Ok(())
    }

    /// Runs after fork in the child archix spawned; it must not allocate.
    fn enter(&self) -> io::Result<()> {
        unsafe {
            // The death signal follows the spawning thread, so the caller must wait for the command on the thread that started it.
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 || libc::getppid() != self.parent {
                libc::_exit(127);
            }
            if libc::unshare(libc::CLONE_NEWNS | libc::CLONE_NEWPID) != 0 {
                return Err(say_failed(b"archix: can't make namespaces for the staging root\n"));
            }
            for step in &self.steps {
                if step.run().is_err() {
                    return Err(say_failed(&step.failure));
                }
            }
            let child = libc::fork();
            if child < 0 {
                return Err(io::Error::last_os_error());
            }
            if child == 0 {
                return self.become_init();
            }
            let mut status = 0;
            loop {
                let done = libc::waitpid(child, &mut status, 0);
                if done == child {
                    break;
                }
                if done < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                    libc::_exit(127);
                }
            }
            if libc::WIFEXITED(status) {
                libc::_exit(libc::WEXITSTATUS(status));
            }
            libc::_exit(128 + libc::WTERMSIG(status));
        }
    }

    /// PID 1 of the new namespace: when it exits, the kernel kills everything the command left running.
    unsafe fn become_init(&self) -> io::Result<()> {
        unsafe {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            let flags = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
            if libc::mount(
                c"proc".as_ptr(),
                self.proc_dir.as_ptr(),
                c"proc".as_ptr(),
                flags,
                std::ptr::null(),
            ) != 0
            {
                return Err(say_failed(b"archix: can't mount /proc in the staging root\n"));
            }
            if libc::chdir(self.view.as_ptr()) != 0 {
                return Err(say_failed(b"archix: can't enter the staging root\n"));
            }
            if libc::syscall(libc::SYS_pivot_root, c".".as_ptr(), c".".as_ptr()) == 0 {
                if libc::umount2(c".".as_ptr(), libc::MNT_DETACH) != 0 {
                    return Err(say_failed(
                        b"archix: can't detach the live root from the staging root\n",
                    ));
                }
            } else if libc::chroot(c".".as_ptr()) != 0 {
                return Err(say_failed(b"archix: can't chroot into the staging root\n"));
            }
            if libc::chdir(c"/".as_ptr()) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

fn say_failed(message: &[u8]) -> io::Error {
    let err = io::Error::last_os_error();
    unsafe { libc::write(2, message.as_ptr().cast(), message.len()) };
    err
}

impl Step {
    fn run(&self) -> io::Result<()> {
        let check = |result: libc::c_int| {
            if result == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        };
        unsafe {
            match &self.op {
                Op::MakePrivate => check(libc::mount(
                    std::ptr::null(),
                    c"/".as_ptr(),
                    std::ptr::null(),
                    libc::MS_REC | libc::MS_PRIVATE,
                    std::ptr::null(),
                )),
                Op::Mount {
                    source,
                    target,
                    fstype,
                    flags,
                    data,
                } => check(libc::mount(
                    source.as_ptr(),
                    target.as_ptr(),
                    fstype.as_ref().map_or(std::ptr::null(), |t| t.as_ptr()),
                    *flags,
                    data.as_ref().map_or(std::ptr::null(), |d| d.as_ptr().cast()),
                )),
                Op::Mkdir { path, mode, owner } => {
                    if libc::mkdir(path.as_ptr(), *mode) != 0 {
                        let err = io::Error::last_os_error();
                        return if err.raw_os_error() == Some(libc::EEXIST) {
                            Ok(())
                        } else {
                            Err(err)
                        };
                    }
                    if let Some((uid, gid)) = owner {
                        check(libc::lchown(path.as_ptr(), *uid, *gid))?;
                    }
                    check(libc::chmod(path.as_ptr(), *mode))
                }
                Op::Symlink { target, path } => check(libc::symlink(target.as_ptr(), path.as_ptr())),
                Op::Mknod { path, dev } => {
                    check(libc::mknod(path.as_ptr(), libc::S_IFCHR | 0o666, *dev))?;
                    check(libc::chmod(path.as_ptr(), 0o666))
                }
                Op::Touch { path } => {
                    let fd = libc::open(
                        path.as_ptr(),
                        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                        0o644,
                    );
                    if fd < 0 {
                        let err = io::Error::last_os_error();
                        // Opening an existing lower file for writing would copy it up whole.
                        return if err.raw_os_error() == Some(libc::EEXIST) {
                            Ok(())
                        } else {
                            Err(err)
                        };
                    }
                    libc::close(fd);
                    Ok(())
                }
            }
        }
    }
}

/// The live sources of the view's /var binds must exist; pacman would make them anyway.
fn prepare_live_var(var: &Path) -> Result<()> {
    let cache = var.join("cache/pacman/pkg");
    fs::create_dir_all(&cache).with_context(|| format!("creating {}", cache.display()))?;
    let log = var.join("log/pacman.log");
    if log.symlink_metadata().is_err() {
        fs::create_dir_all(var.join("log"))?;
        fs::write(&log, "").with_context(|| format!("creating {}", log.display()))?;
        fs::set_permissions(&log, fs::Permissions::from_mode(0o644))?;
    }
    Ok(())
}

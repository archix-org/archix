use std::ffi::{CString, OsStr};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use anyhow::{Context, Result, bail};

use super::{Backend, OVERLAID, OVERLAY_OPTIONS};

/// Paths inside the view's /var that are bound live or from the staging tree instead of overlaid, so its upper layer never carries them.
pub const VAR_BINDS: &[&str] = &["log/pacman.log", "cache/pacman/pkg", "lib/pacman/local", "lib/dkms"];

/// Live /var directories bound into the view when they exist: archix's package and build caches, which pacman installs from, and pacman's sync databases.
pub const VAR_LIVE_IF_PRESENT: &[&str] = &["cache/archix", "lib/pacman/sync"];

/// Mounts at or below these directories of the live /var, such as CachyOS's @log and @cache subvolumes, are bound live into the view; others stay behind the overlay, so their changes wait for activation.
pub const LIVE_VAR_MOUNTS: &[&str] = &["log", "cache", "tmp"];

/// Where the staging root is assembled, inside the staging tree, so the view's / is on the real root filesystem; only the transaction's own mount namespace ever mounts anything there.
pub const VIEW_DIR: &str = ".view";

/// Runs commands in a staging tree, each under a reaper that is PID 1 of its own PID and mount namespaces, so no mount and no process outlives it.
pub struct Transaction {
    plan: Arc<Plan>,
}

/// What a staging view is made of, for `Transaction::new`.
pub struct Layout<'a> {
    pub backend: Backend,
    /// The real root of the system: its top-level directories and symlinks are mirrored, its /var mounts and caches are bound live.
    pub root: &'a Path,
    /// The lower layers of the view's /var, top first; never the live /var, which keeps changing under a mounted overlay.
    pub var_lowers: &'a [PathBuf],
    /// The deployment being staged from, the lower layer of usr, opt and boot on the overlay backend.
    pub base: &'a Path,
    pub staging: &'a Path,
    /// Live directories or files bound in last, each at an absolute path of the view.
    pub binds: &'a [(PathBuf, PathBuf)],
    /// Whether `SECRETS` stay readable, for the boot build's own signing step; package scriptlets never run with it.
    pub signing: bool,
}

/// Signing keys, relative to the view's root, that nothing run in the view may read but the boot build's signing: sbctl's Secure Boot keys and the PCR policy key.
pub const SECRETS: &[&str] = &[
    "var/lib/sbctl/keys",
    "usr/share/secureboot/keys",
    "etc/systemd/tpm2-pcr-private-key.pem",
    "etc/kernel/secure-boot-private-key.pem",
];

impl Transaction {
    pub fn new(layout: &Layout) -> Result<Transaction> {
        let plan = Plan::build(layout)?;
        Ok(Transaction { plan: Arc::new(plan) })
    }

    /// A command that runs `program`, found on the view's PATH, as the only child of the view's PID 1; add arguments, environment and stdio as usual.
    pub fn command<S: AsRef<OsStr>>(&self, program: S) -> Command {
        self.command_in(program, false)
    }

    /// pacman in the view, which can't make the network namespace CachyOS's pacman puts each scriptlet and hook in without CAP_SYS_ADMIN, so a pacman that would gets one from the view for the whole command instead.
    pub fn pacman(&self) -> Result<Command> {
        let isolates = self.output(&["pacman", SANDBOX_NETWORK_OFF, "--version"])?.status.success();
        let mut command = self.command_in("pacman", isolates);
        if isolates {
            command.arg(SANDBOX_NETWORK_OFF);
        }
        Ok(command)
    }

    fn command_in<S: AsRef<OsStr>>(&self, program: S, offline: bool) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/bin")
            .env("HOME", "/root")
            .env("SYSTEMD_OFFLINE", "1");
        for name in ["TERM", "LANG", "LC_ALL"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let plan = Arc::clone(&self.plan);
        unsafe {
            command.pre_exec(move || plan.enter(offline));
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
    /// Covers a directory with an empty read-only tmpfs, or a file with /dev/null, when it exists.
    Hide {
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
        fs::set_permissions(&view, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("chmod {}", view.display()))?;
        let mut plan = Plan {
            steps: Vec::new(),
            view: c(&view)?,
            proc_dir: c(view.join("proc"))?,
            parent: std::process::id() as libc::pid_t,
        };
        let at = |sub: &str| view.join(sub);
        plan.push(Op::MakePrivate, "making the mount namespace private")?;
        // A bind, not a tmpfs, so tools asking what / is, such as mkinitcpio's autodetect, see the root filesystem.
        plan.bind(&view, &view)?;
        plan.skeleton(layout.root, &view)?;

        let live_var = layout.root.join("var");
        for tree in OVERLAID {
            match layout.backend {
                Backend::Snapshot => plan.bind(&layout.staging.join(tree), &at(tree))?,
                Backend::Overlay => plan.overlay(&layout.base.join(tree), layout.staging, tree, &at(tree))?,
            }
        }
        plan.bind(&layout.staging.join("etc"), &at("etc"))?;
        plan.overlay_layers(layout.var_lowers, layout.staging, "var", &at("var"))?;

        let live_mounts = live_var_mounts(&live_var)?;
        for rel in &live_mounts {
            let inside = at("var").join(rel);
            plan.mkdir(&inside, 0o755, None)?;
            let op = Op::Mount {
                source: c(live_var.join(rel))?,
                target: c(&inside)?,
                fstype: None,
                flags: libc::MS_BIND | libc::MS_REC,
                data: None,
            };
            plan.push(
                op,
                &format!("binding the live /var/{} into the staging root", rel.display()),
            )?;
        }

        prepare_live_var(&live_var)?;
        let present = VAR_LIVE_IF_PRESENT
            .iter()
            .copied()
            .filter(|rel| live_var.join(rel).is_dir());
        for bind in VAR_BINDS.iter().copied().chain(present) {
            if live_mounts.iter().any(|live| Path::new(bind).starts_with(live)) {
                continue;
            }
            let inside = at("var").join(bind);
            let mut dir = at("var");
            let parents: Vec<&OsStr> = Path::new(bind).iter().collect();
            for part in &parents[..parents.len() - 1] {
                dir.push(part);
                plan.mkdir(&dir, 0o755, None)?;
            }
            let source = match bind {
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
        for (source, inside) in layout.binds {
            let rel = inside
                .strip_prefix("/")
                .with_context(|| format!("{} isn't an absolute path", inside.display()))?;
            let target = view.join(rel);
            let mut dir = view.clone();
            let parts: Vec<&OsStr> = rel.iter().collect();
            for part in parts.iter().take(parts.len().saturating_sub(1)) {
                dir.push(part);
                plan.mkdir(&dir, 0o755, None)?;
            }
            if source.is_dir() {
                plan.mkdir(&target, 0o755, None)?;
            } else {
                plan.push(
                    Op::Touch { path: c(&target)? },
                    &format!("creating {}", target.display()),
                )?;
            }
            plan.bind(source, &target)?;
        }
        if !layout.signing {
            for secret in SECRETS {
                let path = view.join(secret);
                plan.push(Op::Hide { path: c(&path)? }, &format!("hiding {}", path.display()))?;
            }
        }
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
        self.overlay_layers(&[lower.to_path_buf()], staging, tree, target)
    }

    fn overlay_layers(&mut self, lowers: &[PathBuf], staging: &Path, tree: &str, target: &Path) -> Result<()> {
        let lowers = lowers.iter().map(|lower| layer(lower)).collect::<Result<Vec<_>>>()?;
        let data = format!(
            "lowerdir={},upperdir={},workdir={},{OVERLAY_OPTIONS}",
            lowers.join(":"),
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

    /// Runs after fork in the child archix spawned, which builds the view and waits for the namespace's PID 1; nothing here may allocate.
    fn enter(&self, offline: bool) -> io::Result<()> {
        unsafe {
            // The death signal follows the spawning thread, so the caller must wait for the command on the thread that started it.
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 || libc::getppid() != self.parent {
                libc::_exit(127);
            }
            let net = if offline { libc::CLONE_NEWNET } else { 0 };
            if libc::unshare(libc::CLONE_NEWNS | libc::CLONE_NEWPID | net) != 0 {
                return Err(say_failed(b"archix: can't make namespaces for the staging root\n"));
            }
            if offline && !loopback_up() {
                return Err(say_failed(b"archix: can't bring up loopback in the staging root\n"));
            }
            for step in &self.steps {
                if step.run().is_err() {
                    return Err(say_failed(&step.failure));
                }
            }
            let this = libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0) as libc::c_int;
            if this < 0 {
                return Err(say_failed(b"archix: can't watch the staging root's parent\n"));
            }
            let init = fork_forwarding()?;
            if init == 0 {
                return self.become_init(this);
            }
            close_inherited();
            libc::sigprocmask(libc::SIG_UNBLOCK, &forwarded_signals(), std::ptr::null_mut());
            libc::_exit(wait_for(init));
        }
    }

    /// PID 1 of the new namespace, a reaper that forwards SIGTERM and SIGINT to the command; when it exits, the kernel kills everything the command left running.
    unsafe fn become_init(&self, parent: libc::c_int) -> io::Result<()> {
        unsafe {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            let mut watch = libc::pollfd {
                fd: parent,
                events: libc::POLLIN,
                revents: 0,
            };
            if libc::poll(&mut watch, 1, 0) != 0 {
                libc::_exit(127);
            }
            libc::close(parent);
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
            if libc::syscall(libc::SYS_pivot_root, c".".as_ptr(), c".".as_ptr()) != 0 {
                return Err(say_failed(b"archix: can't pivot into the staging root\n"));
            }
            if libc::umount2(c".".as_ptr(), libc::MNT_DETACH) != 0 {
                return Err(say_failed(
                    b"archix: can't detach the live root from the staging root\n",
                ));
            }
            if libc::chdir(c"/".as_ptr()) != 0 {
                return Err(io::Error::last_os_error());
            }
            let command = fork_forwarding()?;
            if command == 0 {
                // Without it nothing the command runs can mount or unmount, so the covers over SECRETS stay.
                if libc::prctl(libc::PR_CAPBSET_DROP, CAP_SYS_ADMIN, 0, 0, 0) != 0 {
                    return Err(say_failed(b"archix: can't drop CAP_SYS_ADMIN in the staging root\n"));
                }
                for signal in FORWARDED {
                    libc::signal(signal, libc::SIG_DFL);
                }
                libc::sigprocmask(libc::SIG_UNBLOCK, &forwarded_signals(), std::ptr::null_mut());
                return Ok(());
            }
            close_inherited();
            libc::sigprocmask(libc::SIG_UNBLOCK, &forwarded_signals(), std::ptr::null_mut());
            let mut status = 0;
            loop {
                let done = libc::waitpid(-1, &mut status, 0);
                if done == command {
                    libc::_exit(exit_code(status));
                }
                if done < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                    libc::_exit(127);
                }
            }
        }
    }
}

const FORWARDED: [libc::c_int; 2] = [libc::SIGTERM, libc::SIGINT];

/// The kernel's number for CAP_SYS_ADMIN, which the libc crate doesn't define.
const CAP_SYS_ADMIN: libc::c_ulong = 21;

/// The option that keeps CachyOS's pacman from making network namespaces itself; pacman without that sandbox rejects it.
const SANDBOX_NETWORK_OFF: &str = "--disable-sandbox-network";

/// Brings up the loopback interface a new network namespace starts with down, as pacman's own network sandbox does; nothing here may allocate.
unsafe fn loopback_up() -> bool {
    unsafe {
        let sock = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if sock < 0 {
            return false;
        }
        let mut ifr: libc::ifreq = std::mem::zeroed();
        for (to, from) in ifr.ifr_name.iter_mut().zip(b"lo") {
            *to = *from as libc::c_char;
        }
        // The request's type is c_ulong on glibc and c_int on musl.
        let up = libc::ioctl(sock, libc::SIOCGIFFLAGS as _, &mut ifr) == 0 && {
            ifr.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
            libc::ioctl(sock, libc::SIOCSIFFLAGS as _, &ifr) == 0
        };
        libc::close(sock);
        up
    }
}

static FORWARD_TO: AtomicI32 = AtomicI32::new(0);

extern "C" fn forward(signal: libc::c_int) {
    let pid = FORWARD_TO.load(Ordering::Relaxed);
    if pid > 0 {
        unsafe { libc::kill(pid, signal) };
    }
}

fn forwarded_signals() -> libc::sigset_t {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for signal in FORWARDED {
            libc::sigaddset(&mut set, signal);
        }
        set
    }
}

/// Forks with SIGTERM and SIGINT blocked and forwarded to the child once unblocked; both sides return with them still blocked.
unsafe fn fork_forwarding() -> io::Result<libc::pid_t> {
    unsafe {
        libc::sigprocmask(libc::SIG_BLOCK, &forwarded_signals(), std::ptr::null_mut());
        for signal in FORWARDED {
            libc::signal(signal, forward as *const () as libc::sighandler_t);
        }
        let child = libc::fork();
        if child < 0 {
            return Err(io::Error::last_os_error());
        }
        if child > 0 {
            FORWARD_TO.store(child, Ordering::Relaxed);
        }
        Ok(child)
    }
}

/// Closes everything but stdio, above all the close-on-exec pipe through which spawn() learns the command started: a process that never execs would hold it open until the command ends.
fn close_inherited() {
    unsafe { libc::syscall(libc::SYS_close_range, 3, libc::c_uint::MAX, 0) };
}

fn wait_for(child: libc::pid_t) -> libc::c_int {
    let mut status = 0;
    loop {
        let done = unsafe { libc::waitpid(child, &mut status, 0) };
        if done == child {
            return exit_code(status);
        }
        if done < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return 127;
        }
    }
}

fn exit_code(status: libc::c_int) -> libc::c_int {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        128 + libc::WTERMSIG(status)
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
        let existing_is_fine = |result: libc::c_int| {
            if result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EEXIST) {
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
                        return existing_is_fine(-1);
                    }
                    if let Some((uid, gid)) = owner {
                        check(libc::lchown(path.as_ptr(), *uid, *gid))?;
                    }
                    check(libc::chmod(path.as_ptr(), *mode))
                }
                Op::Symlink { target, path } => existing_is_fine(libc::symlink(target.as_ptr(), path.as_ptr())),
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
                        // Opening an existing lower file for writing would copy it up whole.
                        return existing_is_fine(-1);
                    }
                    libc::close(fd);
                    Ok(())
                }
                Op::Hide { path } => {
                    let mut stat: libc::stat = std::mem::zeroed();
                    if libc::lstat(path.as_ptr(), &mut stat) != 0 {
                        return Ok(());
                    }
                    if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
                        check(libc::mount(
                            c"tmpfs".as_ptr(),
                            path.as_ptr(),
                            c"tmpfs".as_ptr(),
                            libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
                            c"mode=0700,size=4k".as_ptr().cast(),
                        ))
                    } else if stat.st_mode & libc::S_IFMT == libc::S_IFREG {
                        check(libc::mount(
                            c"/dev/null".as_ptr(),
                            path.as_ptr(),
                            std::ptr::null(),
                            libc::MS_BIND,
                            std::ptr::null(),
                        ))
                    } else {
                        Ok(())
                    }
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

/// The outermost mounts at or below `LIVE_VAR_MOUNTS` in the live /var, relative to it.
fn live_var_mounts(var: &Path) -> Result<Vec<PathBuf>> {
    let var = fs::canonicalize(var).with_context(|| format!("resolving {}", var.display()))?;
    let mountinfo = fs::read("/proc/self/mountinfo").context("reading /proc/self/mountinfo")?;
    let mut found: Vec<PathBuf> = mount_points(&mountinfo)
        .into_iter()
        .filter_map(|point| point.strip_prefix(&var).ok().map(Path::to_path_buf))
        .filter(|rel| {
            rel.iter()
                .next()
                .is_some_and(|top| LIVE_VAR_MOUNTS.iter().any(|live| top == OsStr::new(live)))
        })
        .collect();
    found.sort();
    found.dedup();
    let mut outermost: Vec<PathBuf> = Vec::new();
    for rel in found {
        if !outermost.iter().any(|outer| rel.starts_with(outer)) {
            outermost.push(rel);
        }
    }
    Ok(outermost)
}

/// The mount points in a mountinfo table, with its octal escapes undone.
pub fn mount_points(mountinfo: &[u8]) -> Vec<PathBuf> {
    mountinfo
        .split(|&b| b == b'\n')
        .filter_map(|line| line.split(|&b| b == b' ').nth(4))
        .map(|field| {
            let mut out = Vec::with_capacity(field.len());
            let mut i = 0;
            while i < field.len() {
                let octal = field
                    .get(i + 1..i + 4)
                    .filter(|d| field[i] == b'\\' && d.iter().all(|b| (b'0'..=b'7').contains(b)));
                match octal {
                    Some(d) => {
                        out.push((d[0] - b'0') * 64 + (d[1] - b'0') * 8 + (d[2] - b'0'));
                        i += 4;
                    }
                    None => {
                        out.push(field[i]);
                        i += 1;
                    }
                }
            }
            PathBuf::from(OsStr::from_bytes(&out))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_points_undo_mountinfo_escapes() {
        let table = b"22 1 0:21 / /var/log rw - btrfs /dev/x rw\n23 1 0:22 / /var/my\\040cache rw - tmpfs t rw\n";
        assert_eq!(
            mount_points(table),
            vec![PathBuf::from("/var/log"), PathBuf::from("/var/my cache")]
        );
    }
}

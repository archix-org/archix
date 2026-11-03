#![allow(dead_code)]

use std::cell::{Cell, RefCell};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard};

use archix::pacman::read_local_db;
use archix::paths::Paths;
use flate2::Compression;
use flate2::write::GzEncoder;
use tempfile::TempDir;

/// The snapshot the test image is pinned to, so locking at it changes nothing but what a test declares.
pub const IMAGE_SNAPSHOT: &str = "2026-10-10";
static SYSTEM: Mutex<()> = Mutex::new(());
/// Small packages the boot tests add one at a time.
pub const UKI_TEST_PACKAGES: [&str; 10] = [
    "tree", "bc", "pv", "dos2unix", "figlet", "rlwrap", "cowsay", "sl", "toilet", "ncdu",
];
/// A tmpfs for the machines and their template, which hardlinks need on one filesystem; the container's /tmp is noexec, which binds of it would keep.
const MACHINES: &str = "/archix-machines";
/// Where the template every machine's /usr is hardlinked from lives.
const TEMPLATE: &str = "/archix-machines/template";
/// The static binary tests/arch/run.sh builds, which the initramfs hook copies.
const STATIC_BUILD: &str = "/target/x86_64-unknown-linux-musl/release/archix";
/// The machine archix runs against right now, whose current deployment `installed` reads.
static ACTIVE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Packages tests install and remove; they never count as part of the image's own declared set.
fn test_package(name: &str) -> bool {
    UKI_TEST_PACKAGES.contains(&name) || matches!(name, "tree" | "lemon" | "jq") || name.starts_with("archix-test-")
}

pub fn lock_system() -> MutexGuard<'static, ()> {
    SYSTEM.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Checks a test that runs several behaviours on one setup collects, so one failing doesn't hide the others; `finish` fails naming them all.
#[derive(Default)]
pub struct Checks {
    failed: RefCell<Vec<String>>,
    finished: Cell<bool>,
}

impl Checks {
    pub fn check(&self, ok: bool, what: impl std::fmt::Display) -> bool {
        if !ok {
            eprintln!("check failed: {what}");
            self.failed.borrow_mut().push(what.to_string());
        }
        ok
    }

    pub fn eq<T: PartialEq + std::fmt::Debug>(&self, left: T, right: T, what: impl std::fmt::Display) -> bool {
        let ok = left == right;
        self.check(ok, format_args!("{what}: {left:?} != {right:?}"))
    }

    pub fn finish(&self) {
        self.finished.set(true);
        let failed = self.failed.borrow();
        assert!(
            failed.is_empty(),
            "{} checks failed:\n- {}",
            failed.len(),
            failed.join("\n- ")
        );
    }
}

impl Drop for Checks {
    fn drop(&mut self) {
        let failed = self.failed.borrow();
        if !self.finished.get() && !failed.is_empty() {
            eprintln!(
                "before the test stopped, {} checks had failed:\n- {}",
                failed.len(),
                failed.join("\n- ")
            );
        }
    }
}

fn sh(script: &str) {
    let output = Command::new("bash").args(["-ec", script]).output().unwrap();
    assert!(
        output.status.success(),
        "{script}\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The test image's userland without what no machine runs (compilers, headers, docs, kernel modules), built once per container.
fn template() -> PathBuf {
    sh(&format!(
        "mkdir -p {MACHINES}; mountpoint -q {MACHINES} || mount -t tmpfs -o mode=0755 archix-machines {MACHINES}"
    ));
    let lock = fs::File::create(format!("{TEMPLATE}.lock")).unwrap();
    unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX) };
    if Path::new(TEMPLATE).join(".done").exists() {
        return PathBuf::from(TEMPLATE);
    }
    assert!(
        Path::new(STATIC_BUILD).is_file(),
        "{STATIC_BUILD} is missing; tests/arch/run.sh builds it before the tests"
    );
    sh(&format!(
        r#"t={TEMPLATE}; rm -rf "$t"; mkdir -p "$t"
        tar -C / --xattrs --xattrs-include='*' --exclude=usr/lib/rustlib --exclude=usr/lib/gcc --exclude=usr/include \
          --exclude=usr/share/doc --exclude=usr/share/man --exclude=usr/share/info --exclude=usr/share/qemu \
          --exclude=usr/share/edk2 --exclude='usr/lib/modules/*/kernel' --exclude='usr/lib/modules/*/build' -cf - usr \
          | tar -C "$t" --xattrs --xattrs-include='*' -xpf -
        install -Dm755 {STATIC_BUILD} "$t/usr/lib/archix/archix-static"
        install -Dm755 {archix} "$t{archix}"
        touch "$t/.done""#,
        archix = env!("CARGO_BIN_EXE_archix"),
    ));
    PathBuf::from(TEMPLATE)
}

/// A loop btrfs laid out as CachyOS does, `@` at the top level, whose top level and `@` stay mounted until it's dropped.
pub struct Btrfs {
    _dir: TempDir,
    pub top: PathBuf,
    pub root: PathBuf,
}

impl Btrfs {
    fn new(size: &str) -> Btrfs {
        let dir = TempDir::new().unwrap();
        let (image, top, root) = (
            dir.path().join("disk.img"),
            dir.path().join("top"),
            dir.path().join("root"),
        );
        fs::create_dir_all(&top).unwrap();
        fs::create_dir_all(&root).unwrap();
        sh(&format!(
            "truncate -s {size} {image} && mkfs.btrfs -q {image} && mount -o loop {image} {top} && btrfs -q subvolume create {top}/@ && mount -o subvol=/@ \"$(findmnt -n -o SOURCE {top})\" {root}",
            image = image.display(),
            top = top.display(),
            root = root.display()
        ));
        Btrfs { _dir: dir, top, root }
    }

    /// Mounts @archix at <root>/.archix, as the fstab line rollbacks' setup writes does from the next boot on.
    pub fn mount_archix(&self) {
        let device = sh_out(&format!("findmnt -n -o SOURCE {}", self.top.display()));
        let device = device.trim().split('[').next().unwrap().to_string();
        sh(&format!(
            "btrfs -q subvolume create {top}/@archix && mkdir -p {root}/.archix && mount -o subvol=/@archix {device} {root}/.archix",
            top = self.top.display(),
            root = self.root.display()
        ));
    }
}

impl Drop for Btrfs {
    fn drop(&mut self) {
        Machine::unmount_all(&self.root);
        Machine::unmount_all(&self.top);
    }
}

fn sh_out(script: &str) -> String {
    let output = Command::new("bash").args(["-ec", script]).output().unwrap();
    assert!(
        output.status.success(),
        "{script}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// What a machine leaves in the container's archix state, which its next machine and tests on the container itself must not see.
const FORGET: &str = "cd /var/lib/archix && rm -rf generations boot backups edited users id-map.json config.git outside-changes.json notes kernels secrets";

/// A whole Arch root in a directory, which archix switches through ARCHIX_TEST_ROOT: its /usr is hardlinked from the template (or, on btrfs, an overlay of it), its own path inside it leads back to it so paths archix recorded outside resolve in pacman's hooks, chrooted there, /etc and pacman's database are copies of the container's, and the container's package caches and archix state are bound in.
pub struct Machine {
    dir: Option<TempDir>,
    pub root: PathBuf,
    /// The btrfs the root is `@` of, when it's on one.
    pub btrfs: Option<Btrfs>,
    /// On the whole rollback layout, whose archix state is the root's own, bound over the container's.
    layout: bool,
}

impl Machine {
    pub fn new() -> Machine {
        let template = template();
        let dir = tempfile::Builder::new()
            .prefix("machine-")
            .tempdir_in(MACHINES)
            .unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(&root).unwrap();
        Machine::populate(&root, &template, false);
        Machine {
            dir: Some(dir),
            root,
            btrfs: None,
            layout: false,
        }
    }

    /// A machine whose root is `@` on a loop btrfs; its /usr is an overlay of the template's whose changes land in `@`.
    pub fn on_btrfs() -> Machine {
        Machine::on_btrfs_of("2G")
    }

    /// Like `on_btrfs`, on a disk of `size`.
    pub fn on_btrfs_of(size: &str) -> Machine {
        let template = template();
        let btrfs = Btrfs::new(size);
        let root = btrfs.root.clone();
        Machine::populate(&root, &template, true);
        Machine {
            dir: None,
            root,
            btrfs: Some(btrfs),
            layout: false,
        }
    }

    /// A machine on the whole rollback layout, as `archix snapshot setup` and the next boot leave one: `@` with @archix at /.archix and @varlib at /var/lib, pacman's database, DKMS's and archix's state on `@` under /var/rollback and bound over their /var/lib paths, and /run a tmpfs; archix's state is bound over the container's own until the machine goes, so it rolls back with the root.
    pub fn on_layout(size: &str) -> Machine {
        let template = template();
        let btrfs = Btrfs::new(size);
        let root = btrfs.root.clone();
        btrfs.mount_archix();
        sh(&format!(
            r#"r={root}; d="$(findmnt -n -o SOURCE {top} | cut -d'[' -f1)"
            btrfs -q subvolume create {top}/@varlib && mkdir -p $r/var/lib && mount -o subvol=/@varlib "$d" $r/var/lib"#,
            root = root.display(),
            top = btrfs.top.display()
        ));
        Machine::populate(&root, &template, true);
        sh(&format!(
            r#"r={root}; umount $r/var/lib/archix; mkdir -p $r/var/rollback
            for n in pacman dkms archix; do mv $r/var/lib/$n $r/var/rollback/$n; mkdir $r/var/lib/$n; : > $r/var/lib/$n/db.lck; done
            cp -a /var/lib/archix/. $r/var/rollback/archix/"#,
            root = root.display()
        ));
        let machine = Machine {
            dir: None,
            root,
            btrfs: Some(btrfs),
            layout: true,
        };
        machine.bind_rolled();
        machine
    }

    fn bind_rolled(&self) {
        sh(&format!(
            r#"r={root}; mount --bind $r/var/rollback/archix /var/lib/archix
            for n in pacman dkms archix; do mount --bind $r/var/rollback/$n $r/var/lib/$n; done
            mount -t tmpfs -o mode=0755 run $r/run"#,
            root = self.root.display()
        ));
    }

    /// Unmounts everything and mounts the root again as a boot would, `@` being whatever subvolume has that name now, as after a keep.
    pub fn reboot(&self) {
        assert!(self.layout, "only a machine on the whole layout reboots");
        let btrfs = self.btrfs.as_ref().unwrap();
        let _ = Command::new("umount").arg("/var/lib/archix").status();
        Machine::unmount_all(&self.root);
        assert!(
            Machine::mounts_under(&self.root).is_empty(),
            "the old root is still mounted"
        );
        sh(&format!(
            r#"r={root}; t={template}; d="$(findmnt -n -o SOURCE {top} | cut -d'[' -f1)"
            mount -o subvol=/@ "$d" $r; mount -o subvol=/@archix "$d" $r/.archix; mount -o subvol=/@varlib "$d" $r/var/lib
            mount -t overlay overlay -o "lowerdir=$t/usr,upperdir=$r/.usr-upper,workdir=$r/.usr-work" "$r/usr"
            mount --bind -o ro "$t/target" "$r/target"
            for d in var/cache/archix var/cache/pacman/pkg; do mount --bind "/$d" "$r/$d"; done
            mount -t proc proc "$r/proc"; mount --rbind /dev "$r/dev""#,
            root = self.root.display(),
            template = template().display(),
            top = btrfs.top.display()
        ));
        self.bind_rolled();
    }

    fn populate(root: &Path, template: &Path, bound: bool) {
        let usr = match bound {
            false => r#"cp -al "$t/usr" "$r/usr"; cp -al "$t/target" "$r/target""#,
            true => {
                r#"mkdir -p "$r/usr" "$r/target" "$r/.usr-upper" "$r/.usr-work"
                mount -t overlay overlay -o "lowerdir=$t/usr,upperdir=$r/.usr-upper,workdir=$r/.usr-work" "$r/usr"
                mount --bind -o ro "$t/target" "$r/target""#
            }
        };
        sh(&format!(
            r#"r={root}; t={template}; umask 022
            chmod 755 "$(dirname "$r")"
            {usr}
            cp -a /etc "$r/etc"
            rm -f "$r"/etc/pacman.d/hooks/zz-archix-test-*
            printf 'MODULES=()\nBINARIES=()\nFILES=()\nHOOKS=(base)\nCOMPRESSION=cat\n' > "$r/etc/mkinitcpio.conf"
            mkdir -p "$r/etc/kernel"; echo 'root=/dev/vda2 rw' > "$r/etc/kernel/cmdline"
            for d in boot opt efi home root srv tmp run proc sys dev mnt var/log var/lib/pacman var/lib/dkms \
                var/lib/archix var/cache/archix var/cache/pacman/pkg var/tmp; do mkdir -p "$r/$d"; done
            chmod 1777 "$r/tmp" "$r/var/tmp"; chmod 700 "$r/root"
            ln -s usr/bin "$r/bin"; ln -s usr/bin "$r/sbin"; ln -s usr/lib "$r/lib"; ln -s usr/lib "$r/lib64"
            cp -a /var/lib/pacman/local "$r/var/lib/pacman/local"
            : > "$r/var/log/pacman.log"
            mkdir -p /var/cache/archix /var/cache/pacman/pkg /var/lib/archix
            for d in var/cache/archix var/cache/pacman/pkg var/lib/archix; do mount --bind "/$d" "$r/$d"; done
            mount -t proc proc "$r/proc"; mount --rbind /dev "$r/dev"
            mkdir -p "$r$(dirname "$r")"; ln -sfn / "$r$r"
            {FORGET}"#,
            root = root.display(),
            template = template.display(),
            FORGET = FORGET,
        ));
        *ACTIVE.lock().unwrap() = Some(root.to_path_buf());
    }

    /// Where `path`, relative to the root, is on the machine.
    pub fn system_file(&self, path: &str) -> PathBuf {
        self.root.join(path)
    }

    /// The machine's pacman database.
    pub fn local_db(&self) -> PathBuf {
        self.root.join("var/lib/pacman/local")
    }

    pub fn esp(&self) -> PathBuf {
        self.root.join("efi")
    }

    /// The environment that points archix at this machine.
    pub fn env(&self) -> Vec<(String, String)> {
        vec![("ARCHIX_TEST_ROOT".into(), self.root.display().to_string())]
    }

    /// The generation the machine was last switched or rolled back to.
    pub fn current(&self) -> Option<u32> {
        fs::read_to_string("/var/lib/archix/generations/current")
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// A package's version in the machine's pacman database.
    pub fn installed(&self, name: &str) -> Option<String> {
        read_local_db(&self.local_db())
            .unwrap()
            .remove(name)
            .map(|pkg| pkg.version)
    }

    /// archix's boot entries on the ESP by file name.
    pub fn entries(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.esp().join("loader/entries"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("archix-"))
            .collect();
        names.sort();
        names
    }

    /// Mount points at or under `path`, deepest first.
    fn mounts_under(path: &Path) -> Vec<PathBuf> {
        let mut points: Vec<PathBuf> = archix::conflicts::mount_points()
            .into_iter()
            .filter(|point| point.starts_with(path))
            .collect();
        points.sort_by_key(|point| std::cmp::Reverse(point.components().count()));
        points
    }

    fn unmount_all(path: &Path) {
        for _ in 0..4 {
            for point in Machine::mounts_under(path) {
                let _ = Command::new("umount").arg("-l").arg(&point).status();
            }
        }
    }

    /// Runs plain pacman against the machine, as `pacman -U` or yay would there.
    pub fn pacman(&self, args: &[&str]) {
        fs::create_dir_all(self.root.join("etc/pacman.d/hooks")).unwrap();
        let status = Command::new("pacman")
            .arg("--root")
            .arg(&self.root)
            .arg("--dbpath")
            .arg(self.root.join("var/lib/pacman"))
            .arg("--config")
            .arg(self.root.join("etc/pacman.conf"))
            .arg("--hookdir")
            .arg(self.root.join("etc/pacman.d/hooks"))
            .args(args)
            .arg("--noconfirm")
            .status()
            .unwrap();
        assert!(status.success(), "pacman {args:?}");
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = Command::new("bash").args(["-c", FORGET]).status();
        if self.layout {
            let _ = Command::new("umount").arg("/var/lib/archix").status();
        }
        let mut active = ACTIVE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if active.as_deref() == Some(self.root.as_path()) {
            *active = None;
        }
        Machine::unmount_all(&self.root);
        let left = Machine::mounts_under(&self.root);
        if !left.is_empty() {
            eprintln!("warning: still mounted, so {} is left: {left:?}", self.root.display());
            std::mem::forget(self.dir.take());
            std::mem::forget(self.btrfs.take());
        }
    }
}

pub struct Setup {
    pub dir: TempDir,
    /// The config directory archix runs with: `dir`, or the machine's /etc/archix once moved there.
    pub config: PathBuf,
    pub paths: Paths,
    pub machine: Machine,
}

/// The boot settings every test config gets unless it sets its own, for a machine's ESP.
pub fn boot_body(machine: &Machine) -> String {
    format!(
        r#"boot = {{ loader = "systemd-boot", esp = "{}", kernel = "linux", rescue_entry = false }}, kernel_params = {{ "root=/dev/vda2 rw" }}"#,
        machine.esp().display()
    )
}

impl Setup {
    /// A config declaring everything explicitly installed in the image plus `extra`, locked at the image's snapshot, for a fresh machine.
    pub fn new(extra: &[&str], body: &str) -> Setup {
        Setup::with_files(extra, body, &[])
    }

    /// Like `new`, with `files` (path relative to the config, content) written before the first lock.
    pub fn with_files(extra: &[&str], body: &str, files: &[(&str, &str)]) -> Setup {
        Setup::on(Machine::new(), extra, body, files)
    }

    /// Like `new`, for a machine already made, such as one on btrfs.
    pub fn on(machine: Machine, extra: &[&str], body: &str, files: &[(&str, &str)]) -> Setup {
        let paths = Paths::under(&machine.root);
        let paths = Paths {
            state: PathBuf::from("/var/lib/archix"),
            cache: PathBuf::from("/var/cache/archix"),
            pacman_cache: PathBuf::from("/var/cache/pacman/pkg"),
            ..paths
        };
        let dir = TempDir::new().unwrap();
        for (path, content) in files {
            let path = dir.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        let setup = Setup {
            config: dir.path().to_path_buf(),
            dir,
            paths,
            machine,
        };
        setup.write_init(extra, body);
        setup.ok(&["update", "--to", IMAGE_SNAPSHOT, "--noconfirm"]);
        setup
    }

    /// Moves the config into the machine's /etc/archix, as on a real machine, and runs archix with it there.
    pub fn move_config_to_etc(&mut self) {
        let etc = self.machine.root.join("etc/archix");
        sh(&format!(
            "rm -rf {etc}; cp -a {from} {etc}",
            etc = etc.display(),
            from = self.config.display()
        ));
        self.config = etc;
    }

    /// Writes init.lua declaring the image's explicit packages plus `extra`, with the machine's boot settings unless `body` has its own.
    fn write_init(&self, extra: &[&str], body: &str) {
        let mut packages: Vec<String> = read_local_db(Path::new("/var/lib/pacman/local"))
            .unwrap()
            .into_values()
            .filter(|pkg| pkg.explicit)
            .map(|pkg| pkg.name)
            .filter(|name| !test_package(name))
            .collect();
        packages.extend(extra.iter().map(|p| p.to_string()));
        let quoted: Vec<String> = packages.iter().map(|p| format!("{p:?}")).collect();
        let mut fields = vec![format!("packages = {{ {} }}", quoted.join(", "))];
        if !body.trim().is_empty() {
            fields.push(body.to_string());
        }
        if !body.contains("boot =") {
            fields.push(boot_body(&self.machine));
        }
        let init = format!("return {{ {} }}", fields.join(", "));
        fs::write(self.config.join("init.lua"), init).unwrap();
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.run_env(&[], args)
    }

    pub fn run_env(&self, env: &[(&str, &str)], args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_archix"))
            .args(["--config", self.config.to_str().unwrap(), "--host", "box"])
            .args(args)
            .envs(self.machine.env())
            .envs(env.iter().copied())
            .output()
            .unwrap()
    }

    pub fn ok_env(&self, env: &[(&str, &str)], args: &[&str]) -> String {
        let output = self.run_env(env, args);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "archix {args:?} failed:\n{text}");
        text
    }

    pub fn read(&self, path: &str) -> Option<String> {
        fs::read_to_string(self.config.join(path)).ok()
    }

    pub fn write(&self, path: &str, content: &str) {
        fs::write(self.config.join(path), content).unwrap();
    }

    pub fn answering_raw(&self, input: &str, args: &[&str]) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_archix"))
            .args(["--config", self.config.to_str().unwrap(), "--host", "box"])
            .args(args)
            .envs(self.machine.env())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    }

    pub fn answering(&self, input: &str, args: &[&str]) -> String {
        let output = self.answering_raw(input, args);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "archix {args:?} failed:\n{text}");
        text
    }

    pub fn lock(&self) -> archix::lock::HostLock {
        archix::lock::Lockfile::load(&self.config.join("archix.lock"))
            .unwrap()
            .hosts["box"]
            .clone()
    }

    pub fn ok(&self, args: &[&str]) -> String {
        self.ok_env(&[], args)
    }

    pub fn fails(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(!output.status.success(), "archix {args:?} should have failed");
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    /// Replaces the config with one declaring `extra` and `body`, relocked at the image's snapshot.
    pub fn set_body(&self, extra: &[&str], body: &str) {
        let _ = fs::remove_file(self.config.join("archix.lock"));
        self.write_init(extra, body);
        self.ok(&["update", "--to", IMAGE_SNAPSHOT, "--noconfirm"]);
    }

    /// The first switch, which records generation 1 (and applies whatever the config changes).
    pub fn first_switch(&self) -> String {
        self.ok(&["switch", "--noconfirm"])
    }
}

/// Lines only in `before` as "- line" and only in `after` as "+ line", or "(identical)".
pub fn line_diff(before: &str, after: &str) -> String {
    let (old, new): (Vec<&str>, Vec<&str>) = (before.lines().collect(), after.lines().collect());
    let mut out: Vec<String> = old
        .iter()
        .filter(|l| !new.contains(l))
        .map(|l| format!("- {l}"))
        .collect();
    out.extend(new.iter().filter(|l| !old.contains(l)).map(|l| format!("+ {l}")));
    if out.is_empty() && before == after {
        return "(identical)".to_string();
    }
    if out.is_empty() {
        out.push("(same lines, different bytes)".to_string());
    }
    out.join("\n")
}

/// The version of `name` installed on the active machine.
pub fn installed(name: &str) -> Option<String> {
    let root = ACTIVE.lock().unwrap().clone().expect("no machine is active");
    read_local_db(&root.join("var/lib/pacman/local"))
        .unwrap()
        .remove(name)
        .map(|pkg| pkg.version)
}

/// A file on the active machine; `path` is absolute.
pub fn system_path(path: &str) -> PathBuf {
    let root = ACTIVE.lock().unwrap().clone().expect("no machine is active");
    root.join(path.trim_start_matches('/'))
}

/// Writes `path` of the active machine by hand, as a user would.
pub fn write_system_path(path: &str, contents: &str) {
    fs::write(system_path(path), contents).unwrap();
}

/// Plain pacman on the container itself, for tests of what `archix init` reads from the machine it runs on.
pub fn pacman(args: &[&str]) {
    let status = Command::new("pacman").args(args).arg("--noconfirm").status().unwrap();
    assert!(status.success(), "pacman {args:?}");
}

pub fn cached_file(dir: &Path, name: &str) -> PathBuf {
    fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            let file = path.file_name().unwrap().to_string_lossy().into_owned();
            file.starts_with(&format!("{name}-")) && file.ends_with(".pkg.tar.zst")
        })
        .unwrap_or_else(|| panic!("no cached {name} package in {}", dir.display()))
}

/// Puts a file back as it was, or removes it if it didn't exist, when dropped, also when the test panics.
pub struct RestoreFile {
    path: PathBuf,
    was: Option<Vec<u8>>,
}

impl RestoreFile {
    pub fn new(path: &Path) -> RestoreFile {
        RestoreFile {
            path: path.to_path_buf(),
            was: fs::read(path).ok(),
        }
    }
}

impl Drop for RestoreFile {
    fn drop(&mut self) {
        match &self.was {
            Some(bytes) => {
                let _ = fs::write(&self.path, bytes);
            }
            None => {
                let _ = fs::remove_file(&self.path);
            }
        }
    }
}

/// Packages built with makepkg, and a file:// repository of them for `pacman -S`.
pub struct Packages {
    pub dir: TempDir,
}

impl Packages {
    pub fn new() -> Packages {
        if !Command::new("id")
            .arg("archix-build")
            .output()
            .unwrap()
            .status
            .success()
        {
            sh_out("useradd --system --no-create-home archix-build");
        }
        let dir = TempDir::new().unwrap();
        sh(&format!("chmod 755 {}", dir.path().display()));
        Packages { dir }
    }

    /// Builds `name` with `package` as more of its package() and adds it to the repo; returns its file.
    pub fn build(&self, name: &str, package: &str) -> PathBuf {
        self.build_version(name, "1", package)
    }

    /// Like `build`, at version `version`, which `package` can read as $pkgver.
    pub fn build_version(&self, name: &str, version: &str, package: &str) -> PathBuf {
        self.build_scripted(name, version, package, None)
    }

    /// Like `build_version`, with `install` as the package's install scriptlet.
    pub fn build_scripted(&self, name: &str, version: &str, package: &str, install: Option<&str>) -> PathBuf {
        let work = TempDir::new().unwrap();
        if let Some(script) = install {
            fs::write(work.path().join(format!("{name}.install")), script).unwrap();
        }
        let scriptlet = install.map_or(String::new(), |_| format!("install={name}.install\n"));
        fs::write(
            work.path().join("PKGBUILD"),
            format!(
                "pkgname={name}\npkgver={version}\npkgrel=1\narch=('any')\nlicense=('MIT')\noptions=('!strip' '!debug')\n{scriptlet}\
                 package() {{\n  install -d \"$pkgdir/usr/share/{name}\"\n  echo {version} > \"$pkgdir/usr/share/{name}/version\"\n  {package}\n}}\n"
            ),
        )
        .unwrap();
        sh_out(&format!(
            "chown -R archix-build: {w} && cd {w} && setpriv --reuid=archix-build --regid=archix-build --init-groups env HOME={w} PKGEXT=.pkg.tar.zst makepkg --noconfirm >/dev/null && cp {w}/{name}-{version}-1-any.pkg.tar.zst {d}/ && repo-add -q {d}/archix-hooks.db.tar.gz {d}/{name}-{version}-1-any.pkg.tar.zst",
            w = work.path().display(),
            d = self.dir.path().display()
        ));
        self.dir.path().join(format!("{name}-{version}-1-any.pkg.tar.zst"))
    }

    /// A pacman.conf on `machine` with only this repo, for `pacman -S`.
    pub fn conf(&self, machine: &Machine) -> PathBuf {
        let conf = machine.root.join("etc/pacman-hooks-test.conf");
        fs::write(
            &conf,
            format!(
                "[options]\nArchitecture = auto\n\n[archix-hooks]\nSigLevel = Optional TrustAll\nServer = file://{}\n",
                self.dir.path().display()
            ),
        )
        .unwrap();
        conf
    }
}

/// A package published to the local test AUR.
pub struct TestPkg<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub depends: &'a [&'a str],
    pub makedepends: &'a [&'a str],
    pub provides: &'a [&'a str],
    pub build: &'a str,
    pub package: &'a str,
    /// None for an orphaned package.
    pub maintainer: Option<&'a str>,
}

impl<'a> TestPkg<'a> {
    pub fn new(name: &'a str, version: &'a str) -> Self {
        TestPkg {
            name,
            version,
            depends: &[],
            makedepends: &[],
            provides: &[],
            build: "true",
            package: "",
            maintainer: Some("archix-tests"),
        }
    }
}

/// An AUR served from local bare git repos plus a metadata dump, so AUR tests don't depend on aur.archlinux.org.
pub struct LocalAur {
    dir: TempDir,
    published: Vec<(String, String, Vec<String>, Option<String>)>,
}

impl LocalAur {
    pub fn new() -> LocalAur {
        let dir = TempDir::new().unwrap();
        write_aur_dump(&dir.path().join("packages-meta-ext-v1.json.gz"), &[]);
        LocalAur {
            dir,
            published: Vec::new(),
        }
    }

    pub fn url(&self) -> String {
        format!("file://{}", self.dir.path().display())
    }

    /// The commit `back` commits before the newest of `name`'s recipe.
    pub fn commit(&self, name: &str, back: usize) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(self.dir.path().join(format!("{name}.git")))
            .args(["rev-parse", &format!("HEAD~{back}")])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(["-c", "user.name=test", "-c", "user.email=test@example.com", "-C"])
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    pub fn publish(&mut self, pkg: TestPkg) {
        let bare = self.dir.path().join(format!("{}.git", pkg.name));
        let work = self.dir.path().join("work").join(pkg.name);
        if !bare.exists() {
            fs::create_dir_all(&bare).unwrap();
            LocalAur::git(&bare, &["init", "--quiet", "--bare", "-b", "master"]);
            fs::create_dir_all(&work).unwrap();
            LocalAur::git(&work, &["init", "--quiet", "-b", "master"]);
            LocalAur::git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
        }
        let list = |items: &[&str]| items.iter().map(|i| format!("'{i}'")).collect::<Vec<_>>().join(" ");
        let pkgbuild = format!(
            "pkgname={name}\npkgver={version}\npkgrel=1\npkgdesc='archix test package {name}'\narch=('any')\n\
             license=('MIT')\ndepends=({depends})\nmakedepends=({makedepends})\nprovides=({provides})\n\
             build() {{\n  {build}\n}}\n\
             package() {{\n  install -d \"$pkgdir/usr/share/{name}\"\n  echo {version} > \"$pkgdir/usr/share/{name}/version\"\n  {package}\n}}\n",
            name = pkg.name,
            version = pkg.version,
            depends = list(pkg.depends),
            makedepends = list(pkg.makedepends),
            provides = list(pkg.provides),
            build = pkg.build,
            package = pkg.package,
        );
        let mut srcinfo = format!(
            "pkgbase = {name}\n\tpkgdesc = archix test package {name}\n\tpkgver = {version}\n\tpkgrel = 1\n\tarch = any\n\tlicense = MIT\n",
            name = pkg.name,
            version = pkg.version
        );
        for dep in pkg.makedepends {
            srcinfo.push_str(&format!("\tmakedepends = {dep}\n"));
        }
        for dep in pkg.depends {
            srcinfo.push_str(&format!("\tdepends = {dep}\n"));
        }
        for provided in pkg.provides {
            srcinfo.push_str(&format!("\tprovides = {provided}\n"));
        }
        srcinfo.push_str(&format!("\npkgname = {}\n", pkg.name));
        fs::write(work.join("PKGBUILD"), pkgbuild).unwrap();
        fs::write(work.join(".SRCINFO"), srcinfo).unwrap();
        LocalAur::git(&work, &["add", "."]);
        LocalAur::git(
            &work,
            &["commit", "--quiet", "-m", &format!("{} {}", pkg.name, pkg.version)],
        );
        LocalAur::git(&work, &["push", "--quiet", "origin", "master"]);

        self.published.retain(|(name, _, _, _)| name != pkg.name);
        self.published.push((
            pkg.name.to_string(),
            format!("{}-1", pkg.version),
            pkg.provides.iter().map(|p| p.to_string()).collect(),
            pkg.maintainer.map(str::to_string),
        ));
        let entries: Vec<serde_json::Value> = self
            .published
            .iter()
            .map(|(name, version, provides, maintainer)| {
                serde_json::json!({ "Name": name, "PackageBase": name, "Version": version, "Provides": provides, "Popularity": 0.0, "Maintainer": maintainer })
            })
            .collect();
        write_aur_dump(&self.dir.path().join("packages-meta-ext-v1.json.gz"), &entries);
    }
}

/// Writes an AUR metadata dump listing `entries` at `path`.
pub fn write_aur_dump(path: &Path, entries: &[serde_json::Value]) {
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(serde_json::to_string(entries).unwrap().as_bytes())
        .unwrap();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, gz.finish().unwrap()).unwrap();
}

/// A fresh random marker of `len` letters and digits, which can't be a leftover of another run.
pub fn marker(len: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    let mut bytes = vec![0u8; len];
    assert_eq!(
        unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), len, 0) },
        len as isize
    );
    bytes
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect()
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// Where a leak sweep found a value: the place and the marker's name.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub marker: String,
    pub place: String,
}

/// The leak sweep every secrets scenario ends with: each marker, raw and as base64, in every file under the machine's root but the secrets' ramfs, /proc, /sys and /dev (deployments, staging leftovers, /var with archix's state, blobs, generations, backups, the /etc merge's state and the package caches, the ESP with its UKIs' and initramfs images' compressed streams opened), the config but its .age files, `extra` directories (a package_cache_push target), the journal, every core dump and `outputs`, the archix commands' output; returns the hits.
pub fn leak_sweep(
    machine: &Machine,
    config: &Path,
    extra: &[&Path],
    markers: &[(String, Vec<u8>)],
    outputs: &[String],
) -> Vec<Hit> {
    use std::collections::BTreeSet;
    use std::os::unix::fs::MetadataExt;
    let needles: Vec<(String, Vec<Vec<u8>>)> = markers
        .iter()
        .map(|(name, value)| (name.clone(), vec![value.clone(), base64(value).into_bytes()]))
        .collect();
    let mut hits = Vec::new();
    let flat: Vec<&[u8]> = needles
        .iter()
        .flat_map(|(_, forms)| forms.iter().map(Vec::as_slice))
        .collect();
    let mut look = |bytes: &[u8], place: &str, deep: bool| {
        let inside = match deep {
            true => archix::secrets::scan::in_streams(bytes, &flat),
            false => vec![false; flat.len()],
        };
        for (i, (name, forms)) in needles.iter().enumerate() {
            let found = forms
                .iter()
                .enumerate()
                .any(|(j, form)| archix::secrets::memory::contains(bytes, form) || inside[i * 2 + j]);
            if found {
                hits.push(Hit {
                    marker: name.clone(),
                    place: place.to_string(),
                });
            }
        }
    };
    let mut seen: BTreeSet<(u64, u64)> = BTreeSet::new();
    let mut stack: Vec<(PathBuf, bool)> = vec![(machine.root.clone(), false), (config.to_path_buf(), false)];
    stack.extend(extra.iter().map(|dir| (dir.to_path_buf(), false)));
    stack.extend(["/var/lib/archix", "/var/cache/archix", "/var/cache/pacman/pkg"].map(|d| (PathBuf::from(d), false)));
    let skipped: Vec<PathBuf> = ["run/archix/secrets", "proc", "sys", "dev"]
        .iter()
        .map(|d| machine.root.join(d))
        .collect();
    while let Some((path, deep)) = stack.pop() {
        if skipped.contains(&path) {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !seen.insert((meta.dev(), meta.ino())) {
            continue;
        }
        let name = path.display().to_string();
        let deep = deep || name.contains("/efi") || name.ends_with("/boot") || name.contains("/boot/");
        if meta.is_dir() {
            for entry in fs::read_dir(&path).into_iter().flatten().flatten() {
                stack.push((entry.path(), deep));
            }
        } else if meta.file_type().is_symlink() {
            if let Ok(target) = fs::read_link(&path) {
                look(target.as_os_str().as_encoded_bytes(), &name, false);
            }
        } else if meta.is_file()
            && !(path.starts_with(config) && name.ends_with(".age"))
            && let Ok(bytes) = fs::read(&path)
        {
            look(&bytes, &name, deep);
        }
    }
    let journal = Command::new("journalctl")
        .args(["-o", "export", "--all", "--no-pager"])
        .output();
    if let Ok(journal) = journal {
        look(&journal.stdout, "the journal", false);
    }
    for entry in fs::read_dir("/var/lib/systemd/coredump")
        .into_iter()
        .flatten()
        .flatten()
    {
        if let Ok(bytes) = fs::read(entry.path()) {
            let plain = zstd::decode_all(&bytes[..]).unwrap_or_default();
            look(&bytes, &entry.path().display().to_string(), false);
            look(&plain, &entry.path().display().to_string(), false);
        }
    }
    for (i, output) in outputs.iter().enumerate() {
        look(
            output.as_bytes(),
            &format!("the output of archix command {}", i + 1),
            false,
        );
    }
    hits
}

/// Writes an executable file.
pub fn write_executable(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

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
        ln -s /var/lib/pacman/local "$t/pacman""#,
        archix = env!("CARGO_BIN_EXE_archix"),
    ));
    // The image's layers lost some owners and modes its packages give (setgid tty, setuid dbus), which `archix verify` would rightly report.
    for problem in archix::deploy::verify::check(Path::new(TEMPLATE)).unwrap() {
        let path = Path::new(TEMPLATE).join(&problem.path);
        if problem.what.starts_with("owner") || problem.what.starts_with("mode") {
            std::os::unix::fs::lchown(&path, Some(problem.uid as u32), Some(problem.gid as u32)).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(problem.mode)).unwrap();
        }
    }
    sh(&format!("rm {TEMPLATE}/pacman; touch {TEMPLATE}/.done"));
    PathBuf::from(TEMPLATE)
}

/// A whole Arch root in a directory on the container's /tmp, which archix switches through ARCHIX_TEST_ROOT: its /usr is hardlinked from the template, /etc and pacman's database are the container's, and the container's package caches and archix state are bound in, so the staging root sees package files where archix put them.
pub struct Machine {
    dir: Option<TempDir>,
    pub root: PathBuf,
    /// The boot entry LoaderEntrySelected names, as ARCHIX_TEST_ENTRY; None is a boot that set none.
    booted: RefCell<Option<String>>,
}

impl Machine {
    pub fn new() -> Machine {
        let template = template();
        let dir = tempfile::Builder::new()
            .prefix("machine-")
            .tempdir_in(MACHINES)
            .unwrap();
        let root = dir.path().join("root");
        sh(&format!(
            r#"r={root}; t={template}; umask 022
            mkdir -p "$r"; chmod 755 "$(dirname "$r")"
            cp -al "$t/usr" "$r/usr"
            cp -al "$t/target" "$r/target"
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
            cd /var/lib/archix && rm -rf generations boot backups edited users id-map.json id-registry.json etc-merge outside-changes.json \
              pending-hooks notes layout-pruned eject activated accounts"#,
            root = root.display(),
            template = template.display(),
        ));
        *ACTIVE.lock().unwrap() = Some(root.clone());
        Machine {
            dir: Some(dir),
            root,
            booted: RefCell::new(None),
        }
    }

    pub fn esp(&self) -> PathBuf {
        self.root.join("efi")
    }

    /// The environment that points archix at this machine and the boot it's in.
    pub fn env(&self) -> Vec<(String, String)> {
        vec![
            ("ARCHIX_TEST_ROOT".into(), self.root.display().to_string()),
            (
                "ARCHIX_TEST_ENTRY".into(),
                self.booted.borrow().clone().unwrap_or_default(),
            ),
        ]
    }

    pub fn current(&self) -> Option<u32> {
        let target = fs::read_link(self.root.join("archix/current")).ok()?;
        target.to_str()?.strip_prefix("deploy/")?.parse().ok()
    }

    pub fn deployment(&self, number: u32) -> PathBuf {
        self.root.join("archix/deploy").join(number.to_string())
    }

    /// Committed deployments, oldest first.
    pub fn deployments(&self) -> Vec<u32> {
        let mut numbers: Vec<u32> = fs::read_dir(self.root.join("archix/deploy"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
            .collect();
        numbers.sort_unstable();
        numbers
    }

    /// What's in the archix deploy directory besides committed deployments, such as staging trees.
    pub fn leftovers(&self) -> Vec<String> {
        fs::read_dir(self.root.join("archix/deploy"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.parse::<u32>().is_err())
            .collect()
    }

    /// A package's version in a deployment's pacman database.
    pub fn installed_in(&self, number: u32, name: &str) -> Option<String> {
        read_local_db(&self.deployment(number).join("pacman"))
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

    pub fn loader_conf(&self) -> String {
        fs::read_to_string(self.esp().join("loader/loader.conf")).unwrap_or_default()
    }

    /// Mount points at or under `path`, deepest first.
    fn mounts_under(path: &Path) -> Vec<PathBuf> {
        let info = fs::read("/proc/self/mountinfo").unwrap();
        let mut points: Vec<PathBuf> = archix::deploy::view::mount_points(&info)
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

    /// Restarts the machine into the boot entry `entry`, or into `current` without one: the deployment binds go, and the initramfs's `archix deploy-mount` binds the deployment it picks.
    pub fn reboot(&self, entry: Option<&str>) -> String {
        self.power_off();
        let efivar = self.root.join("run/test-efivar");
        match entry {
            Some(entry) => {
                let mut bytes = vec![6, 0, 0, 0];
                bytes.extend(entry.encode_utf16().flat_map(u16::to_le_bytes));
                bytes.extend([0, 0]);
                fs::write(&efivar, bytes).unwrap();
            }
            None => {
                let _ = fs::remove_file(&efivar);
            }
        }
        let output = Command::new(env!("CARGO_BIN_EXE_archix"))
            .arg("deploy-mount")
            .arg(&self.root)
            .arg("--efivar")
            .arg(&efivar)
            .arg("--run-dir")
            .arg(self.root.join("run/archix"))
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "deploy-mount failed:\n{text}");
        *self.booted.borrow_mut() = entry.map(str::to_string);
        text
    }

    /// Takes the running deployment's binds and the read-only store off the root, leaving it as a live USB mounts it.
    pub fn power_off(&self) {
        for (_, at) in archix::system::deploy_boot::TREES {
            Machine::unmount_all(&self.root.join(at));
        }
        let store = self.root.join("archix");
        while Machine::mounts_under(&store).contains(&store) {
            sh(&format!("umount {}", store.display()));
        }
    }

    /// Makes the machine's /archix, which its boot bound read-only, writable or read-only again, as a test that edits a deployment by hand needs.
    pub fn store_writable(&self, writable: bool) {
        let store = self.root.join("archix");
        if Machine::mounts_under(&store).contains(&store) {
            let mode = if writable { "rw" } else { "ro" };
            sh(&format!("mount -o remount,bind,{mode} {}", store.display()));
        }
    }

    /// Runs plain pacman against the machine's running deployment, as `pacman -U` or yay would there, with /usr, /opt and /boot made writable for it.
    pub fn pacman(&self, args: &[&str]) {
        let remount = |mode: &str| {
            for tree in archix::system::deploy_boot::READ_ONLY {
                let path = self.root.join(tree);
                if Machine::mounts_under(&path).contains(&path) {
                    let _ = Command::new("mount")
                        .args(["-o", &format!("remount,bind,{mode}")])
                        .arg(&path)
                        .status();
                }
            }
        };
        remount("rw");
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
        remount("ro");
        assert!(status.success(), "pacman {args:?}");
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        let mut active = ACTIVE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if active.as_deref() == Some(self.root.as_path()) {
            *active = None;
        }
        Machine::unmount_all(&self.root);
        let left = Machine::mounts_under(&self.root);
        if !left.is_empty() {
            eprintln!("warning: still mounted, so {} is left: {left:?}", self.root.display());
            std::mem::forget(self.dir.take());
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
        let machine = Machine::new();
        let mut paths = Paths::system();
        paths.root = machine.root.clone();
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

    /// Moves the config into the machine's running /etc/archix, as on a real machine, and runs archix with it there.
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

    /// The first switch, which lays the machine out as deployment 1 and builds deployment 2 if the config changes anything; the machine then boots the current deployment.
    pub fn lay_out(&self) -> String {
        let out = self.ok(&["switch", "--noconfirm"]);
        self.machine
            .reboot(Some(&format!("archix-{}.conf", self.machine.current().unwrap())));
        out
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

/// The version of `name` that boots next: in the active machine's current deployment, or before its first layout its own database.
pub fn installed(name: &str) -> Option<String> {
    let root = ACTIVE.lock().unwrap().clone().expect("no machine is active");
    let db = match fs::read_link(root.join("archix/current")) {
        Ok(target) => root.join("archix").join(target).join("pacman"),
        Err(_) => root.join("var/lib/pacman/local"),
    };
    read_local_db(&db).unwrap().remove(name).map(|pkg| pkg.version)
}

/// A file in the active machine's current deployment, or its root before the first layout, or for /usr/local the shared /var/usrlocal; `path` is absolute.
pub fn system_path(path: &str) -> PathBuf {
    let root = ACTIVE.lock().unwrap().clone().expect("no machine is active");
    let rel = path.trim_start_matches('/');
    if archix::system::usr_local::holds(path) {
        return archix::system::usr_local::on_disk(&root, path);
    }
    let booted = fs::read_to_string(root.join("run/archix/deployment")).unwrap_or_default();
    match fs::read_link(root.join("archix/current")) {
        Ok(target) if target.to_str() == Some(&format!("deploy/{}", booted.trim())) => root.join(rel),
        Ok(target) if archix::system::apply::deployment_tree(path).is_some() => {
            root.join("archix").join(target).join(rel)
        }
        _ => root.join(rel),
    }
}

/// Writes `path` of the active machine where `system_path` puts it, by hand as a user would, through /archix when it's in a deployment that isn't running.
pub fn write_system_path(path: &str, contents: &str) {
    let file = system_path(path);
    let root = ACTIVE.lock().unwrap().clone().expect("no machine is active");
    let store = root.join("archix");
    let protected = file.starts_with(&store) && Machine::mounts_under(&store).contains(&store);
    if protected {
        sh(&format!("mount -o remount,bind,rw {}", store.display()));
    }
    fs::write(&file, contents).unwrap();
    if protected {
        sh(&format!("mount -o remount,bind,ro {}", store.display()));
    }
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
        LocalAur {
            dir: TempDir::new().unwrap(),
            published: Vec::new(),
        }
    }

    pub fn url(&self) -> String {
        format!("file://{}", self.dir.path().display())
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
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(serde_json::to_string(&entries).unwrap().as_bytes())
            .unwrap();
        fs::write(
            self.dir.path().join("packages-meta-ext-v1.json.gz"),
            gz.finish().unwrap(),
        )
        .unwrap();
    }
}

/// Writes an executable file.
pub fn write_executable(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

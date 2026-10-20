//! Deployment staging, flattening, the transaction's namespaces and the commit order on loop-mounted ext4 and btrfs; run only inside the privileged Arch container via `tests/arch/run.sh`.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};

use archix::deploy::{Backend, OVERLAY_OPTIONS, Staging, Store, SystemdBoot, flatten, xattr};
use tempfile::TempDir;

static LOOPS: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    LOOPS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn sh(command: &mut Command) -> String {
    let output = command.output().unwrap_or_else(|e| panic!("{command:?}: {e}"));
    assert!(
        output.status.success(),
        "{command:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A small filesystem image on a loop device, mounted until dropped.
struct LoopFs {
    _dir: TempDir,
    dev: String,
    mnt: PathBuf,
}

impl LoopFs {
    fn new(fs: &str) -> LoopFs {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("fs.img");
        let size = if fs == "btrfs" { "160M" } else { "64M" };
        sh(Command::new("truncate").args(["-s", size]).arg(&img));
        let dev = sh(Command::new("losetup").args(["--show", "-f"]).arg(&img))
            .trim()
            .to_string();
        let mnt = dir.path().join("mnt");
        fs::create_dir(&mnt).unwrap();
        let loop_fs = LoopFs { _dir: dir, dev, mnt };
        sh(Command::new(format!("mkfs.{fs}")).arg("-q").arg(&loop_fs.dev));
        sh(Command::new("mount").arg(&loop_fs.dev).arg(&loop_fs.mnt));
        loop_fs
    }
}

impl Drop for LoopFs {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg("-R").arg(&self.mnt).status();
        let _ = Command::new("losetup").arg("-d").arg(&self.dev).status();
    }
}

/// Copies an ELF program and the libraries it loads into `usr`.
fn install_program(usr: &Path, program: &str, name: &str) {
    fs::create_dir_all(usr.join("bin")).unwrap();
    fs::create_dir_all(usr.join("lib")).unwrap();
    fs::copy(program, usr.join("bin").join(name)).unwrap();
    for line in sh(Command::new("ldd").arg(program)).lines() {
        let Some((needed, found)) = line.trim().split_once(" => ") else {
            continue;
        };
        let source = found.split(" (").next().unwrap().trim();
        let target = usr.join("lib").join(Path::new(needed.trim()).file_name().unwrap());
        if !target.exists() {
            fs::copy(source, target).unwrap();
        }
    }
}

/// A system root on a fresh filesystem whose deployment 1 is a busybox userland with setcap; returns the filesystem and the store.
fn system(fs_type: &str) -> (LoopFs, Store) {
    let loop_fs = LoopFs::new(fs_type);
    let root = loop_fs.mnt.clone();
    for (link, target) in [
        ("bin", "usr/bin"),
        ("sbin", "usr/bin"),
        ("lib", "usr/lib"),
        ("lib64", "usr/lib"),
    ] {
        std::os::unix::fs::symlink(target, root.join(link)).unwrap();
    }
    for (dir, mode) in [
        ("usr", 0o755),
        ("etc", 0o755),
        ("opt", 0o755),
        ("boot", 0o755),
        ("home", 0o755),
        ("srv", 0o755),
        ("root", 0o700),
        ("run", 0o755),
        ("tmp", 0o1777),
        ("proc", 0o555),
        ("sys", 0o555),
        ("dev", 0o755),
        ("efi", 0o755),
        ("var/log", 0o755),
        ("var/cache/pacman/pkg", 0o755),
        ("var/lib/pacman/local", 0o755),
        ("var/lib/dkms", 0o755),
        ("archix/deploy", 0o755),
    ] {
        fs::create_dir_all(root.join(dir)).unwrap();
        fs::set_permissions(root.join(dir), fs::Permissions::from_mode(mode)).unwrap();
    }
    fs::write(root.join("var/log/pacman.log"), "before\n").unwrap();
    fs::write(root.join("var/lib/oldstate"), "state\n").unwrap();
    let dep = root.join("archix/deploy/1");
    if fs_type == "btrfs" {
        sh(Command::new("btrfs").args(["-q", "subvolume", "create"]).arg(&dep));
    } else {
        fs::create_dir(&dep).unwrap();
    }
    for tree in ["usr", "etc", "opt", "boot", "pacman/base-1", "dkms", "var-upper"] {
        fs::create_dir_all(dep.join(tree)).unwrap();
    }
    let busybox = "/usr/lib/initcpio/busybox";
    install_program(&dep.join("usr"), busybox, "busybox");
    install_program(&dep.join("usr"), "/usr/bin/setcap", "setcap");
    for applet in sh(Command::new(busybox).arg("--list")).split_whitespace() {
        let link = dep.join("usr/bin").join(applet);
        if link.symlink_metadata().is_err() {
            std::os::unix::fs::symlink("busybox", link).unwrap();
        }
    }
    fs::create_dir_all(dep.join("usr/share/doc")).unwrap();
    fs::write(dep.join("usr/lib/data"), "original\n").unwrap();
    fs::write(dep.join("usr/share/doc/a"), "a\n").unwrap();
    fs::write(dep.join("usr/share/doc/b"), "b\n").unwrap();
    fs::copy(dep.join("usr/bin/busybox"), dep.join("usr/bin/tool")).unwrap();
    fs::write(dep.join("etc/hostname"), "deployment-1\n").unwrap();
    fs::write(dep.join("etc/passwd"), "root:x:0:0::/root:/bin/sh\n").unwrap();
    fs::write(dep.join("pacman/base-1/desc"), "%NAME%\nbase\n").unwrap();
    fs::write(dep.join("boot/archix.efi"), "uki-1").unwrap();
    std::os::unix::fs::symlink("deploy/1", root.join("archix/current")).unwrap();
    let esp = root.join("efi");
    fs::create_dir_all(esp.join("EFI/archix")).unwrap();
    fs::create_dir_all(esp.join("loader/entries")).unwrap();
    fs::write(esp.join("EFI/archix/1.efi"), "uki-1").unwrap();
    fs::write(
        esp.join("loader/entries/archix-1.conf"),
        "title archix deployment 1\nefi /EFI/archix/1.efi\n",
    )
    .unwrap();
    fs::write(esp.join("loader/loader.conf"), "timeout 2\ndefault archix-1.conf\n").unwrap();
    let store = Store::open(&root).unwrap();
    let expected = if fs_type == "btrfs" {
        Backend::Snapshot
    } else {
        Backend::Overlay
    };
    assert_eq!(store.backend(), expected);
    (loop_fs, store)
}

#[derive(Debug, PartialEq)]
struct Entry {
    mode: u32,
    uid: u32,
    gid: u32,
    mtime: (i64, i64),
    size: Option<u64>,
    sha256: Option<String>,
    link: Option<PathBuf>,
    xattrs: Vec<(String, Vec<u8>)>,
}

/// Every entry under `root` with what a merged view and a flattened tree must agree on; directory sizes and link counts differ by nature.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Entry> {
    let mut entries = BTreeMap::new();
    walk(root, Path::new(""), &mut entries);
    entries
}

fn walk(root: &Path, rel: &Path, out: &mut BTreeMap<PathBuf, Entry>) {
    let path = root.join(rel);
    let meta = fs::symlink_metadata(&path).unwrap();
    let kind = meta.file_type();
    let mut xattrs: Vec<(String, Vec<u8>)> = xattr::list(&path)
        .unwrap()
        .into_iter()
        .map(|name| {
            let value = xattr::get(&path, &name).unwrap().unwrap_or_default();
            (name.to_string_lossy().into_owned(), value)
        })
        .collect();
    xattrs.sort();
    out.insert(
        rel.to_path_buf(),
        Entry {
            mode: meta.mode(),
            uid: meta.uid(),
            gid: meta.gid(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            size: (!kind.is_dir()).then_some(meta.size()),
            sha256: kind
                .is_file()
                .then(|| archix::fetch::sha256_bytes(&fs::read(&path).unwrap())),
            link: kind.is_symlink().then(|| fs::read_link(&path).unwrap()),
            xattrs,
        },
    );
    if kind.is_dir() {
        for entry in fs::read_dir(&path).unwrap() {
            walk(root, &rel.join(entry.unwrap().file_name()), out);
        }
    }
}

/// Groups of two or more paths under `root` that share an inode.
fn hardlink_groups(root: &Path) -> Vec<Vec<PathBuf>> {
    let mut by_inode: BTreeMap<u64, Vec<PathBuf>> = BTreeMap::new();
    for rel in snapshot(root).into_keys() {
        let meta = fs::symlink_metadata(root.join(&rel)).unwrap();
        if !meta.is_dir() {
            by_inode.entry(meta.ino()).or_default().push(rel);
        }
    }
    let mut groups: Vec<Vec<PathBuf>> = by_inode.into_values().filter(|group| group.len() > 1).collect();
    groups.sort();
    groups
}

fn ino(path: &Path) -> u64 {
    fs::symlink_metadata(path).unwrap().ino()
}

/// A VFS v2 capability blob granting cap_net_raw, effective.
fn capability() -> Vec<u8> {
    let mut blob = Vec::new();
    for word in [0x0200_0001u32, 1 << 13, 0, 0, 0] {
        blob.extend_from_slice(&word.to_le_bytes());
    }
    blob
}

fn caps(path: &Path) -> Option<Vec<u8>> {
    xattr::get(path, c"security.capability").unwrap()
}

#[test]
#[ignore]
fn flattening_an_overlay_upper_gives_exactly_the_merged_view_without_copying_unchanged_files() {
    let _serial = serial();
    let loop_fs = LoopFs::new("ext4");
    let base = &loop_fs.mnt;
    let (lower, upper, work, merged, out) = (
        base.join("lower"),
        base.join("upper"),
        base.join("work"),
        base.join("merged"),
        base.join("out"),
    );
    for dir in [
        "f",
        "bin",
        "d/gone/sub",
        "d/opaque",
        "d/becomes-file",
        "d/chmoddir",
        "d/keep",
        "l",
        "empty",
    ] {
        fs::create_dir_all(lower.join(dir)).unwrap();
    }
    let file = |rel: &str, content: &str, mode: u32| {
        fs::write(lower.join(rel), content).unwrap();
        fs::set_permissions(lower.join(rel), fs::Permissions::from_mode(mode)).unwrap();
    };
    for name in [
        "plain",
        "append",
        "chmod",
        "chown",
        "gone",
        "becomes-dir",
        "old-name",
        "recreated",
        "xattr-new",
        "with-xattr",
        "link-a",
    ] {
        file(&format!("f/{name}"), &format!("{name}\n"), 0o644);
    }
    file("f/exec", "#!/bin/sh\n", 0o755);
    file("f/setuid", "suid\n", 0o4755);
    file("f/owned", "mine\n", 0o600);
    std::os::unix::fs::chown(lower.join("f/owned"), Some(1000), Some(1000)).unwrap();
    fs::hard_link(lower.join("f/link-a"), lower.join("f/link-b")).unwrap();
    xattr::set(&lower.join("f/with-xattr"), c"user.k", b"v").unwrap();
    let fifo = std::ffi::CString::new(lower.join("f/fifo").into_os_string().into_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o640) }, 0);
    file("bin/capped", "tool\n", 0o755);
    file("bin/had-cap", "tool\n", 0o755);
    xattr::set(&lower.join("bin/had-cap"), c"security.capability", &capability()).unwrap();
    for rel in [
        "d/gone/x",
        "d/gone/sub/y",
        "d/opaque/old1",
        "d/opaque/old2",
        "d/becomes-file/z",
        "d/chmoddir/c1",
        "d/keep/k",
    ] {
        file(rel, rel, 0o644);
    }
    fs::set_permissions(lower.join("d/keep"), fs::Permissions::from_mode(0o750)).unwrap();
    std::os::unix::fs::chown(lower.join("d/keep"), Some(1000), Some(1000)).unwrap();
    std::os::unix::fs::symlink("../f/plain", lower.join("l/link")).unwrap();
    std::os::unix::fs::symlink("nowhere", lower.join("l/dangling")).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", lower.join("l/abs")).unwrap();
    for dir in [&upper, &work, &merged] {
        fs::create_dir(dir).unwrap();
    }
    let lower_root = fs::metadata(&lower).unwrap();
    fs::set_permissions(&upper, fs::Permissions::from_mode(lower_root.mode() & 0o7777)).unwrap();
    flatten::set_times(&upper, &lower_root).unwrap();
    let options = format!(
        "lowerdir={},upperdir={},workdir={},{OVERLAY_OPTIONS}",
        lower.display(),
        upper.display(),
        work.display()
    );
    let before = snapshot(&lower);
    sh(Command::new("mount")
        .args(["-t", "overlay", "overlay", "-o", &options])
        .arg(&merged));

    let m = |rel: &str| merged.join(rel);
    fs::OpenOptions::new()
        .append(true)
        .open(m("f/append"))
        .unwrap()
        .write_all(b"more\n")
        .unwrap();
    fs::set_permissions(m("f/chmod"), fs::Permissions::from_mode(0o600)).unwrap();
    std::os::unix::fs::chown(m("f/chown"), Some(1000), Some(1000)).unwrap();
    xattr::set(&m("bin/capped"), c"security.capability", &capability()).unwrap();
    xattr::set(&m("f/xattr-new"), c"user.added", b"yes").unwrap();
    fs::remove_file(m("f/gone")).unwrap();
    fs::remove_dir_all(m("d/gone")).unwrap();
    fs::remove_dir_all(m("d/opaque")).unwrap();
    fs::create_dir(m("d/opaque")).unwrap();
    fs::write(m("d/opaque/fresh"), "fresh\n").unwrap();
    fs::remove_file(m("f/becomes-dir")).unwrap();
    fs::create_dir(m("f/becomes-dir")).unwrap();
    fs::write(m("f/becomes-dir/inside"), "inside\n").unwrap();
    fs::remove_dir_all(m("d/becomes-file")).unwrap();
    fs::write(m("d/becomes-file"), "now a file\n").unwrap();
    fs::remove_file(m("l/link")).unwrap();
    std::os::unix::fs::symlink("../f/exec", m("l/link-new")).unwrap();
    fs::create_dir(m("new")).unwrap();
    fs::write(m("new/a"), "new\n").unwrap();
    fs::hard_link(m("new/a"), m("new/b")).unwrap();
    fs::rename(m("f/old-name"), m("f/new-name")).unwrap();
    fs::create_dir(m("d/private")).unwrap();
    fs::set_permissions(m("d/private"), fs::Permissions::from_mode(0o700)).unwrap();
    std::os::unix::fs::chown(m("d/private"), Some(1000), Some(1000)).unwrap();
    fs::set_permissions(m("d/chmoddir"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_file(m("f/recreated")).unwrap();
    fs::write(m("f/recreated"), "again\n").unwrap();

    let view = snapshot(&merged);
    let view_links = hardlink_groups(&merged);
    sh(Command::new("umount").arg(&merged));
    flatten::flatten(&lower, &upper, &out).unwrap();
    let flat = snapshot(&out);

    let paths: std::collections::BTreeSet<&PathBuf> = view.keys().chain(flat.keys()).collect();
    let differing: Vec<_> = paths
        .into_iter()
        .filter(|rel| view.get(*rel) != flat.get(*rel))
        .map(|rel| {
            format!(
                "{}: merged {:?}, flattened {:?}",
                rel.display(),
                view.get(rel),
                flat.get(rel)
            )
        })
        .collect();
    assert!(differing.is_empty(), "{}", differing.join("\n"));
    assert_eq!(hardlink_groups(&out), view_links);
    for unchanged in [
        "f/plain",
        "f/exec",
        "f/setuid",
        "f/owned",
        "f/with-xattr",
        "f/link-a",
        "f/link-b",
        "f/fifo",
        "bin/had-cap",
        "d/keep/k",
        "d/chmoddir/c1",
        "l/dangling",
        "l/abs",
    ] {
        assert_eq!(
            ino(&out.join(unchanged)),
            ino(&lower.join(unchanged)),
            "{unchanged} is shared"
        );
    }
    for changed in ["f/append", "f/chmod", "f/chown", "bin/capped", "f/xattr-new"] {
        assert_ne!(
            ino(&out.join(changed)),
            ino(&lower.join(changed)),
            "{changed} is a copy"
        );
    }
    assert_eq!(snapshot(&lower), before, "the lower layer is untouched");
    assert_eq!(caps(&out.join("bin/capped")), Some(capability()));
    assert_eq!(caps(&lower.join("bin/capped")), None);
}

#[test]
#[ignore]
fn in_place_writes_and_setcap_in_staging_leave_the_previous_deployment_untouched() {
    let _serial = serial();
    for fs_type in ["ext4", "btrfs"] {
        let (_fs, store) = system(fs_type);
        let old = store.path(1);
        let before = snapshot(&old);
        let inodes: BTreeMap<PathBuf, u64> = before.keys().map(|rel| (rel.clone(), ino(&old.join(rel)))).collect();
        let staging = Staging::new(&store, 2).unwrap();
        let tx = staging.transaction().unwrap();
        tx.run(&[
            "sh",
            "-ec",
            "echo more >> /usr/lib/data; setcap cap_net_raw+ep /usr/bin/tool; chmod 700 /usr/share/doc/a; \
             rm /usr/share/doc/b; echo deployment-2 > /etc/hostname; echo x >> /var/lib/pacman/local/base-1/desc; touch -c /usr",
        ])
        .unwrap();
        let new = staging.finish().unwrap().dir;

        assert_eq!(snapshot(&old), before, "{fs_type}: deployment 1 changed");
        for (rel, inode) in &inodes {
            assert_eq!(ino(&old.join(rel)), *inode, "{fs_type}: {} was replaced", rel.display());
        }
        assert_eq!(
            fs::read_to_string(new.join("usr/lib/data")).unwrap(),
            "original\nmore\n"
        );
        assert_eq!(caps(&new.join("usr/bin/tool")), Some(capability()), "{fs_type}");
        assert_eq!(caps(&old.join("usr/bin/tool")), None, "{fs_type}");
        assert_eq!(
            fs::metadata(new.join("usr/share/doc/a")).unwrap().mode() & 0o7777,
            0o700
        );
        assert!(!new.join("usr/share/doc/b").exists());
        assert_eq!(fs::read_to_string(new.join("etc/hostname")).unwrap(), "deployment-2\n");
        assert!(
            fs::read_to_string(new.join("pacman/base-1/desc"))
                .unwrap()
                .ends_with("x\n")
        );
        assert!(
            fs::metadata(new.join("usr")).unwrap().mtime() >= fs::metadata(new.join("usr/lib/data")).unwrap().mtime(),
            "{fs_type}: /usr keeps the time `touch -c /usr` gave it"
        );
        if fs_type == "ext4" {
            assert_eq!(ino(&new.join("usr/lib/libc.so.6")), ino(&old.join("usr/lib/libc.so.6")));
            assert_eq!(ino(&new.join("usr/bin/sh")), ino(&old.join("usr/bin/sh")));
        }
    }
}

fn mountinfo_mentions(path: &Path) -> bool {
    fs::read_to_string("/proc/self/mountinfo")
        .unwrap()
        .contains(&*path.to_string_lossy())
}

#[test]
#[ignore]
fn the_transaction_writes_nothing_live_but_the_pacman_log_and_package_cache() {
    let _serial = serial();
    for fs_type in ["ext4", "btrfs"] {
        let (_fs, store) = system(fs_type);
        let root = store.root().to_path_buf();
        let host_shutdown = fs::read("/run/initramfs/mkinitcpio-shutdown.conf").ok();
        let host_null = fs::metadata("/dev/null").unwrap().mode();
        let staging = Staging::new(&store, 2).unwrap();
        let tx = staging.transaction().unwrap();
        let script = "test ! -e /archix; test -e /proc/self/exe; test -c /dev/null; \
            mkdir -p /run/initramfs; echo staged > /run/initramfs/mkinitcpio-shutdown.conf; \
            touch /run/archix-probe /home/probe /srv/probe /root/probe /probe /tmp/probe; \
            echo staged-line >> /var/log/pacman.log; echo pkg > /var/cache/pacman/pkg/probe.pkg.tar.zst; \
            mkdir /var/lib/pacman/local/probe-1; echo desc > /var/lib/pacman/local/probe-1/desc; \
            echo built > /var/lib/dkms/probe; mkdir -p /var/lib/newstate; echo s > /var/lib/newstate/f; \
            rm /var/lib/oldstate; echo kernel > /boot/vmlinuz-probe; chmod 600 /dev/null";
        let output = tx.output(&["sh", "-ec", script]).unwrap();
        assert!(
            output.status.success(),
            "{fs_type}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!mountinfo_mentions(staging.dir()), "{fs_type}: a view mount leaked");
        let staged = staging.finish().unwrap().dir;

        assert_eq!(fs::read("/run/initramfs/mkinitcpio-shutdown.conf").ok(), host_shutdown);
        assert!(!Path::new("/run/archix-probe").exists());
        assert!(!Path::new("/probe").exists());
        assert_eq!(fs::metadata("/dev/null").unwrap().mode(), host_null);
        for live in [
            "home/probe",
            "srv/probe",
            "root/probe",
            "probe",
            "tmp/probe",
            "run/archix-probe",
            "boot/vmlinuz-probe",
        ] {
            assert!(!root.join(live).exists(), "{fs_type}: the live /{live} was written");
        }
        assert_eq!(
            fs::read_to_string(root.join("var/log/pacman.log")).unwrap(),
            "before\nstaged-line\n"
        );
        assert!(root.join("var/cache/pacman/pkg/probe.pkg.tar.zst").exists());
        assert!(
            !root.join("var/lib/newstate").exists(),
            "{fs_type}: /var changes wait for activation"
        );
        assert!(root.join("var/lib/oldstate").exists());
        assert!(!root.join("var/lib/pacman/local/probe-1").exists());
        assert!(!root.join("var/lib/dkms/probe").exists());

        assert_eq!(
            fs::read_to_string(staged.join("pacman/probe-1/desc")).unwrap(),
            "desc\n"
        );
        assert_eq!(fs::read_to_string(staged.join("dkms/probe")).unwrap(), "built\n");
        assert_eq!(
            fs::read_to_string(staged.join("boot/vmlinuz-probe")).unwrap(),
            "kernel\n"
        );
        let upper = staged.join("var-upper");
        assert_eq!(fs::read_to_string(upper.join("lib/newstate/f")).unwrap(), "s\n");
        let whiteout = fs::symlink_metadata(upper.join("lib/oldstate")).unwrap();
        assert!(whiteout.file_type().is_char_device() && whiteout.rdev() == 0);
        for bound in ["log/pacman.log", "cache/pacman/pkg", "lib/pacman/local", "lib/dkms"] {
            assert!(
                upper.join(bound).symlink_metadata().is_err(),
                "{fs_type}: var-upper carries {bound}"
            );
        }
        assert!(!store.path(1).join("boot/vmlinuz-probe").exists());
    }
}

/// Processes whose command line has `marker`, as the host sees them.
fn processes_with(marker: &str) -> Vec<String> {
    fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|entry| fs::read(entry.path().join("cmdline")).ok())
        .map(|cmdline| String::from_utf8_lossy(&cmdline).replace('\0', " "))
        .filter(|cmdline| cmdline.contains(marker))
        .collect()
}

fn wait_until(what: &str, done: impl Fn() -> bool) {
    for _ in 0..100 {
        if done() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("timed out waiting until {what}");
}

/// Runs the helper below in a child process of this test binary, which `ARCHIX_TEST_CRASH` can kill at a step.
fn helper(action: &str, root: &Path, env: &[(&str, &str)]) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "deploy_helper",
            "--exact",
            "--include-ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("ARCHIX_DEPLOY_HELPER", action)
        .env("ARCHIX_DEPLOY_ROOT", root)
        .env_remove("ARCHIX_TEST_CRASH");
    for (name, value) in env {
        command.env(name, value);
    }
    command
}

#[test]
#[ignore]
fn deploy_helper() {
    let Ok(action) = std::env::var("ARCHIX_DEPLOY_HELPER") else {
        return;
    };
    let root = PathBuf::from(std::env::var("ARCHIX_DEPLOY_ROOT").unwrap());
    let store = Store::open(&root).unwrap();
    let number = store.next_number().unwrap();
    let staging = Staging::new(&store, number).unwrap();
    let tx = staging.transaction().unwrap();
    match action.as_str() {
        "sleep" => tx.run(&["sleep", "98762"]).unwrap(),
        "switch" => {
            tx.run(&["sh", "-ec", &format!("printf uki-{number} > /boot/archix.efi")])
                .unwrap();
            let esp = root.join("efi");
            let boot = SystemdBoot {
                esp: esp.clone(),
                uki: PathBuf::from("boot/archix.efi"),
            };
            staging.finish().unwrap().commit(&store, &esp, &boot).unwrap();
        }
        other => panic!("unknown helper action {other}"),
    }
}

#[test]
#[ignore]
fn nothing_the_transaction_starts_outlives_it_or_archix() {
    let _serial = serial();
    let (_fs, store) = system("ext4");
    let staging = Staging::new(&store, 2).unwrap();
    let tx = staging.transaction().unwrap();
    tx.run(&[
        "sh",
        "-c",
        "sleep 98761 & (sleep 98761 &); (sh -c 'sleep 98761 &') & exit 0",
    ])
    .unwrap();
    assert_eq!(processes_with("98761"), Vec::<String>::new());
    drop(staging);

    let mut child = helper("sleep", store.root(), &[]).spawn().unwrap();
    wait_until("the transaction runs", || !processes_with("sleep 98762").is_empty());
    child.kill().unwrap();
    child.wait().unwrap();
    wait_until("the transaction is gone", || processes_with("sleep 98762").is_empty());
    assert!(!mountinfo_mentions(&store.deploy_dir()), "a view mount outlived archix");
}

/// The boot entries on the ESP by deployment number, and the number loader.conf's default names.
fn esp_state(esp: &Path) -> (Vec<u32>, Option<u32>) {
    let mut entries: Vec<u32> = fs::read_dir(esp.join("loader/entries"))
        .unwrap()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.strip_prefix("archix-")?.strip_suffix(".conf")?.parse().ok()
        })
        .collect();
    entries.sort_unstable();
    let conf = fs::read_to_string(esp.join("loader/loader.conf")).unwrap();
    let default = conf.lines().find_map(|line| {
        line.strip_prefix("default archix-")?
            .strip_suffix(".conf")?
            .parse()
            .ok()
    });
    (entries, default)
}

/// What must hold after any crash: `current`, every entry and the default name complete deployments whose UKI is in place.
fn assert_consistent(store: &Store, what: &str) {
    let committed = store.deployments().unwrap();
    let complete = |n: u32| {
        archix::deploy::TREES
            .iter()
            .all(|tree| store.path(n).join(tree).is_dir())
            && fs::read_to_string(store.path(n).join("boot/archix.efi")).unwrap() == format!("uki-{n}")
    };
    let current = store.current().unwrap().unwrap();
    assert!(
        committed.contains(&current) && complete(current),
        "{what}: current {current}"
    );
    let esp = store.root().join("efi");
    let (entries, default) = esp_state(&esp);
    for n in &entries {
        assert!(committed.contains(n) && complete(*n), "{what}: entry {n}");
        assert_eq!(
            fs::read_to_string(esp.join(format!("EFI/archix/{n}.efi"))).unwrap(),
            format!("uki-{n}")
        );
    }
    let default = default.unwrap_or_else(|| panic!("{what}: loader.conf has no default"));
    assert!(entries.contains(&default), "{what}: default {default}");
}

#[test]
#[ignore]
fn a_kill_at_any_switch_step_leaves_only_complete_deployments_named_and_the_next_switch_needs_no_repair() {
    let _serial = serial();
    let steps = [
        ("deploy-stage", 1),
        ("deploy-transact", 1),
        ("deploy-flatten", 1),
        ("deploy-commit-sync", 1),
        ("deploy-commit-rename", 1),
        ("deploy-commit-esp", 1),
        ("deploy-esp-uki", 1),
        ("deploy-esp-entry", 1),
        ("deploy-esp-loader", 1),
        ("deploy-commit-current", 1),
        ("deploy-done", 2),
    ];
    for fs_type in ["ext4", "btrfs"] {
        let (_fs, store) = system(fs_type);
        for (step, current) in steps {
            let what = format!("{fs_type}, killed at {step}");
            let status = helper("switch", store.root(), &[("ARCHIX_TEST_CRASH", step)])
                .output()
                .unwrap();
            assert_eq!(
                std::os::unix::process::ExitStatusExt::signal(&status.status),
                Some(libc::SIGKILL),
                "{what}: {}",
                String::from_utf8_lossy(&status.stderr)
            );
            assert!(!mountinfo_mentions(&store.deploy_dir()), "{what}: a view mount leaked");
            assert_consistent(&store, &what);
            assert_eq!(store.current().unwrap(), Some(current), "{what}");

            let next = store.next_number().unwrap();
            let status = helper("switch", store.root(), &[]).output().unwrap();
            assert!(
                status.status.success(),
                "{what}, then a switch: {}",
                String::from_utf8_lossy(&status.stderr)
            );
            assert_consistent(&store, &format!("{what}, then a switch"));
            assert_eq!(store.current().unwrap(), Some(next), "{what}, then a switch");
            let names: Vec<String> = fs::read_dir(store.deploy_dir())
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            assert!(!names.iter().any(|name| name.starts_with('.')), "{what}: {names:?}");

            store.point_current(1).unwrap();
            for n in store.deployments().unwrap().into_iter().filter(|n| *n != 1) {
                store.delete(n).unwrap();
            }
            let esp = store.root().join("efi");
            for entry in fs::read_dir(esp.join("loader/entries")).unwrap().flatten() {
                if entry.file_name() != "archix-1.conf" {
                    fs::remove_file(entry.path()).unwrap();
                }
            }
            fs::write(esp.join("loader/loader.conf"), "timeout 2\ndefault archix-1.conf\n").unwrap();
        }
    }
}

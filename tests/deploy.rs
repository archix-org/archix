//! Deployment staging, flattening, the transaction's namespaces and the commit order on loop-mounted ext4 and btrfs; run only inside the privileged Arch container via `tests/arch/run.sh`.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};

use archix::deploy::{Backend, Keep, OVERLAY_OPTIONS, Staging, Store, SystemdBoot, flatten, xattr};
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
        LoopFs::sized(fs, if fs == "btrfs" { "160M" } else { "64M" })
    }

    fn sized(fs: &str, size: &str) -> LoopFs {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("fs.img");
        sh(Command::new("truncate").args(["-s", size]).arg(&img));
        let dev = sh(Command::new("losetup").args(["--show", "-f"]).arg(&img))
            .trim()
            .to_string();
        let mnt = dir.path().join("mnt");
        fs::create_dir(&mnt).unwrap();
        let loop_fs = LoopFs { _dir: dir, dev, mnt };
        let mut mkfs = Command::new(format!("mkfs.{fs}"));
        if fs != "vfat" {
            mkfs.arg("-q");
        }
        sh(mkfs.arg(&loop_fs.dev));
        sh(Command::new("mount").arg(&loop_fs.dev).arg(&loop_fs.mnt));
        loop_fs
    }
}

/// A vfat ESP on a device-mapper target over a loop device, which `fail` turns into one where every I/O errors.
struct FailingEsp {
    _dir: TempDir,
    dev: String,
    name: String,
    sectors: String,
    mnt: PathBuf,
}

impl FailingEsp {
    fn new() -> FailingEsp {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("esp.img");
        sh(Command::new("truncate").args(["-s", "32M"]).arg(&img));
        let dev = sh(Command::new("losetup").args(["--show", "-f"]).arg(&img))
            .trim()
            .to_string();
        let sectors = sh(Command::new("blockdev").arg("--getsz").arg(&dev)).trim().to_string();
        let name = format!("archix-test-esp-{}", std::process::id());
        let mnt = dir.path().join("mnt");
        fs::create_dir(&mnt).unwrap();
        let esp = FailingEsp {
            _dir: dir,
            dev,
            name,
            sectors,
            mnt,
        };
        let table = format!("0 {} linear {} 0", esp.sectors, esp.dev);
        sh(Command::new("dmsetup").args(["create", &esp.name, "--table", &table]));
        sh(Command::new("dmsetup").arg("mknodes").arg(&esp.name));
        sh(Command::new("mkfs.vfat").arg(esp.mapper()));
        sh(Command::new("mount").arg(esp.mapper()).arg(&esp.mnt));
        esp
    }

    fn mapper(&self) -> String {
        format!("/dev/mapper/{}", self.name)
    }

    fn fail(&self) {
        let table = format!("0 {} error", self.sectors);
        sh(Command::new("dmsetup").args(["suspend", "--nolockfs", &self.name]));
        sh(Command::new("dmsetup").args(["load", &self.name, "--table", &table]));
        sh(Command::new("dmsetup").args(["resume", &self.name]));
    }
}

impl Drop for FailingEsp {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg(&self.mnt).status();
        let _ = Command::new("dmsetup").args(["remove", &self.name]).status();
        let _ = Command::new("losetup").arg("-d").arg(&self.dev).status();
    }
}

/// An ESP holding deployment 1's entry, as `system` makes one.
fn esp_with_deployment_1(esp: &Path) {
    fs::create_dir_all(esp.join("EFI/archix")).unwrap();
    fs::create_dir_all(esp.join("loader/entries")).unwrap();
    fs::write(esp.join("EFI/archix/1.efi"), "uki-1").unwrap();
    fs::write(
        esp.join("loader/entries/archix-1.conf"),
        "title archix deployment 1\nefi /EFI/archix/1.efi\n",
    )
    .unwrap();
    fs::write(esp.join("loader/loader.conf"), "timeout 2\ndefault archix-1.conf\n").unwrap();
}

/// Files under `dir` whose name ends in .tmp.
fn tmp_files(dir: &Path) -> Vec<PathBuf> {
    snapshot(dir)
        .into_keys()
        .filter(|rel| rel.to_string_lossy().ends_with(".tmp"))
        .collect()
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
        let target = match Path::new(source).strip_prefix("/usr/lib") {
            Ok(rel) if rel.components().count() > 1 => usr.join("lib").join(rel),
            _ => usr.join("lib").join(Path::new(needed.trim()).file_name().unwrap()),
        };
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        if !target.exists() {
            fs::copy(source, target).unwrap();
        }
    }
}

/// A system root on a fresh filesystem whose deployment 1 is a busybox userland with setcap; returns the filesystem and the store.
fn system(fs_type: &str) -> (LoopFs, Store) {
    system_on(LoopFs::new(fs_type), fs_type)
}

fn system_on(loop_fs: LoopFs, fs_type: &str) -> (LoopFs, Store) {
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
    esp_with_deployment_1(&root.join("efi"));
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

/// The classic chroot escape: chroot into a subdirectory, climb out with `..`, chroot there and list /; it only stays in the view if the view was pivoted into, not chrooted.
const ESCAPE: &str = r#"
#include <dirent.h>
#include <stdio.h>
#include <sys/stat.h>
#include <unistd.h>
int main(void) {
    mkdir("/tmp/escape", 0700);
    if (chroot("/tmp/escape") != 0) return 2;
    for (int i = 0; i < 64; i++) if (chdir("..") != 0) return 3;
    if (chroot(".") != 0) return 4;
    DIR *dir = opendir("/");
    if (!dir) return 5;
    for (struct dirent *e; (e = readdir(dir));) if (e->d_name[0] != '.') puts(e->d_name);
    return 0;
}
"#;

fn install_escape(target: &Path) {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("escape.c"), ESCAPE).unwrap();
    sh(Command::new("cc")
        .args(["-static", "-O", "-o"])
        .arg(target)
        .arg(dir.path().join("escape.c")));
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
        install_escape(&store.path(1).join("usr/bin/escape"));
        let staging = Staging::new(&store, 2).unwrap();
        let tx = staging.transaction().unwrap();
        let listing = |argv: &[&str]| {
            let output = tx.output(argv).unwrap();
            assert!(
                output.status.success(),
                "{fs_type}: {argv:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let mut names: Vec<String> = String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .map(str::to_string)
                .collect();
            names.sort();
            names
        };
        let top: Vec<String> = [
            "bin", "boot", "dev", "efi", "etc", "home", "lib", "lib64", "opt", "proc", "root", "run", "sbin", "srv",
            "sys", "tmp", "usr", "var",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(listing(&["ls", "-A", "/"]), top, "{fs_type}: /");
        assert_eq!(listing(&["ls", "-A", "/proc/1/root/"]), top, "{fs_type}: PID 1's root");
        assert_eq!(
            listing(&["escape"]),
            top,
            "{fs_type}: a chroot escape got out of the view"
        );
        for live in ["/home", "/srv", "/root", "/efi"] {
            assert!(listing(&["ls", "-A", live]).is_empty(), "{fs_type}: {live} isn't empty");
        }
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
    if action == "delete" {
        let number = std::env::var("ARCHIX_DEPLOY_NUMBER").unwrap().parse().unwrap();
        let pins = Keep::read(1, &store, &root.join("efi")).unwrap();
        store.delete(number, &pins).unwrap();
        return;
    }
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
                store.delete(n, &Keep::default()).unwrap();
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

fn pids_with(marker: &str) -> Vec<libc::pid_t> {
    fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
        .filter(|pid: &libc::pid_t| {
            fs::read(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|cmdline| String::from_utf8_lossy(&cmdline).contains(marker))
        })
        .collect()
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
#[ignore]
fn output_returns_megabytes_written_to_both_streams() {
    let _serial = serial();
    let (_fs, store) = system("ext4");
    let staging = Staging::new(&store, 2).unwrap();
    let tx = staging.transaction().unwrap();
    let (done, finished) = std::sync::mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if finished.recv_timeout(std::time::Duration::from_secs(60)).is_ok() {
            return false;
        }
        for pid in pids_with("98763") {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        true
    });
    let output = tx
        .output(&[
            "sh",
            "-c",
            ": 98763; head -c 4000000 /dev/zero; head -c 4000000 /dev/zero >&2",
        ])
        .unwrap();
    let _ = done.send(());
    assert!(!watchdog.join().unwrap(), "output() deadlocked on a full pipe");
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(output.stdout.len(), 4_000_000);
    assert_eq!(output.stderr.len(), 4_000_000);
}

#[test]
#[ignore]
fn the_command_is_not_pid_1_and_sigterm_reaches_it() {
    let _serial = serial();
    let (_fs, store) = system("ext4");
    let staging = Staging::new(&store, 2).unwrap();
    let tx = staging.transaction().unwrap();
    let mut child = tx
        .command("sh")
        .args(["-c", "trap 'exit 7' TERM; echo $$; while :; do sleep 1; done"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(child.stdout.take().unwrap()), &mut line).unwrap();
    assert_ne!(line.trim(), "1", "the command runs as PID 1");
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(7), "the command didn't get SIGTERM: {status:?}");
}

#[test]
#[ignore]
fn a_second_switch_cannot_stage_while_one_is_staging() {
    let _serial = serial();
    let (_fs, store) = system("ext4");
    let first = Staging::new(&store, 2).unwrap();
    let err = Staging::new(&store, 3).unwrap_err();
    assert!(format!("{err:#}").contains("another archix process"), "{err:#}");
    assert!(
        first.dir().is_dir(),
        "the second switch swept the first one's staging tree"
    );
    drop(first);
    Staging::new(&store, 3).unwrap();
}

#[test]
#[ignore]
fn a_delete_killed_after_the_rename_or_partway_leaves_no_torn_deployment_named() {
    let _serial = serial();
    let cases: [(&str, &[&str]); 2] = [
        ("ext4", &["deploy-delete-renamed", "deploy-delete-partway"]),
        ("btrfs", &["deploy-delete-renamed"]),
    ];
    for (fs_type, steps) in cases {
        let (_fs, store) = system(fs_type);
        let esp = store.root().join("efi");
        for step in steps {
            let what = format!("{fs_type}, killed at {step}");
            let switch = helper("switch", store.root(), &[]).output().unwrap();
            assert!(switch.status.success(), "{what}: {}", stderr(&switch));
            let number = store.current().unwrap().unwrap();
            store.point_current(1).unwrap();
            fs::remove_file(esp.join(format!("loader/entries/archix-{number}.conf"))).unwrap();
            fs::remove_file(esp.join(format!("EFI/archix/{number}.efi"))).unwrap();
            fs::write(esp.join("loader/loader.conf"), "timeout 2\ndefault archix-1.conf\n").unwrap();
            let delete = helper(
                "delete",
                store.root(),
                &[
                    ("ARCHIX_TEST_CRASH", step),
                    ("ARCHIX_DEPLOY_NUMBER", &number.to_string()),
                ],
            )
            .output()
            .unwrap();
            assert_eq!(
                std::os::unix::process::ExitStatusExt::signal(&delete.status),
                Some(libc::SIGKILL),
                "{what}: {}",
                stderr(&delete)
            );
            assert_eq!(store.deployments().unwrap(), vec![1], "{what}");
            assert_consistent(&store, &what);
            let visible: Vec<String> = fs::read_dir(store.deploy_dir())
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| !name.starts_with('.'))
                .collect();
            assert_eq!(visible, vec!["1"], "{what}: a torn deployment kept its name");
            assert_eq!(
                store.remove_leftovers().unwrap(),
                vec![format!(".deleting-{number}")],
                "{what}"
            );
        }
    }
}

#[test]
#[ignore]
fn a_failed_flush_stops_the_commit_before_anything_is_renamed_or_pointed() {
    let _serial = serial();
    let (_fs, store) = system("ext4");
    let esp = FailingEsp::new();
    esp_with_deployment_1(&esp.mnt);
    let staging = Staging::new(&store, 2).unwrap();
    staging
        .transaction()
        .unwrap()
        .run(&["sh", "-ec", "printf uki-2 > /boot/archix.efi"])
        .unwrap();
    let staged = staging.finish().unwrap();
    let staged_dir = staged.dir.clone();
    fs::write(esp.mnt.join("dirty"), vec![1u8; 1 << 20]).unwrap();
    esp.fail();
    let boot = SystemdBoot {
        esp: esp.mnt.clone(),
        uki: PathBuf::from("boot/archix.efi"),
    };
    let err = staged.commit(&store, &esp.mnt, &boot).unwrap_err();
    assert!(
        !store.path(2).exists(),
        "deployment 2 was committed after a failed flush: {err:#}"
    );
    assert!(staged_dir.is_dir());
    assert_eq!(store.current().unwrap(), Some(1));
    assert!(format!("{err:#}").contains("flushing"), "{err:#}");
}

fn free_bytes(path: &Path) -> u64 {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) }, 0);
    stat.f_bavail * stat.f_frsize
}

#[test]
#[ignore]
fn a_commit_that_fills_the_esp_leaves_no_temporary_file_and_the_next_commit_succeeds() {
    let _serial = serial();
    let (_fs, store) = system("ext4");
    let esp_fs = LoopFs::sized("vfat", "4M");
    let esp = esp_fs.mnt.clone();
    esp_with_deployment_1(&esp);
    let boot = SystemdBoot {
        esp: esp.clone(),
        uki: PathBuf::from("boot/archix.efi"),
    };
    let uki: u64 = 1_500_000;
    let stage = |number: u32| {
        let staging = Staging::new(&store, number).unwrap();
        staging
            .transaction()
            .unwrap()
            .run(&["sh", "-ec", &format!("head -c {uki} /dev/zero > /boot/archix.efi")])
            .unwrap();
        staging.finish().unwrap()
    };
    let filler = esp.join("filler");
    fs::write(&filler, vec![0u8; (free_bytes(&esp) - uki / 2) as usize]).unwrap();
    let err = stage(2).commit(&store, &esp, &boot).unwrap_err();
    assert!(
        tmp_files(&esp).is_empty(),
        "a failed write left {:?}: {err:#}",
        tmp_files(&esp)
    );
    assert_eq!(store.current().unwrap(), Some(1));
    fs::remove_file(&filler).unwrap();
    fs::write(
        esp.join("EFI/archix/9.efi.tmp"),
        vec![0u8; (free_bytes(&esp) - uki / 2) as usize],
    )
    .unwrap();
    fs::write(esp.join("loader/entries/archix-9.conf.tmp"), "half").unwrap();
    fs::write(esp.join("loader/loader.conf.tmp"), "half").unwrap();
    stage(3).commit(&store, &esp, &boot).unwrap();
    assert!(
        tmp_files(&esp).is_empty(),
        "stale temporary files survived: {:?}",
        tmp_files(&esp)
    );
    assert_eq!(store.current().unwrap(), Some(3));
    assert_eq!(esp_state(&esp), (vec![1, 3], Some(3)));
}

#[test]
#[ignore]
fn mounts_under_the_live_var_log_and_cache_are_bound_live_and_others_stay_behind_the_overlay() {
    let _serial = serial();
    let (_fs, store) = system("ext4");
    let var = store.root().join("var");
    for sub in ["log", "log/journal", "cache", "lib/docker"] {
        fs::create_dir_all(var.join(sub)).unwrap();
        sh(Command::new("mount").args(["-t", "tmpfs", "tmpfs"]).arg(var.join(sub)));
    }
    fs::write(var.join("log/live.log"), "old\n").unwrap();
    fs::write(var.join("log/journal/j"), "journal\n").unwrap();
    fs::write(var.join("lib/docker/state"), "docker\n").unwrap();
    let staging = Staging::new(&store, 2).unwrap();
    let script = "grep -qx old /var/log/live.log; echo new >> /var/log/live.log; grep -qx journal /var/log/journal/j; \
        echo fc > /var/cache/fontconfig-probe; echo line >> /var/log/pacman.log; \
        test ! -e /var/lib/docker/state; echo staged > /var/lib/docker/staged";
    let output = staging.transaction().unwrap().output(&["sh", "-ec", script]).unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let staged = staging.finish().unwrap().dir;
    assert_eq!(fs::read_to_string(var.join("log/live.log")).unwrap(), "old\nnew\n");
    assert!(var.join("cache/fontconfig-probe").exists());
    assert_eq!(fs::read_to_string(var.join("log/pacman.log")).unwrap(), "line\n");
    assert!(!var.join("lib/docker/staged").exists());
    let upper = staged.join("var-upper");
    assert_eq!(fs::read_to_string(upper.join("lib/docker/staged")).unwrap(), "staged\n");
    for live in ["log", "cache"] {
        assert!(upper.join(live).symlink_metadata().is_err(), "var-upper carries {live}");
    }
}

#[test]
#[ignore]
fn systemctl_in_the_view_skips_live_verbs_and_enables_offline() {
    let _serial = serial();
    let (_fs, store) = system("ext4");
    let usr = store.path(1).join("usr");
    install_program(&usr, "/usr/bin/systemctl", "systemctl");
    fs::create_dir_all(usr.join("lib/systemd/system")).unwrap();
    fs::write(
        usr.join("lib/systemd/system/probe.service"),
        "[Service]\nExecStart=/usr/bin/true\n[Install]\nWantedBy=multi-user.target\n",
    )
    .unwrap();
    let staging = Staging::new(&store, 2).unwrap();
    let tx = staging.transaction().unwrap();
    for verb in ["daemon-reload", "enable", "restart"] {
        let mut argv = vec!["systemctl", verb];
        if verb != "daemon-reload" {
            argv.push("probe.service");
        }
        let output = tx.output(&argv).unwrap();
        assert!(output.status.success(), "{argv:?}: {}", stderr(&output));
    }
    let staged = staging.finish().unwrap().dir;
    assert_eq!(
        fs::read_link(staged.join("etc/systemd/system/multi-user.target.wants/probe.service")).unwrap(),
        Path::new("/usr/lib/systemd/system/probe.service")
    );
}

/// Copies the host's files of `packages` under /usr, without docs and locales, and the libraries they load, into the deployment `dep`.
fn install_host_packages(dep: &Path, packages: &[&str]) {
    let script = r#"
        for p in $PACKAGES; do pacman -Qlq "$p"; done | grep -v '/$' | grep '^/usr/' \
          | grep -Ev '^/usr/(share/(man|doc|info|locale|i18n|help|licenses|zoneinfo|bash-completion|zsh|fish|gtk-doc|iana-etc)|include|lib/(perl5|python[^/]*|gconv|getconf|audit))/' > "$WORK/files"
        while read -r f; do
          if [ -f "$f" ] && [ ! -L "$f" ]; then ldd "$f" 2>/dev/null | awk '$2 == "=>" && $3 ~ /^\// {print $3}'; fi
        done < "$WORK/files" > "$WORK/libs"
        cat "$WORK/files" "$WORK/libs" | while read -r f; do echo "$f"; readlink -f "$f"; done | sort -u | sed 's|^/||' > "$WORK/all"
        tar -C / --no-recursion -cf - -T "$WORK/all" | tar -C "$DEP" --unlink-first -xpf -
    "#;
    let work = tempfile::tempdir().unwrap();
    sh(Command::new("bash")
        .args(["-c", script])
        .env("PACKAGES", packages.join(" "))
        .env("WORK", work.path())
        .env("DEP", dep));
}

#[test]
#[ignore]
fn mkinitcpio_autodetect_in_the_view_sees_the_root_filesystem() {
    let _serial = serial();
    let kver = sh(Command::new("ls").arg("/usr/lib/modules"))
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();
    for fs_type in ["ext4", "btrfs"] {
        let (_fs, store) = system_on(LoopFs::sized(fs_type, "448M"), fs_type);
        let dep = store.path(1);
        install_host_packages(
            &dep,
            &[
                "mkinitcpio",
                "mkinitcpio-busybox",
                "bash",
                "coreutils",
                "gawk",
                "sed",
                "grep",
                "findutils",
                "util-linux",
                "kmod",
                "libarchive",
                "zstd",
                "gzip",
                "diffutils",
                "glibc",
            ],
        );
        let modules = dep.join("usr/lib/modules").join(&kver);
        fs::create_dir_all(&modules).unwrap();
        for entry in fs::read_dir(Path::new("/usr/lib/modules").join(&kver))
            .unwrap()
            .flatten()
        {
            if entry.file_name().to_string_lossy().starts_with("modules.") {
                fs::copy(entry.path(), modules.join(entry.file_name())).unwrap();
            }
        }
        fs::write(
            dep.join("etc/mkinitcpio-test.conf"),
            "MODULES=()\nBINARIES=()\nFILES=()\nHOOKS=(autodetect archixprobe)\nCOMPRESSION=cat\n",
        )
        .unwrap();
        fs::write(
            dep.join("usr/lib/initcpio/install/archixprobe"),
            "#!/usr/bin/env bash\nbuild() {\n    local m\n    for m in \"${!_autodetect_cache[@]}\"; do\n        add_dir \"/autodetected/$m\"\n    done\n}\n",
        )
        .unwrap();
        let staging = Staging::new(&store, 2).unwrap();
        let tx = staging.transaction().unwrap();
        let root = tx.output(&["findmnt", "-uno", "fstype", "-T", "/"]).unwrap();
        let build = tx
            .output(&[
                "mkinitcpio",
                "-c",
                "/etc/mkinitcpio-test.conf",
                "-k",
                &kver,
                "-g",
                "/boot/initramfs-test.img",
            ])
            .unwrap();
        assert!(
            build.status.success(),
            "{fs_type}: {}{}",
            String::from_utf8_lossy(&build.stdout),
            stderr(&build)
        );
        let staged = staging.finish().unwrap().dir;
        let listing = sh(Command::new("lsinitcpio").arg(staged.join("boot/initramfs-test.img")));
        let module = format!("autodetected/{fs_type}");
        assert!(
            listing.lines().any(|line| line.trim_end_matches('/') == module),
            "{fs_type}: autodetect dropped the root filesystem's module:\n{listing}"
        );
        assert_eq!(
            String::from_utf8_lossy(&root.stdout).trim(),
            fs_type,
            "{}",
            stderr(&root)
        );
    }
}

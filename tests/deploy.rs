//! Deployment staging, flattening, the transaction's namespaces and the commit order on loop-mounted ext4 and btrfs; run only inside the privileged Arch container via `tests/arch/run.sh`.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};

use archix::deploy::{Backend, Keep, OVERLAY_OPTIONS, Staging, Store, SystemdBoot, flatten, xattr};
use archix::system::boot::{BootPlan, Built, Entries};
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
        LoopFs::made(fs, size, &[])
    }

    fn made(fs: &str, size: &str, mkfs_args: &[&str]) -> LoopFs {
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
        sh(mkfs.args(mkfs_args).arg(&loop_fs.dev));
        // A probe can still see the previous filesystem on a reused loop device, so the type isn't left to it.
        sh(Command::new("mount")
            .args(["-t", fs])
            .arg(&loop_fs.dev)
            .arg(&loop_fs.mnt));
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
    fs::write(
        dep.join("pacman/base-1/files"),
        "%FILES%\nvar/\nvar/lib/\nvar/lib/oldstate\nvar/log/\nvar/cache/\nvar/cache/pacman/\nvar/cache/pacman/pkg/\n\
         var/lib/pacman/\nvar/lib/dkms/\n\n",
    )
    .unwrap();
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
        .env("ARCHIX_TEST_PLAIN_ESP", "1")
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
    if action == "layout" {
        archix::deploy::layout::lay_out(&root).unwrap();
        return;
    }
    if action == "drop" {
        let paths = archix::paths::Paths::under(&root);
        let esp = PathBuf::from(std::env::var("ARCHIX_DEPLOY_ESP").unwrap());
        archix::system::boot::drop_entries(&paths, &esp, 4).unwrap();
        return;
    }
    if action == "eject" {
        archix::system::deploy_boot::writable_store(&root).unwrap();
        let paths = archix::paths::Paths::under(&root);
        archix::deploy::eject::eject(&paths, Path::new(env!("CARGO_BIN_EXE_archix")), false).unwrap();
        return;
    }
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
        "commit" => {
            let name = format!("test-{number}.efi");
            tx.run(&[
                "sh",
                "-ec",
                &format!("mkdir -p /boot/archix && printf uki-{number} > /boot/archix/{name}"),
            ])
            .unwrap();
            let esp = root.join("efi");
            let paths = archix::paths::Paths::under(&root);
            let plan = BootPlan {
                esp: esp.display().to_string(),
                kernel: "linux".to_string(),
                microcode: Vec::new(),
                cmdline: String::new(),
                secure_boot: Some(false),
                anticheat: false,
                tries: 3,
                revoke_unused: false,
                pcr_key: None,
                health_check: None,
            };
            let built = Built::single("test", &name, &format!("archix {number}"));
            let entries = Entries {
                paths: &paths,
                plan: &plan,
                built: &built,
                droppable: Vec::new(),
            };
            staging.finish().unwrap().commit(&store, &esp, &entries).unwrap();
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

/// What must hold after any crash of archix's real ESP step: `current`, every archix entry and loader.conf's `default` and `preferred` name complete deployments, and each entry's UKI is its deployment's.
fn assert_bootable(store: &Store, what: &str) {
    let committed = store.deployments().unwrap();
    let complete = |n: u32| {
        committed.contains(&n)
            && archix::deploy::TREES
                .iter()
                .all(|tree| store.path(n).join(tree).is_dir())
    };
    let current = store.current().unwrap().unwrap();
    assert!(complete(current), "{what}: current {current}");
    let esp = store.root().join("efi");
    let found = archix::system::boot::entries(&esp);
    for (n, _, path) in &found {
        assert!(
            complete(*n),
            "{what}: {} names an incomplete deployment",
            path.display()
        );
        let text = fs::read_to_string(path).unwrap();
        let uki = text
            .lines()
            .find_map(|line| line.strip_prefix("efi ").or_else(|| line.strip_prefix("uki ")))
            .unwrap_or_else(|| panic!("{what}: {} names no UKI: {text:?}", path.display()));
        assert_eq!(
            fs::read_to_string(esp.join(uki.trim().trim_start_matches('/'))).unwrap(),
            format!("uki-{n}"),
            "{what}: {}",
            path.display()
        );
    }
    let conf = fs::read_to_string(esp.join("loader/loader.conf")).unwrap();
    assert!(
        conf.lines().any(|line| line.starts_with("default ")),
        "{what}: no default in {conf:?}"
    );
    for id in conf.lines().filter_map(|line| {
        line.strip_prefix("default ")
            .or_else(|| line.strip_prefix("preferred "))
    }) {
        let n = archix::deploy::keep::entry_number(id.trim()).unwrap();
        assert!(
            found.iter().any(|(m, extra, _)| *m == n && !extra),
            "{what}: loader.conf names {id}, which has no entry"
        );
    }
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
    // systemd-boot 260's LoaderFeatures, with which archix counts tries and writes `preferred`.
    let counting = ((1u64 << 4) | (1 << 16) | (1 << 19)).to_string();
    for (fs_type, features) in [("ext4", "0"), ("ext4", counting.as_str()), ("btrfs", counting.as_str())] {
        let (_fs, store) = system(fs_type);
        let esp_fs = LoopFs::sized("vfat", "64M");
        let esp = store.root().join("efi");
        sh(Command::new("mount").arg("--bind").arg(&esp_fs.mnt).arg(&esp));
        esp_with_deployment_1(&esp);
        let env = [("ARCHIX_TEST_LOADER_FEATURES", features)];
        for (step, current) in steps {
            let what = format!("{fs_type} with a FAT ESP, LoaderFeatures {features}, killed at {step}");
            let status = helper("commit", store.root(), &[env[0], ("ARCHIX_TEST_CRASH", step)])
                .output()
                .unwrap();
            assert_eq!(
                std::os::unix::process::ExitStatusExt::signal(&status.status),
                Some(libc::SIGKILL),
                "{what}: {}",
                String::from_utf8_lossy(&status.stderr)
            );
            assert!(!mountinfo_mentions(&store.deploy_dir()), "{what}: a view mount leaked");
            assert_bootable(&store, &what);
            assert_eq!(store.current().unwrap(), Some(current), "{what}");

            let next = store.next_number().unwrap();
            let status = helper("commit", store.root(), &env).output().unwrap();
            assert!(
                status.status.success(),
                "{what}, then a switch: {}",
                String::from_utf8_lossy(&status.stderr)
            );
            assert_bootable(&store, &format!("{what}, then a switch"));
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
        // Unbound before its loop device goes, which otherwise detaches later and under the next test's losetup -f.
        sh(Command::new("umount").arg(&esp));
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

#[test]
#[ignore]
fn a_flush_that_fails_once_current_names_the_new_deployment_is_a_warning_not_a_failed_switch() {
    let _serial = serial();
    let (_fs, store) = system("ext4");
    let out = helper(
        "switch",
        store.root(),
        &[("ARCHIX_TEST_FAIL", "deploy-commit-final-flush")],
    )
    .output()
    .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "a committed deployment failed the switch:\n{text}"
    );
    assert!(text.contains("deployment 2 is committed; flushing failed"), "{text}");
    assert_eq!(store.current().unwrap(), Some(2));
    assert_eq!(store.deployments().unwrap(), vec![1, 2]);
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
    assert_eq!(
        store.deployments().unwrap(),
        vec![1],
        "deployment 2 stayed without a boot entry: {err:#}"
    );
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
        test ! -e /var/lib/docker/state; mkdir -p /var/lib/docker; echo staged > /var/lib/docker/staged";
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

/// A plain Arch-like root on a fresh filesystem, as the first switch finds one: usr with a hardlinked pair and a file capability, etc, opt, boot, pacman's database, the static binary and an ESP directory.
fn plain_root(fs_type: &str) -> LoopFs {
    let loop_fs = LoopFs::new(fs_type);
    let root = &loop_fs.mnt;
    for dir in [
        "usr/bin",
        "usr/lib/archix",
        "etc",
        "opt/app",
        "boot",
        "var/lib/pacman/local/base-1",
        "var/log",
        "efi",
        "home",
        "proc",
        "sys",
        "dev",
        "run",
        "tmp",
    ] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    for (link, target) in [("bin", "usr/bin"), ("lib", "usr/lib")] {
        std::os::unix::fs::symlink(target, root.join(link)).unwrap();
    }
    fs::write(root.join("usr/bin/tool"), "tool\n").unwrap();
    fs::set_permissions(root.join("usr/bin/tool"), fs::Permissions::from_mode(0o755)).unwrap();
    xattr::set(&root.join("usr/bin/tool"), c"security.capability", &capability()).unwrap();
    fs::write(root.join("usr/bin/a"), "same file\n").unwrap();
    fs::hard_link(root.join("usr/bin/a"), root.join("usr/bin/b")).unwrap();
    fs::write(root.join("usr/lib/archix/archix-static"), "static\n").unwrap();
    fs::write(root.join("etc/hostname"), "plain\n").unwrap();
    fs::write(root.join("etc/shadow"), "root:!:1::::::\n").unwrap();
    fs::set_permissions(root.join("etc/shadow"), fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(root.join("opt/app/data"), "data\n").unwrap();
    fs::write(root.join("boot/vmlinuz-linux"), "kernel\n").unwrap();
    fs::write(root.join("var/lib/pacman/local/base-1/desc"), "%NAME%\nbase\n").unwrap();
    loop_fs
}

/// The trees a layout moves, keyed by tree name, as `snapshot` sees them.
fn live_trees(root: &Path) -> BTreeMap<String, BTreeMap<PathBuf, Entry>> {
    [
        ("usr", "usr"),
        ("etc", "etc"),
        ("opt", "opt"),
        ("boot", "boot"),
        ("pacman", "var/lib/pacman/local"),
    ]
    .into_iter()
    .map(|(tree, at)| (tree.to_string(), snapshot(&root.join(at))))
    .collect()
}

fn mounted(path: &Path) -> bool {
    archix::deploy::view::mount_points(&fs::read("/proc/self/mountinfo").unwrap()).contains(&path.to_path_buf())
}

#[test]
#[ignore]
fn the_first_layout_copies_the_live_trees_into_deployment_1_binds_them_back_and_prunes_what_they_hide() {
    let _serial = serial();
    for fs_type in ["ext4", "btrfs"] {
        let loop_fs = plain_root(fs_type);
        let root = loop_fs.mnt.clone();
        let before = live_trees(&root);
        assert_eq!(archix::deploy::layout::unsupported(&root, &root.join("efi")), None);
        let store = archix::deploy::layout::lay_out(&root).unwrap();
        assert_eq!(store.deployments().unwrap(), vec![1], "{fs_type}");
        assert_eq!(store.current().unwrap(), Some(1), "{fs_type}");
        let one = store.path(1);
        for (tree, entries) in &before {
            assert_eq!(&snapshot(&one.join(tree)), entries, "{fs_type}: deployment 1's {tree}");
        }
        assert!(one.join("dkms").is_dir() && one.join("var-upper").is_dir());
        assert_eq!(
            live_trees(&root),
            before,
            "{fs_type}: the live trees stay whole until bound"
        );
        if fs_type == "btrfs" {
            assert_eq!(ino(&one), 256, "deployment 1 is a subvolume");
        } else {
            assert_eq!(
                ino(&one.join("usr/bin/tool")),
                ino(&root.join("usr/bin/tool")),
                "usr is hardlinked"
            );
            assert_ne!(
                ino(&one.join("etc/hostname")),
                ino(&root.join("etc/hostname")),
                "etc is a real copy"
            );
        }

        archix::deploy::layout::bind_live(&root, &store, 1).unwrap();
        for at in ["usr", "etc", "opt", "boot", "var/lib/pacman/local", "var/lib/dkms"] {
            assert!(mounted(&root.join(at)), "{fs_type}: {at} isn't bound");
        }
        let err = fs::write(root.join("usr/bin/new"), "x").unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EROFS), "{fs_type}: /usr is read-only");
        fs::write(root.join("etc/new"), "x").unwrap();
        assert!(
            one.join("etc/new").exists(),
            "{fs_type}: the live /etc is deployment 1's"
        );
        fs::remove_file(one.join("etc/new")).unwrap();

        archix::deploy::layout::prune_hidden(&root, Path::new(env!("CARGO_BIN_EXE_archix"))).unwrap();
        for at in ["usr", "etc", "opt", "boot", "var/lib/pacman/local", "var/lib/dkms"] {
            sh(Command::new("umount").arg(root.join(at)));
        }
        for at in ["usr", "etc", "opt", "boot", "var/lib/pacman/local"] {
            assert_eq!(
                fs::read_dir(root.join(at)).unwrap().count(),
                0,
                "{fs_type}: the hidden {at} wasn't pruned"
            );
        }
        for (tree, entries) in &before {
            let now = snapshot(&one.join(tree));
            // Writing etc/new above changed only the etc directory's own times.
            let kept = |(rel, _): &(&PathBuf, &Entry)| tree != "etc" || !rel.as_os_str().is_empty();
            assert!(
                now.iter().filter(kept).eq(entries.iter().filter(kept)),
                "{fs_type}: pruning touched deployment 1's {tree}"
            );
        }
    }
}

#[test]
#[ignore]
fn pruning_refuses_a_root_whose_trees_arent_bound_from_a_deployment() {
    let _serial = serial();
    let loop_fs = plain_root("ext4");
    let root = loop_fs.mnt.clone();
    let before = live_trees(&root);
    archix::deploy::layout::lay_out(&root).unwrap();
    let err = archix::deploy::layout::prune_hidden(&root, Path::new(env!("CARGO_BIN_EXE_archix"))).unwrap_err();
    assert!(format!("{err:#}").contains("isn't bound from a deployment"), "{err:#}");
    assert_eq!(live_trees(&root), before, "nothing was pruned");
}

#[test]
#[ignore]
fn a_layout_killed_partway_leaves_the_old_system_whole_and_starts_over_cleanly() {
    let _serial = serial();
    for fs_type in ["ext4", "btrfs"] {
        let loop_fs = plain_root(fs_type);
        let root = loop_fs.mnt.clone();
        let before = live_trees(&root);
        for step in ["layout-copied", "layout-committed"] {
            let what = format!("{fs_type}, killed at {step}");
            let killed = helper("layout", &root, &[("ARCHIX_TEST_CRASH", step)])
                .output()
                .unwrap();
            assert_eq!(
                std::os::unix::process::ExitStatusExt::signal(&killed.status),
                Some(libc::SIGKILL),
                "{what}: {}",
                stderr(&killed)
            );
            assert!(
                !root.join("archix/current").exists(),
                "{what}: nothing names a deployment yet"
            );
            assert_eq!(live_trees(&root), before, "{what}: the live system changed");
        }
        let store = archix::deploy::layout::lay_out(&root).unwrap();
        assert_eq!(store.deployments().unwrap(), vec![1], "{fs_type}");
        let names: Vec<String> = fs::read_dir(store.deploy_dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["1"], "{fs_type}: leftovers of the killed layouts");
        assert_eq!(snapshot(&store.path(1).join("usr")), before["usr"], "{fs_type}");
    }
}

#[test]
#[ignore]
fn a_separate_boot_usr_opt_or_var_or_an_esp_in_boot_is_refused_before_anything_changes() {
    let _serial = serial();
    let loop_fs = plain_root("ext4");
    let root = loop_fs.mnt.clone();
    fs::create_dir_all(root.join("boot/efi")).unwrap();
    let refused = |esp: &str| archix::deploy::layout::unsupported(&root, &root.join(esp)).unwrap_or_default();
    assert!(refused("boot/efi").contains("in /boot"), "{}", refused("boot/efi"));
    for tree in ["boot", "usr", "opt", "var", "var/lib", "var/lib/pacman"] {
        sh(Command::new("mount")
            .args(["-t", "tmpfs", "tmpfs"])
            .arg(root.join(tree)));
        let why = refused("efi");
        sh(Command::new("umount").arg(root.join(tree)));
        assert!(
            why.contains(&format!("/{tree} is a separate filesystem")),
            "{tree}: {why}"
        );
    }
    fs::remove_file(root.join("usr/lib/archix/archix-static")).unwrap();
    assert!(refused("efi").contains("archix-static"));
    assert!(!root.join("archix").exists());
}

/// Fills the filesystem holding `dir` until `leave` bytes are free.
fn fill(dir: &Path, leave: u64) {
    let size = free_bytes(dir).saturating_sub(leave);
    sh(Command::new("fallocate")
        .args(["-l", &size.to_string()])
        .arg(dir.join("filler")));
}

#[test]
#[ignore]
fn a_switch_that_only_removes_packages_still_needs_room_to_stage() {
    let _serial = serial();
    let loop_fs = LoopFs::new("ext4");
    let root = loop_fs.mnt.clone();
    let db = root.join("db");
    fs::create_dir(&db).unwrap();
    let removed = ["tree".to_string()];
    archix::conflicts::check(&[], &removed, &db, &root).unwrap();
    fill(&root, 1 << 20);
    let err = archix::conflicts::check(&[], &removed, &db, &root).expect_err("a full disk passed the space check");
    assert!(format!("{err:#}").contains("nothing was changed"), "{err:#}");
}

#[test]
#[ignore]
fn a_build_that_fails_on_a_full_btrfs_leaves_no_staging_tree() {
    let _serial = serial();
    let (_fs, store) = system("btrfs");
    let root = store.root().to_path_buf();
    fill(&root, 16 << 20);
    let paths = archix::paths::Paths::under(&root);
    let record: archix::generations::Generation = serde_json::from_str(
        r#"{"number": 2, "created": 0, "host": "box", "date": "2026-10-10", "packages": {},
            "settings": {"remove_undeclared": true, "ignored": [], "keep": 1, "keep_days": 0},
            "config_commit": null, "config_dirty": false}"#,
    )
    .unwrap();
    let boot = archix::system::boot::BootPlan {
        esp: root.join("efi").display().to_string(),
        kernel: "linux".into(),
        microcode: Vec::new(),
        cmdline: "rw".into(),
        secure_boot: Some(false),
        anticheat: false,
        tries: 0,
        revoke_unused: false,
        pcr_key: None,
        health_check: None,
    };
    let packages = archix::packages::Packages {
        install: Vec::new(),
        remove: Vec::new(),
        versions: BTreeMap::new(),
        reasons: BTreeMap::new(),
        reinstall: false,
    };
    let plan = archix::deploy::build::Plan {
        paths: &paths,
        number: 2,
        base: 1,
        packages: &packages,
        system: &archix::system::apply::SystemChange::default(),
        boot: &boot,
        repos: &[],
        record: &record,
        keep: 2,
        config: &|_| Ok(()),
    };
    let Err(err) = archix::deploy::build::build(&store, &plan) else {
        panic!("the build can't succeed");
    };
    let left: Vec<String> = fs::read_dir(store.deploy_dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with('.'))
        .collect();
    assert!(left.is_empty(), "the failed build left {left:?}: {err:#}");
    assert_eq!(store.deployments().unwrap(), vec![1]);
    assert_eq!(store.current().unwrap(), Some(1));
}

/// Runs `script` in the view with sh -ec and returns its stdout, failing the test when it fails.
fn in_view(tx: &archix::deploy::Transaction, script: &str) -> String {
    let output = tx.output(&["sh", "-ec", script]).unwrap();
    assert!(output.status.success(), "{script}: {}", stderr(&output));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn commit(store: &Store, staging: Staging) {
    archix::testhooks::allow_plain_esp();
    let esp = store.root().join("efi");
    let boot = SystemdBoot {
        esp: esp.clone(),
        uki: PathBuf::from("boot/archix.efi"),
    };
    staging.finish().unwrap().commit(store, &esp, &boot).unwrap();
}

fn kernel_log() -> Option<Vec<String>> {
    let output = Command::new("dmesg").output().ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_string)
            .collect()
    })
}

#[test]
#[ignore]
fn the_views_var_lower_is_a_private_copy_of_what_packages_own_so_a_busy_live_var_never_reaches_it() {
    let _serial = serial();
    for fs_type in ["ext4", "btrfs"] {
        let (_fs, store) = system(fs_type);
        let root = store.root().to_path_buf();
        fs::write(root.join("var/lib/unowned"), "service state\n").unwrap();
        let before = kernel_log();
        let mut busy = Command::new("sh")
            .arg("-c")
            .arg(
                "i=0; while :; do echo $i >> var/log/busy.log; mkdir -p var/lib/busy; echo $i > var/lib/busy/f$i; \
                 rm -f var/lib/busy/f$((i-3)) var/lib/unowned.$((i-1)); echo $i > var/lib/unowned.$i; i=$((i+1)); done",
            )
            .current_dir(&root)
            .spawn()
            .unwrap();
        let staging = Staging::new(&store, 2).unwrap();
        let tx = staging.transaction().unwrap();
        let mounts = in_view(&tx, "grep ' /var ' /proc/self/mountinfo");
        let listing = in_view(
            &tx,
            "for i in $(seq 300); do mkdir -p /var/lib/pkgstate; echo $i > /var/lib/pkgstate/f$i; ls /var/lib > /dev/null; done; \
             test ! -e /var/lib/unowned; test ! -e /var/lib/busy; test ! -e /var/log/busy.log; rm /var/lib/oldstate; ls -A /var/lib",
        );
        busy.kill().unwrap();
        busy.wait().unwrap();
        let staged = staging.finish().unwrap().dir;

        assert!(
            mounts.contains(".var-lower") && !mounts.contains(&format!("lowerdir={}/var,", root.display())),
            "{fs_type}: the live /var is a lower layer: {mounts}"
        );
        assert!(
            !listing.contains("unowned") && !listing.contains("busy"),
            "{fs_type}: {listing}"
        );
        let upper = staged.join("var-upper");
        assert_eq!(
            fs::read_dir(upper.join("lib/pkgstate")).unwrap().count(),
            300,
            "{fs_type}"
        );
        for live in ["lib/busy", "log/busy.log", "lib/unowned"] {
            assert!(
                upper.join(live).symlink_metadata().is_err(),
                "{fs_type}: var-upper has {live}"
            );
        }
        let whiteout = fs::symlink_metadata(upper.join("lib/oldstate")).unwrap();
        assert!(
            whiteout.file_type().is_char_device() && whiteout.rdev() == 0,
            "{fs_type}"
        );
        assert!(root.join("var/lib/oldstate").exists() && root.join("var/lib/unowned").exists());
        assert!(
            !staged.join(".var-lower").exists(),
            "{fs_type}: the lower copy is scratch"
        );
        if let (Some(before), Some(after)) = (&before, kernel_log()) {
            let new: Vec<&String> = after
                .iter()
                .filter(|line| !before.contains(line) && line.contains("overlay"))
                .collect();
            assert!(new.is_empty(), "{fs_type}: overlayfs complained: {new:?}");
        } else {
            eprintln!("dmesg can't be read here, so overlayfs warnings go unchecked");
        }
    }
}

#[test]
#[ignore]
fn a_second_switch_before_the_reboot_builds_on_the_pending_deployment_and_carries_its_var_changes() {
    let _serial = serial();
    for fs_type in ["ext4", "btrfs"] {
        let (_fs, store) = system(fs_type);
        let root = store.root().to_path_buf();
        let files = store.path(1).join("pacman/base-1/files");
        let listed = fs::read_to_string(&files).unwrap();
        fs::write(&files, format!("{listed}%BACKUP%\nvar/lib/conf\told-md5\n\n")).unwrap();
        fs::write(root.join("var/lib/conf"), "conf\n").unwrap();

        let two = Staging::new(&store, 2).unwrap();
        let tx = two.transaction().unwrap();
        in_view(
            &tx,
            "mkdir -p /var/lib/two; echo 2 > /var/lib/two/f; echo two > /usr/lib/two; echo deployment-2 > /etc/hostname",
        );
        drop(tx);
        let staged_files = two.tree("pacman").join("base-1/files");
        let text = fs::read_to_string(&staged_files).unwrap().replace("old-md5", "two-md5");
        fs::write(&staged_files, text).unwrap();
        commit(&store, two);
        assert_eq!(store.current().unwrap(), Some(2));
        assert_eq!(
            fs::read_to_string(store.path(2).join("var-backup")).unwrap(),
            "lib/conf\told-md5\n",
            "{fs_type}"
        );

        let three = Staging::new(&store, 3).unwrap();
        assert_eq!(
            three.base(),
            2,
            "{fs_type}: staged from the pending deployment, not the running one"
        );
        let tx = three.transaction().unwrap();
        in_view(
            &tx,
            "test \"$(cat /var/lib/two/f)\" = 2; test -e /usr/lib/two; grep -qx deployment-2 /etc/hostname; \
             mkdir -p /var/lib/three; echo 3 > /var/lib/three/f",
        );
        drop(tx);
        commit(&store, three);
        let upper = store.path(3).join("var-upper");
        assert_eq!(
            fs::read_to_string(upper.join("lib/two/f")).unwrap(),
            "2\n",
            "{fs_type}: 2's /var changes are lost"
        );
        assert_eq!(
            fs::read_to_string(upper.join("lib/three/f")).unwrap(),
            "3\n",
            "{fs_type}"
        );
        assert_eq!(
            fs::read_to_string(store.path(3).join("var-backup")).unwrap(),
            "lib/conf\told-md5\n",
            "{fs_type}: the hash the live file was installed with, not the pending one's"
        );
        assert_eq!(
            fs::read_to_string(store.path(2).join("var-upper/lib/two/f")).unwrap(),
            "2\n"
        );

        let id = archix::system::activate::deployment_id(&store.path(3), 3).unwrap();
        let state = archix::paths::Paths::under(&root).state;
        fs::create_dir_all(state.join("activated")).unwrap();
        fs::write(state.join("activated").join(id), "").unwrap();
        let four = Staging::new(&store, 4).unwrap();
        let tx = four.transaction().unwrap();
        in_view(&tx, "test ! -e /var/lib/two/f; test ! -e /var/lib/three/f");
        drop(tx);
        commit(&store, four);
        assert!(
            !store.path(4).join("var-upper/lib/two").exists(),
            "{fs_type}: a booted deployment's /var changes are live already"
        );
        assert!(store.path(4).join("var-upper").is_dir());
    }
}

#[test]
#[ignore]
fn signing_keys_are_hidden_from_everything_in_the_view_but_the_boot_builds_signing_step() {
    let _serial = serial();
    for fs_type in ["ext4", "btrfs"] {
        let (_fs, store) = system(fs_type);
        let root = store.root().to_path_buf();
        let dep = store.path(1);
        let files = dep.join("pacman/base-1/files");
        let listed = fs::read_to_string(&files)
            .unwrap()
            .replace("%FILES%\n", "%FILES%\nvar/lib/sbctl/keys/db/db.key\n");
        fs::write(&files, listed).unwrap();
        for (path, secret) in [
            (root.join("var/lib/sbctl/keys/db/db.key"), "SBCTL-SECRET"),
            (dep.join("usr/share/secureboot/keys/db/db.key"), "OLD-SBCTL-SECRET"),
            (dep.join("etc/systemd/tpm2-pcr-private-key.pem"), "PCR-SECRET"),
        ] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, secret).unwrap();
        }
        let staging = Staging::new(&store, 2).unwrap();
        let tx = staging.transaction().unwrap();
        let script = "for f in /var/lib/sbctl/keys/db/db.key /usr/share/secureboot/keys/db/db.key /etc/systemd/tpm2-pcr-private-key.pem; do \
                      cat $f 2>/dev/null || true; cp $f /tmp/stolen 2>/dev/null && cat /tmp/stolen || true; done; \
                      umount /var/lib/sbctl/keys 2>/dev/null || true; umount /etc/systemd/tpm2-pcr-private-key.pem 2>/dev/null || true; \
                      cat /var/lib/sbctl/keys/db/db.key /etc/systemd/tpm2-pcr-private-key.pem 2>/dev/null || true";
        let seen = in_view(&tx, script);
        assert!(!seen.contains("SECRET"), "{fs_type}: a scriptlet read a key: {seen}");
        let signing = staging.signing_transaction().unwrap();
        assert_eq!(
            in_view(&signing, "cat /etc/systemd/tpm2-pcr-private-key.pem"),
            "PCR-SECRET",
            "{fs_type}: ukify reads the PCR key"
        );
        drop((tx, signing));
        staging.discard().unwrap();
    }
}

/// Stands in for pacman: with /etc/net-sandbox, CachyOS's, which refuses to run hooks and scriptlets it can't put in a network namespace unless told not to; without, Arch's, which rejects that option. Logs each run's arguments, network namespace, interface count and whether loopback is up.
const NET_SANDBOX_PACMAN: &str = r#"#!/bin/sh
if [ "$1" = --disable-sandbox-network ]; then
  [ -e /etc/net-sandbox ] || { echo "pacman: unrecognized option '--disable-sandbox-network'" >&2; exit 1; }
  shift
elif [ -e /etc/net-sandbox ] && ! unshare -n true; then
  echo 'refusing to run "/bin/sh" with network access; set DisableSandboxNetwork in pacman.conf to override' >&2
  exit 1
fi
[ "$1" = --version ] && exit 0
echo "$1 $(readlink /proc/self/ns/net) $(grep -c : /proc/net/dev) $(ip link show lo | grep -c ',UP')" >> /var/log/pacman.log
[ "$1" = -Rdd ] && rm -r /var/lib/pacman/local/base-1
exit 0
"#;

#[test]
#[ignore]
fn a_pacman_that_isolates_its_hooks_from_the_network_gets_a_namespace_for_it_in_the_staging_root() {
    let _serial = serial();
    let host_net = fs::read_link("/proc/self/ns/net")
        .unwrap()
        .to_string_lossy()
        .into_owned();
    for sandboxed in [true, false] {
        let (_fs, store) = system("ext4");
        let root = store.root().to_path_buf();
        let dep = store.path(1);
        fs::write(dep.join("pacman/base-1/desc"), "%NAME%\nbase\n\n%VERSION%\n1-1\n").unwrap();
        let pacman = dep.join("usr/bin/pacman");
        fs::write(&pacman, NET_SANDBOX_PACMAN).unwrap();
        fs::set_permissions(&pacman, fs::Permissions::from_mode(0o755)).unwrap();
        if sandboxed {
            fs::write(dep.join("etc/net-sandbox"), "").unwrap();
        }
        let staging = Staging::new(&store, 2).unwrap();
        let tx = staging.transaction().unwrap();
        let packages = archix::packages::Packages {
            remove: vec!["base".to_string()],
            ..Default::default()
        };
        archix::packages::transact(&tx, &packages, &staging.tree("pacman"))
            .unwrap_or_else(|e| panic!("sandboxed {sandboxed}: {e:#}"));
        drop(tx);
        staging.discard().unwrap();
        let log = fs::read_to_string(root.join("var/log/pacman.log")).unwrap();
        let run: Vec<&str> = log.lines().last().unwrap().split(' ').collect();
        assert_eq!(run[0], "-Rdd", "sandboxed {sandboxed}: {log}");
        if sandboxed {
            assert_ne!(
                run[1], host_net,
                "pacman's hooks share the host's network namespace: {log}"
            );
            assert_eq!(&run[2..], ["1", "1"], "only loopback, and up: {log}");
        } else {
            assert_eq!(run[1], host_net, "pacman without the sandbox runs as before: {log}");
        }
    }
}

/// rename(2) with its errno as the exit status, unlike mv, which copies when a rename crosses devices.
const RENAME: &str = r#"
#include <errno.h>
#include <stdio.h>
int main(int argc, char **argv) { return rename(argv[1], argv[2]) == 0 ? 0 : errno; }
"#;

#[test]
#[ignore]
fn overlay_quirks_on_ext4_hardlinks_directory_renames_and_flushing() {
    let _serial = serial();
    let (_fs, store) = system("ext4");
    let usr = store.path(1).join("usr");
    fs::write(usr.join("lib/hl-a"), "a\n").unwrap();
    fs::hard_link(usr.join("lib/hl-a"), usr.join("lib/hl-b")).unwrap();
    fs::write(usr.join("lib/kept-a"), "k\n").unwrap();
    fs::hard_link(usr.join("lib/kept-a"), usr.join("lib/kept-b")).unwrap();
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("rename.c"), RENAME).unwrap();
    sh(Command::new("cc")
        .args(["-static", "-O", "-o"])
        .arg(usr.join("bin/rename2"))
        .arg(dir.path().join("rename.c")));

    let staging = Staging::new(&store, 2).unwrap();
    let fsid = |path: &Path| {
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::statfs(c.as_ptr(), &mut stat) }, 0);
        unsafe { std::mem::transmute::<libc::fsid_t, [i32; 2]>(stat.f_fsid) }
    };
    assert_eq!(
        fsid(&staging.dir().join(".upper")),
        fsid(store.dir()),
        "the commit's syncfs of the store flushes the upper layer, which overlayfs itself never fsyncs"
    );
    let tx = staging.transaction().unwrap();
    let status = tx
        .output(&["rename2", "/usr/share/doc", "/usr/share/doc2"])
        .unwrap()
        .status;
    assert_eq!(
        status.code(),
        Some(libc::EXDEV),
        "renaming a lower directory with redirect_dir=off"
    );
    in_view(
        &tx,
        "mv /usr/share/doc /usr/share/doc3; echo n > /usr/lib/new-a; ln /usr/lib/new-a /usr/lib/new-b; echo more >> /usr/lib/hl-a",
    );
    drop(tx);
    let new = staging.finish().unwrap().dir;
    let old = store.path(1);
    assert!(!new.join("usr/share/doc").exists() && !new.join("usr/share/doc2").exists());
    assert_eq!(
        fs::read_to_string(new.join("usr/share/doc3/a")).unwrap(),
        "a\n",
        "mv copies instead"
    );
    assert_eq!(
        ino(&new.join("usr/lib/new-a")),
        ino(&new.join("usr/lib/new-b")),
        "links made in staging stay links"
    );
    assert_eq!(
        ino(&new.join("usr/lib/kept-a")),
        ino(&new.join("usr/lib/kept-b")),
        "untouched links stay links"
    );
    assert_eq!(fs::read_to_string(new.join("usr/lib/hl-a")).unwrap(), "a\nmore\n");
    assert_eq!(
        fs::read_to_string(new.join("usr/lib/hl-b")).unwrap(),
        "a\n",
        "copy-up breaks a hardlink, as the merged overlay showed it, unlike an in-place write on a plain root"
    );
    assert_eq!(ino(&new.join("usr/lib/hl-b")), ino(&old.join("usr/lib/hl-b")));
    assert_eq!(
        fs::read_to_string(old.join("usr/lib/hl-a")).unwrap(),
        "a\n",
        "deployment 1 is untouched"
    );
    assert!(old.join("usr/share/doc/a").exists());
}

#[test]
#[ignore]
fn dropping_a_deployments_entries_reaches_the_disk_before_its_ukis_are_freed() {
    let _serial = serial();
    let esp = LoopFs::sized("vfat", "40M");
    let img = esp.mnt.parent().unwrap().join("fs.img");
    let dir = tempfile::tempdir().unwrap();
    for sub in ["loader/entries", "EFI/archix"] {
        fs::create_dir_all(esp.mnt.join(sub)).unwrap();
    }
    for (generation, uki) in [(4, "aaaa"), (5, "bbbb")] {
        fs::write(esp.mnt.join(format!("EFI/archix/{uki}.efi")), vec![b'u'; 1 << 20]).unwrap();
        fs::write(
            esp.mnt.join(format!("loader/entries/archix-{generation}.conf")),
            format!("title t\nefi /EFI/archix/{uki}.efi\n"),
        )
        .unwrap();
    }
    sh(Command::new("sync").arg("-f").arg(&esp.mnt));
    let listing = dir.path().join("listing");
    // The container's /tmp is noexec, so sh reads the step's script, named after the step, from the helper's directory.
    fs::write(
        dir.path().join("deploy-drop-ukis"),
        format!("fsck.fat -n -l {} > {} 2>&1\n", img.display(), listing.display()),
    )
    .unwrap();
    let out = helper(
        "drop",
        dir.path(),
        &[
            ("ARCHIX_TEST_MARK", "/bin/sh"),
            ("ARCHIX_DEPLOY_ESP", esp.mnt.to_str().unwrap()),
        ],
    )
    .current_dir(dir.path())
    .output()
    .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let on_disk = fs::read_to_string(&listing).unwrap().to_lowercase();
    assert!(
        on_disk.contains("archix-5.conf"),
        "fsck.fat listed nothing useful: {on_disk}"
    );
    assert!(
        !on_disk.contains("archix-4.conf"),
        "deployment 4's entry was still on disk when its UKI was freed:\n{on_disk}"
    );
    assert!(!esp.mnt.join("EFI/archix/aaaa.efi").exists() && esp.mnt.join("EFI/archix/bbbb.efi").exists());
}

/// The start cluster and size in `entry`'s short-name directory entry (8.3, upper case, padded) on a FAT image, if it has one.
fn fat_dir_entry(img: &Path, entry: &[u8; 11]) -> Option<(u32, u32)> {
    let bytes = fs::read(img).unwrap();
    bytes
        .as_chunks::<32>()
        .0
        .iter()
        .find(|de| &de[..11] == entry)
        .map(|de| {
            let word = |at: usize| u16::from_le_bytes([de[at], de[at + 1]]) as u32;
            (
                word(20) << 16 | word(26),
                u32::from_le_bytes(de[28..32].try_into().unwrap()),
            )
        })
}

#[test]
#[ignore]
fn an_esp_flush_writes_a_renamed_entry_whole_where_a_directory_fsync_leaves_it_empty() {
    let _serial = serial();
    let esp = LoopFs::sized("vfat", "32M");
    let img = esp.mnt.parent().unwrap().join("fs.img");
    fs::write(esp.mnt.join("entry.tmp"), "title t\n").unwrap();
    archix::durable::sync_esp(&esp.mnt).unwrap();
    assert_eq!(fat_dir_entry(&img, b"ENTRY   TMP").map(|(_, size)| size), Some(8));
    fs::rename(esp.mnt.join("entry.tmp"), esp.mnt.join("entry.cnf")).unwrap();
    fs::File::open(&esp.mnt).unwrap().sync_all().unwrap();
    assert_eq!(
        fat_dir_entry(&img, b"ENTRY   CNF"),
        Some((0, 0)),
        "vfat's rename no longer leaves the new name's entry empty until the file's inode is written"
    );
    archix::durable::sync_esp(&esp.mnt.join("entry.cnf")).unwrap();
    let (cluster, size) = fat_dir_entry(&img, b"ENTRY   CNF").unwrap();
    assert!(cluster != 0 && size == 8, "cluster {cluster}, size {size}");
}

/// Copies installed packages' files and database entries from the container into `root`, as pacstrap would have put them.
fn install_packages(root: &Path, names: &[&str]) {
    let local = Path::new("/var/lib/pacman/local");
    for name in names {
        let entry = fs::read_dir(local)
            .unwrap()
            .flatten()
            .find(|entry| {
                fs::read_to_string(entry.path().join("desc"))
                    .is_ok_and(|desc| desc.starts_with(&format!("%NAME%\n{name}\n")))
            })
            .unwrap_or_else(|| panic!("{name} isn't installed in the container"));
        sh(Command::new("cp")
            .arg("-a")
            .arg(entry.path())
            .arg(root.join("var/lib/pacman/local")));
        let files = fs::read_to_string(entry.path().join("files")).unwrap();
        let list: Vec<&str> = files
            .lines()
            .skip_while(|line| *line != "%FILES%")
            .skip(1)
            .take_while(|line| !line.is_empty())
            .collect();
        let listing = root.join(".file-list");
        fs::write(&listing, list.join("\n") + "\n").unwrap();
        sh(Command::new("sh").arg("-ec").arg(format!(
            "tar -C / --no-recursion --xattrs --xattrs-include='*' -cf - -T {list} | tar -C {root} --xattrs --xattrs-include='*' -xpf -",
            list = listing.display(),
            root = root.display()
        )));
        fs::remove_file(listing).unwrap();
    }
}

/// A root on a fresh filesystem laid out as deployments and booted from deployment 1, as eject finds one: real packages, a kernel package whose UKI a stand-in mkinitcpio writes, archix's integration and an ESP with deployment 1's entry and fallback UKI.
fn ejectable_root(fs_type: &str) -> LoopFs {
    let loop_fs = plain_root(fs_type);
    let root = loop_fs.mnt.clone();
    fs::remove_dir_all(root.join("var/lib/pacman/local/base-1")).unwrap();
    std::os::unix::fs::symlink("usr/lib", root.join("lib64")).unwrap();
    fs::copy(
        "/var/lib/pacman/local/ALPM_DB_VERSION",
        root.join("var/lib/pacman/local/ALPM_DB_VERSION"),
    )
    .unwrap();
    install_packages(&root, &["sed", "gzip"]);
    let modules = root.join("usr/lib/modules/9.9.9-test");
    fs::create_dir_all(&modules).unwrap();
    fs::write(modules.join("vmlinuz"), "kernel\n").unwrap();
    fs::write(modules.join("pkgbase"), "linux\n").unwrap();
    let kernel = root.join("var/lib/pacman/local/linux-9.9.9-1");
    fs::create_dir_all(&kernel).unwrap();
    fs::write(kernel.join("desc"), "%NAME%\nlinux\n\n%VERSION%\n9.9.9-1\n\n").unwrap();
    fs::write(
        kernel.join("files"),
        "%FILES%\nusr/\nusr/lib/\nusr/lib/modules/\nusr/lib/modules/9.9.9-test/\nusr/lib/modules/9.9.9-test/pkgbase\nusr/lib/modules/9.9.9-test/vmlinuz\n\n",
    )
    .unwrap();
    install_program(&root.join("usr"), "/usr/lib/initcpio/busybox", "busybox");
    let mkinitcpio = root.join("usr/bin/mkinitcpio");
    fs::write(
        &mkinitcpio,
        "#!/usr/bin/busybox sh\nwhile [ $# -gt 0 ]; do [ \"$1\" = -p ] && base=$2; shift; done\n\
         uki=$(/usr/bin/busybox sed -n 's/^default_uki=\"\\(.*\\)\"$/\\1/p' /etc/mkinitcpio.d/$base.preset)\n\
         echo \"uki of $base\" > \"$uki\"\n",
    )
    .unwrap();
    fs::set_permissions(&mkinitcpio, fs::Permissions::from_mode(0o755)).unwrap();
    for (path, text) in [
        (
            "etc/mkinitcpio.d/linux.preset",
            "ALL_kver=\"/boot/vmlinuz-linux\"\nPRESETS=('default')\n",
        ),
        ("etc/kernel/cmdline", "root=/dev/vda2 rw\n"),
        ("etc/pacman.conf", "[options]\nHoldPkg = pacman\nCheckSpace\n"),
    ] {
        fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
        fs::write(root.join(path), text).unwrap();
    }
    let paths = archix::paths::Paths::under(&root);
    archix::integration::write_all(&paths, Path::new("/usr/bin/archix"), &|path: &Path| {
        Ok(root.join(path.strip_prefix("/").unwrap()))
    })
    .unwrap();
    let esp = root.join("efi");
    for dir in ["loader/entries", "EFI/archix", "EFI/Linux", "EFI/BOOT"] {
        fs::create_dir_all(esp.join(dir)).unwrap();
    }
    fs::write(esp.join("EFI/archix/9.9.9-test-aaaa.efi"), "uki-1").unwrap();
    fs::write(esp.join("EFI/Linux/archix-fallback-1.efi"), "fallback-1").unwrap();
    fs::write(
        esp.join("loader/entries/archix-1.conf"),
        "title archix\nversion 1\nsort-key archix\nefi /EFI/archix/9.9.9-test-aaaa.efi\n",
    )
    .unwrap();
    fs::write(esp.join("loader/loader.conf"), "timeout 2\ndefault archix-1.conf\n").unwrap();
    fs::create_dir_all(paths.state.join("boot")).unwrap();
    fs::write(paths.state.join("boot/esp"), esp.display().to_string()).unwrap();
    let store = archix::deploy::layout::lay_out(&root).unwrap();
    archix::deploy::layout::bind_live(&root, &store, 1).unwrap();
    loop_fs
}

fn pacman_qkk(root: &Path, names: &[&str]) -> String {
    let output = Command::new("pacman")
        .arg("--root")
        .arg(root)
        .arg("--dbpath")
        .arg(root.join("var/lib/pacman"))
        .arg("-Qkk")
        .args(names)
        .output()
        .unwrap();
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Takes every deployment bind off `root`, as a restart does, then mounts what the ESP's default boots: the current deployment through `archix deploy-mount` for an archix entry, nothing for the plain UKI; returns the default.
fn restart(root: &Path) -> String {
    for (_, at) in archix::system::deploy_boot::TREES {
        while mounted(&root.join(at)) {
            sh(Command::new("umount").arg("-l").arg(root.join(at)));
        }
    }
    let _ = fs::remove_file(root.join("run/archix/deployment"));
    let conf = fs::read_to_string(root.join("efi/loader/loader.conf")).unwrap();
    let default = conf
        .lines()
        .find_map(|line| line.strip_prefix("default "))
        .unwrap_or_default()
        .to_string();
    if default.starts_with("archix-") {
        sh(Command::new(env!("CARGO_BIN_EXE_archix"))
            .arg("deploy-mount")
            .arg(root)
            .arg("--efivar")
            .arg(root.join("run/no-efivar"))
            .arg("--run-dir")
            .arg(root.join("run/archix")));
    }
    default
}

/// The deployments the ESP's archix entries and fallback UKIs name.
fn archix_boot_paths(esp: &Path) -> Vec<u32> {
    let mut named: Vec<u32> = archix::system::boot::entries(esp)
        .into_iter()
        .map(|(n, _, _)| n)
        .chain(archix::system::boot::fallbacks(esp))
        .collect();
    named.sort_unstable();
    named
}

/// The root's /usr as `snapshot` sees it, without its top directory and the deployment mount unit eject takes off.
fn plain_usr(root: &Path) -> BTreeMap<PathBuf, Entry> {
    let mut usr = snapshot(&root.join("usr"));
    for gone in ["", "lib/archix", "lib/archix/archix-deploy-mount.service"] {
        usr.remove(Path::new(gone));
    }
    usr
}

/// What a restart must find wherever an eject was cut off: a default that boots a complete deployment or the complete plain trees, and no archix boot path naming a deployment that isn't complete.
fn assert_eject_bootable(root: &Path, usr: &BTreeMap<PathBuf, Entry>, qkk: &str, what: &str) {
    let esp = root.join("efi");
    let conf = fs::read_to_string(esp.join("loader/loader.conf")).unwrap();
    let default = conf
        .lines()
        .find_map(|line| line.strip_prefix("default "))
        .unwrap_or_else(|| panic!("{what}: loader.conf has no default: {conf}"));
    let complete =
        |n: u32| archix::deploy::store::incomplete(&root.join("archix/deploy").join(n.to_string())).is_none();
    for n in archix_boot_paths(&esp) {
        assert!(
            complete(n),
            "{what}: the ESP still boots deployment {n}, which isn't complete"
        );
    }
    if let Some(n) = archix::system::deploy_boot::generation_of_entry(default) {
        assert!(
            complete(n),
            "{what}: the default {default} names an incomplete deployment"
        );
        let current = fs::read_link(root.join("archix/current")).unwrap();
        assert!(
            complete(archix::deploy::store::parse_current(&current).unwrap()),
            "{what}: current isn't complete"
        );
    } else {
        assert_eq!(default, "arch-linux.efi", "{what}");
        assert!(
            esp.join("EFI/Linux/arch-linux.efi").is_file(),
            "{what}: the plain UKI is missing"
        );
        if !mounted(&root.join("usr")) {
            assert!(
                plain_usr(root) == *usr,
                "{what}: the default boots a plain /usr that isn't the deployment's"
            );
            assert_eq!(pacman_qkk(root, &["sed", "gzip"]), qkk, "{what}");
        }
    }
}

/// What a finished eject leaves: the deployment's packages on the root's own trees, archix's integration and boot paths gone, the plain UKI the default and no /archix.
fn assert_ejected(root: &Path, usr: &BTreeMap<PathBuf, Entry>, qkk: &str, what: &str) {
    let esp = root.join("efi");
    assert!(!root.join("archix").exists(), "{what}: /archix is still there");
    for (_, at) in archix::system::deploy_boot::TREES {
        assert!(!mounted(&root.join(at)), "{what}: {at} is still bound");
    }
    assert!(
        plain_usr(root) == *usr,
        "{what}: the root's /usr isn't the deployment's"
    );
    assert_eq!(pacman_qkk(root, &["sed", "gzip"]), qkk, "{what}");
    assert!(
        qkk.contains("sed: ") && qkk.contains("gzip: ") && !qkk.contains("error:"),
        "{what}: {qkk}"
    );
    assert!(root.join("var/lib/pacman/local/linux-9.9.9-1/desc").is_file(), "{what}");
    for gone in [
        "etc/initcpio/hooks/archix-deploy",
        "etc/mkinitcpio.conf.d/archix-deploy.conf",
        "etc/pacman.d/hooks/00-archix-deployment.hook",
        "etc/pacman.d/hooks/archix.hook",
        "etc/systemd/system/archix-boot.service",
        "etc/systemd/system/sysinit.target.wants/archix-deploy-activate.service",
        "usr/lib/archix/archix-deploy-mount.service",
        "boot/archix",
    ] {
        assert!(
            root.join(gone).symlink_metadata().is_err(),
            "{what}: {gone} is still there"
        );
    }
    let conf = fs::read_to_string(root.join("etc/pacman.conf")).unwrap();
    assert!(conf.lines().any(|line| line == "CheckSpace"), "{what}: {conf}");
    assert!(
        root.join("usr/lib/archix/archix-static").is_file(),
        "{what}: the package's own files stay"
    );
    assert_eq!(archix_boot_paths(&esp), Vec::<u32>::new(), "{what}");
    assert!(!esp.join("EFI/archix").exists(), "{what}");
    assert!(esp.join("EFI/Linux/arch-linux.efi").is_file(), "{what}");
    let loader = fs::read_to_string(esp.join("loader/loader.conf")).unwrap();
    assert!(loader.contains("default arch-linux.efi\n"), "{what}: {loader}");
    let preset = fs::read_to_string(root.join("etc/mkinitcpio.d/linux.preset")).unwrap();
    assert!(
        preset.contains("default_uki=\"/efi/EFI/Linux/arch-linux.efi\""),
        "{what}: {preset}"
    );
}

#[test]
#[ignore]
fn eject_lays_the_running_deployment_out_as_the_plain_root_and_removes_archix() {
    let _serial = serial();
    for fs_type in ["ext4", "btrfs"] {
        let loop_fs = ejectable_root(fs_type);
        let root = loop_fs.mnt.clone();
        let usr = plain_usr(&root);
        let qkk = pacman_qkk(&root, &["sed", "gzip"]);
        let out = helper("eject", &root, &[]).output().unwrap();
        assert!(out.status.success(), "{fs_type}: {}", stderr(&out));
        assert_ejected(&root, &usr, &qkk, fs_type);
        let state = archix::deploy::eject::read_state(&root.join("var/lib/archix")).unwrap();
        assert_eq!(
            state.map(|state| state.phase),
            Some(archix::deploy::eject::Phase::Plain)
        );
        assert_eq!(restart(&root), "arch-linux.efi");
        assert_ejected(&root, &usr, &qkk, &format!("{fs_type}, restarted"));
    }
}

#[test]
#[ignore]
fn an_eject_killed_at_any_step_leaves_a_bootable_system_and_a_rerun_finishes_it() {
    let _serial = serial();
    for fs_type in ["ext4", "btrfs"] {
        for step in [
            "eject-copying",
            "eject-copied",
            "eject-loader",
            "eject-plain",
            "eject-unbound",
            "eject-deleting",
            "eject-removed",
        ] {
            let what = format!("{fs_type}, killed at {step}");
            let loop_fs = ejectable_root(fs_type);
            let root = loop_fs.mnt.clone();
            let usr = plain_usr(&root);
            let qkk = pacman_qkk(&root, &["sed", "gzip"]);
            let killed = helper("eject", &root, &[("ARCHIX_TEST_CRASH", step)]).output().unwrap();
            assert!(!killed.status.success(), "{what}: the eject never got there");
            assert_eject_bootable(&root, &usr, &qkk, &what);
            let booted = restart(&root);
            assert_eject_bootable(&root, &usr, &qkk, &format!("{what}, restarted into {booted}"));
            let rerun = helper("eject", &root, &[]).output().unwrap();
            assert!(rerun.status.success(), "{what}, then a rerun: {}", stderr(&rerun));
            assert_ejected(&root, &usr, &qkk, &format!("{what}, then a rerun"));
        }
    }
}

#[test]
#[ignore]
fn a_switch_is_refused_before_staging_when_the_root_has_too_few_inodes_left() {
    let _serial = serial();
    let (loop_fs, store) = system_on(LoopFs::made("ext4", "64M", &["-N", "8192"]), "ext4");
    let dep = store.path(1);
    let db = dep.join("pacman");
    archix::conflicts::check(&[], &[], &db, &dep).unwrap();
    let filler = loop_fs.mnt.join("filler");
    fs::create_dir(&filler).unwrap();
    let mut made = 0;
    loop {
        match fs::File::create(filler.join(made.to_string())) {
            Ok(_) => made += 1,
            Err(e) => {
                assert_eq!(e.raw_os_error(), Some(libc::ENOSPC), "{e}");
                break;
            }
        }
    }
    for n in 0..64 {
        fs::remove_file(filler.join(n.to_string())).unwrap();
    }
    let listing = || -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(store.deploy_dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    let before = listing();
    let err =
        archix::conflicts::check(&[], &[], &db, &dep).expect_err("a root without inodes to stage passed the preflight");
    let text = format!("{err:#}");
    assert!(
        text.contains("free inodes") && text.contains("nothing was changed"),
        "{text}"
    );
    assert_eq!(listing(), before, "the refused check left something in the store");
    assert_eq!(store.current().unwrap(), Some(1));
}

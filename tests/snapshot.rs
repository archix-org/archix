//! archix's own btrfs snapshots and the root's layout for rollbacks, on loop btrfs images; run only inside the Arch container via `tests/arch/run.sh`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use archix::snapshot::{KEEP_RESERVE, Kind, Meta, Policy, Space, Store, layout};

fn sh(command: &str) -> String {
    let output = Command::new("sh").arg("-c").arg(command).output().unwrap();
    assert!(
        output.status.success(),
        "{command}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A btrfs laid out as CachyOS does, `@` at the top level, mounted: the top level at `top`, `@` at `root`.
struct Disk {
    _dir: tempfile::TempDir,
    top: PathBuf,
    root: PathBuf,
}

impl Disk {
    fn new(size: &str) -> Disk {
        let dir = tempfile::TempDir::new().unwrap();
        let (image, top, root) = (
            dir.path().join("disk.img"),
            dir.path().join("top"),
            dir.path().join("root"),
        );
        fs::create_dir_all(&top).unwrap();
        fs::create_dir_all(&root).unwrap();
        sh(&format!("truncate -s {size} {0} && mkfs.btrfs -q {0}", image.display()));
        sh(&format!("mount -o loop {} {}", image.display(), top.display()));
        sh(&format!("btrfs -q subvolume create {}/@", top.display()));
        let device = sh(&format!("findmnt -n -o SOURCE {}", top.display()));
        sh(&format!("mount -o subvol=/@ {} {}", device.trim(), root.display()));
        Disk { _dir: dir, top, root }
    }

    /// Mounts @archix at <root>/.archix, as setup's fstab line does at boot.
    fn mount_archix(&self) {
        let device = sh(&format!("findmnt -n -o SOURCE {}", self.top.display()));
        let device = device.trim().split('[').next().unwrap().to_string();
        if !self.top.join(layout::ARCHIX).exists() {
            sh(&format!(
                "btrfs -q subvolume create {}/{}",
                self.top.display(),
                layout::ARCHIX
            ));
        }
        fs::create_dir_all(self.root.join(".archix")).unwrap();
        sh(&format!(
            "mount -o subvol=/{} {device} {}/.archix",
            layout::ARCHIX,
            self.root.display()
        ));
    }
}

impl Drop for Disk {
    fn drop(&mut self) {
        let _ = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "umount -R {0} ; umount -R {1}",
                self.root.display(),
                self.top.display()
            ))
            .status();
    }
}

fn read_only(path: &Path) -> bool {
    sh(&format!("btrfs property get -ts {} ro", path.display())).trim() == "ro=true"
}

fn numbers(store: &Store) -> Vec<u32> {
    store.list().unwrap().iter().map(|record| record.number).collect()
}

#[test]
#[ignore]
fn a_snapshot_is_read_only_with_its_record_and_a_cut_short_one_is_cleaned_up_and_its_number_reused() {
    let disk = Disk::new("1G");
    disk.mount_archix();
    fs::write(disk.root.join("marker"), "one").unwrap();
    let store = Store::open(&disk.root);
    let meta = Meta {
        tree: Some("abc123".into()),
        label: "before switch to tree def456".into(),
        note: Some("before the upgrade".into()),
        ..Meta::default()
    };
    let first = store.create(&disk.root, Kind::Switch, meta).unwrap();
    assert_eq!(
        (first.number, first.kind, first.tree.as_deref()),
        (1, Kind::Switch, Some("abc123"))
    );
    assert_eq!(store.get(1).unwrap().unwrap().label, "before switch to tree def456");
    assert!(read_only(&store.path(1)), "a snapshot is read-only");
    assert_eq!(fs::read_to_string(store.path(1).join("marker")).unwrap(), "one");
    assert!(
        !store.path(1).join(".archix/snapshots").join("1").exists(),
        "the snapshots live on @archix, so a snapshot of @ doesn't hold them"
    );
    fs::write(disk.root.join("marker"), "two").unwrap();
    store.create(&disk.root, Kind::Pacman, Meta::default()).unwrap();
    assert_eq!(fs::read_to_string(store.path(1).join("marker")).unwrap(), "one");
    assert_eq!(fs::read_to_string(store.path(2).join("marker")).unwrap(), "two");
    // What a crash between the snapshot and its record leaves.
    fs::create_dir_all(store.dir().join("3")).unwrap();
    sh(&format!(
        "btrfs -q subvolume snapshot -r {} {}",
        disk.root.display(),
        store.path(3).display()
    ));
    assert_eq!(numbers(&store), [1, 2], "a snapshot without its record isn't listed");
    let next = store.create(&disk.root, Kind::Manual, Meta::default()).unwrap();
    assert_eq!(next.number, 3, "the leftover was cleaned up and its number taken again");
    assert!(store.get(3).unwrap().is_some_and(|record| record.kind == Kind::Manual));
    store.delete(2).unwrap();
    assert_eq!(numbers(&store), [1, 3]);
    assert!(!store.dir().join("2").exists(), "deleting takes the subvolume too");
    let subvolumes = sh(&format!(
        "btrfs subvolume list -o {}",
        disk.top.join(layout::ARCHIX).display()
    ));
    assert!(!subvolumes.contains("snapshots/2/"), "{subvolumes}");
}

#[test]
#[ignore]
fn prune_keeps_each_kinds_newest_and_never_drops_manual_pinned_or_the_newest_snapshot_for_space() {
    let disk = Disk::new("1G");
    disk.mount_archix();
    let store = Store::open(&disk.root);
    for kind in [
        Kind::Switch,
        Kind::Pacman,
        Kind::Manual,
        Kind::Switch,
        Kind::Pacman,
        Kind::Switch,
        Kind::Pacman,
    ] {
        store.create(&disk.root, kind, Meta::default()).unwrap();
    }
    let policy = Policy {
        keep_switch: 2,
        keep_pacman: 2,
        min_free: 10,
        entries: 5,
        ..Policy::default()
    };
    let plenty = |percent: f64| {
        Some(Space {
            percent,
            bytes: 64 * KEEP_RESERVE,
        })
    };
    let pruned = store.prune(&policy, &BTreeSet::from([1]), || plenty(50.0)).unwrap();
    assert_eq!(
        pruned.deleted,
        [2],
        "switch 1 is pinned by its boot entry; pacman 2 is over the count"
    );
    assert_eq!(numbers(&store), [1, 3, 4, 5, 6, 7]);
    let free = std::cell::Cell::new(0.0);
    let pruned = store
        .prune(&policy, &BTreeSet::from([1]), || {
            free.set(free.get() + 3.0);
            plenty(free.get())
        })
        .unwrap();
    assert_eq!(
        pruned.deleted,
        [4, 5, 6],
        "short of space it drops the oldest it may until min_free is back"
    );
    assert_eq!(pruned.short, None);
    let pruned = store.prune(&policy, &BTreeSet::from([1]), || plenty(1.0)).unwrap();
    assert_eq!(numbers(&store), [1, 3, 7], "manual 3, pinned 1 and the newest, 7, stay");
    assert!(pruned.deleted.is_empty());
    assert_eq!(pruned.short, plenty(1.0), "it says how short it stayed");
}

#[test]
#[ignore]
fn the_keep_reserve_prunes_a_big_disk_whose_percentage_is_fine_but_whose_bytes_arent() {
    let disk = Disk::new("1G");
    disk.mount_archix();
    let store = Store::open(&disk.root);
    for _ in 0..4 {
        store.create(&disk.root, Kind::Switch, Meta::default()).unwrap();
    }
    let policy = Policy {
        keep_switch: 10,
        keep_pacman: 10,
        min_free: 10,
        entries: 5,
        ..Policy::default()
    };
    let calls = std::cell::Cell::new(0u64);
    let pruned = store
        .prune(&policy, &BTreeSet::new(), || {
            calls.set(calls.get() + 1);
            Some(Space {
                percent: 80.0,
                bytes: calls.get() * KEEP_RESERVE / 2,
            })
        })
        .unwrap();
    assert_eq!(
        pruned.deleted,
        [1],
        "80% free but half a keep's room: the oldest goes, then a whole keep's room is free"
    );
    assert_eq!(numbers(&store), [2, 3, 4]);
}

#[test]
#[ignore]
fn refetchable_packages_leave_archix_s_cache_only_once_no_snapshot_may_go() {
    let disk = Disk::new("1G");
    disk.mount_archix();
    let paths = archix::paths::Paths::under(&disk.root);
    let store = Store::open(&disk.root);
    let locked = |file: &str| archix::lock::Locked {
        version: "1-1".into(),
        repo: "core".into(),
        file: file.into(),
        ..Default::default()
    };
    let old = std::collections::BTreeMap::from([("zlib".to_string(), locked("zlib-old.pkg.tar.zst"))]);
    let now = std::collections::BTreeMap::from([("zlib".to_string(), locked("zlib-new.pkg.tar.zst"))]);
    for packages in [old.clone(), old.clone(), now.clone()] {
        store
            .create(
                &disk.root,
                Kind::Switch,
                Meta {
                    packages,
                    ..Meta::default()
                },
            )
            .unwrap();
    }
    let cached = |file: &str| paths.packages().join(file);
    fs::create_dir_all(paths.packages()).unwrap();
    fs::write(cached("zlib-old.pkg.tar.zst"), "old").unwrap();
    fs::write(cached("zlib-new.pkg.tar.zst"), "new").unwrap();
    let policy = Policy::default();
    let seen = std::cell::RefCell::new(Vec::new());
    let freed = archix::snapshot::prune_root(&paths, &policy, &now, &BTreeSet::new(), || {
        seen.borrow_mut()
            .push((numbers(&store).len(), cached("zlib-old.pkg.tar.zst").exists()));
        Some(Space { percent: 1.0, bytes: 0 })
    });
    assert_eq!(freed.deleted, [1, 2], "every snapshot but the newest goes first");
    let seen = seen.into_inner();
    assert!(
        seen.iter().filter(|(left, _)| *left > 1).all(|(_, cached)| *cached),
        "the old package stays while a snapshot may still go: {seen:?}"
    );
    assert!(!cached("zlib-old.pkg.tar.zst").exists(), "then it goes");
    assert!(cached("zlib-new.pkg.tar.zst").exists(), "the current lock's stays");
    assert_eq!(freed.dropped, 3);
    assert!(freed.warnings[0].contains("rollback.min_free"), "{:?}", freed.warnings);
}

fn uuid_of(path: &Path) -> String {
    let show = sh(&format!("btrfs subvolume show {}", path.display()));
    show.lines()
        .find_map(|line| line.trim().strip_prefix("UUID:"))
        .unwrap()
        .trim()
        .to_string()
}

/// A root with snapshot 1 taken while pacman held its lock, then changed to "broken".
fn kept_disk() -> (Disk, Store) {
    let disk = Disk::new("1G");
    disk.mount_archix();
    fs::create_dir_all(disk.root.join("var/rollback/pacman")).unwrap();
    fs::write(disk.root.join("var/rollback/pacman/db.lck"), "").unwrap();
    fs::write(disk.root.join("marker"), "old").unwrap();
    let store = Store::open(&disk.root);
    store.create(&disk.root, Kind::Switch, Meta::default()).unwrap();
    fs::write(disk.root.join("marker"), "broken").unwrap();
    (disk, store)
}

fn replaced() -> Meta {
    Meta {
        tree: Some("cafe".into()),
        label: "replaced by keeping 1".into(),
        ..Meta::default()
    }
}

#[test]
#[ignore]
fn keep_swaps_a_clone_of_the_snapshot_into_ats_name_and_files_the_replaced_root_as_a_snapshot() {
    let (disk, store) = kept_disk();
    let before = sh(&format!("btrfs subvolume get-default {}", disk.top.display()));
    let old_uuid = uuid_of(&disk.top.join("@"));
    assert!(
        store.keep(9, &disk.top, replaced(), |_| Ok(())).is_err(),
        "no snapshot 9"
    );
    let number = store
        .keep(1, &disk.top, replaced(), |clone| {
            fs::write(clone.join("prepared"), "accounts").map_err(Into::into)
        })
        .unwrap();
    assert_eq!(number, 2);
    let root = disk.top.join("@");
    assert_eq!(
        fs::read_to_string(root.join("marker")).unwrap(),
        "old",
        "@ is now the clone of 1"
    );
    assert!(
        root.join("prepared").exists(),
        "prepare ran on the clone before the swap"
    );
    assert!(
        !root.join("var/rollback/pacman/db.lck").exists(),
        "pacman's lock from the snapshot is gone"
    );
    assert!(!root.join(".archix-keep").exists() && !disk.top.join("@archix-keep").exists());
    assert!(!read_only(&root), "the new root is writable");
    assert_eq!(
        fs::read_to_string(store.path(1).join("marker")).unwrap(),
        "old",
        "snapshot 1 stays"
    );
    let record = store.get(2).unwrap().unwrap();
    assert_eq!(
        (record.kind, record.tree.as_deref(), record.label.as_str()),
        (Kind::Replaced, Some("cafe"), "replaced by keeping 1")
    );
    assert_eq!(fs::read_to_string(store.path(2).join("marker")).unwrap(), "broken");
    assert_eq!(
        uuid_of(&store.path(2)),
        old_uuid,
        "the replaced snapshot is the old root itself"
    );
    assert_eq!(
        fs::read_to_string(disk.root.join("marker")).unwrap(),
        "broken",
        "the running root stays mounted as it was until the reboot"
    );
    assert_eq!(
        sh(&format!("btrfs subvolume get-default {}", disk.top.display())),
        before,
        "no set-default"
    );
    let recorded = fs::read_to_string(disk.root.join(".archix/root.json")).unwrap();
    assert!(recorded.contains(&uuid_of(&root)), "{recorded}");
}

/// Runs keep in a copy of this test binary that ARCHIX_TEST_CRASH kills at `point`, as a power cut would.
fn keep_killed_at(disk: &Disk, point: &str) {
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--include-ignored", "--exact", "keep_in_a_child", "--nocapture"])
        .env("ARCHIX_TEST_CRASH", point)
        .env("ARCHIX_TEST_KEEP_ROOT", &disk.root)
        .env("ARCHIX_TEST_KEEP_TOP", &disk.top)
        .status()
        .unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        status.signal(),
        Some(libc::SIGKILL),
        "keep should have been killed at {point}: {status}"
    );
}

/// Not a test of its own: what keep_killed_at runs.
#[test]
#[ignore]
fn keep_in_a_child() {
    let (Some(root), Some(top)) = (
        std::env::var_os("ARCHIX_TEST_KEEP_ROOT"),
        std::env::var_os("ARCHIX_TEST_KEEP_TOP"),
    ) else {
        return;
    };
    let _ = Store::open(Path::new(&root)).keep(1, Path::new(&top), replaced(), |_| Ok(()));
}

#[test]
#[ignore]
fn a_keep_cut_short_before_the_swap_is_undone_and_after_it_finished() {
    let (disk, store) = kept_disk();
    keep_killed_at(&disk, "keep-before-exchange");
    assert!(disk.top.join("@archix-keep").exists(), "the clone was left");
    store.recover_keep(&disk.top).unwrap();
    assert!(!disk.top.join("@archix-keep").exists(), "the clone is gone");
    assert_eq!(
        fs::read_to_string(disk.top.join("@/marker")).unwrap(),
        "broken",
        "@ is untouched"
    );
    assert_eq!(numbers(&store), [1]);
    for point in ["keep-exchanged", "keep-replaced-moved"] {
        let (disk, store) = kept_disk();
        keep_killed_at(&disk, point);
        assert_eq!(
            fs::read_to_string(disk.top.join("@/marker")).unwrap(),
            "old",
            "{point}: swapped"
        );
        store.recover_keep(&disk.top).unwrap();
        assert_eq!(numbers(&store), [1, 2], "{point}");
        assert_eq!(store.get(2).unwrap().unwrap().kind, Kind::Replaced);
        assert_eq!(fs::read_to_string(store.path(2).join("marker")).unwrap(), "broken");
        assert!(
            !disk.top.join("@archix-keep").exists() && !disk.top.join("@/.archix-keep").exists(),
            "{point}"
        );
        assert!(!store.dir().join("keep.json").exists(), "{point}");
    }
}

#[test]
#[ignore]
fn rollbacks_are_refused_on_ext4_and_on_btrfs_whose_root_is_the_top_level() {
    let disk = Disk::new("1G");
    assert_eq!(archix::snapshot::unsupported(&disk.root), None);
    let top = archix::snapshot::unsupported(&disk.top).unwrap();
    assert!(top.contains("top level"), "{top}");
    let dir = tempfile::TempDir::new().unwrap();
    let (image, mount) = (dir.path().join("ext4.img"), dir.path().join("mnt"));
    fs::create_dir_all(&mount).unwrap();
    sh(&format!(
        "truncate -s 64M {0} && mkfs.ext4 -q {0} && mount -o loop {0} {1}",
        image.display(),
        mount.display()
    ));
    let ext4 = archix::snapshot::unsupported(&mount);
    sh(&format!("umount {}", mount.display()));
    assert!(ext4.unwrap().contains("isn't on btrfs"));
}

/// A root with pacman's database (this container's own), DKMS's and archix's state, and a service's data under /var/lib.
fn populate(root: &Path) {
    let lib = root.join("var/lib");
    fs::create_dir_all(&lib).unwrap();
    sh(&format!("cp -a /var/lib/pacman {}/pacman", lib.display()));
    fs::create_dir_all(lib.join("dkms/nvidia")).unwrap();
    fs::write(lib.join("dkms/nvidia/source"), "dkms").unwrap();
    fs::create_dir_all(lib.join("archix")).unwrap();
    fs::write(lib.join("archix/notes"), "archix").unwrap();
    fs::create_dir_all(lib.join("postgres/data")).unwrap();
    fs::write(lib.join("postgres/data/PG_VERSION"), "17").unwrap();
    fs::write(lib.join("random-seed"), "seed").unwrap();
    fs::create_dir_all(root.join("etc")).unwrap();
    fs::write(root.join("etc/fstab"), "UUID=x / btrfs subvol=/@ 0 0\n").unwrap();
}

#[test]
#[ignore]
fn setup_leaves_var_lib_to_the_next_boot_whose_split_keeps_what_rolls_back_on_the_root_and_pacman_reads_it_through_the_bind()
 {
    let disk = Disk::new("2G");
    populate(&disk.root);
    let packages = sh("pacman -Q | wc -l");
    let state = layout::setup(&disk.root, &disk.top, "abcd", "rw,noatime,subvol=/@").unwrap();
    assert_eq!(state, layout::Setup::AtNextBoot);
    let recorded = fs::read_to_string(disk.top.join("@archix/root.json")).unwrap();
    assert!(
        recorded.contains(&uuid_of(&disk.top.join("@"))),
        "setup records @'s UUID: {recorded}"
    );
    assert!(disk.top.join(layout::ARCHIX).is_dir() && disk.top.join(layout::VAR_LIB).is_dir());
    let fstab = fs::read_to_string(disk.root.join("etc/fstab")).unwrap();
    assert!(
        fstab.contains("UUID=abcd /var/lib btrfs rw,noatime,subvol=/@varlib 0 0"),
        "{fstab}"
    );
    assert!(
        fstab.contains("/var/rollback/pacman /var/lib/pacman none bind"),
        "{fstab}"
    );
    let unit = fs::read_to_string(disk.root.join("etc/systemd/system").join(layout::SPLIT_UNIT)).unwrap();
    assert!(unit.contains("Before=var-lib.mount"), "{unit}");
    assert!(
        disk.root
            .join("etc/systemd/system/var-lib.mount.requires")
            .join(layout::SPLIT_UNIT)
            .symlink_metadata()
            .is_ok()
    );
    assert!(
        disk.root.join("var/lib/postgres/data/PG_VERSION").is_file(),
        "nothing moves on the running system"
    );
    let var_lib = disk.top.join(layout::VAR_LIB);
    // What a crash halfway through the copy of postgres leaves.
    fs::create_dir_all(var_lib.join("postgres")).unwrap();
    fs::write(var_lib.join("postgres/partial"), "").unwrap();
    layout::split_var_lib(&disk.root, &var_lib).unwrap();
    assert_eq!(
        fs::read_to_string(var_lib.join("postgres/data/PG_VERSION")).unwrap(),
        "17"
    );
    assert!(
        !var_lib.join("postgres/partial").exists(),
        "a cut-short copy is made again whole"
    );
    assert_eq!(fs::read_to_string(var_lib.join("random-seed")).unwrap(), "seed");
    assert!(!disk.root.join("var/lib/postgres").exists(), "the data left the root");
    assert_eq!(
        fs::read_to_string(disk.root.join("var/rollback/dkms/nvidia/source")).unwrap(),
        "dkms"
    );
    assert_eq!(
        fs::read_to_string(disk.root.join("var/rollback/archix/notes")).unwrap(),
        "archix"
    );
    for name in layout::ROLLED {
        for dir in [var_lib.join(name), disk.root.join("var/lib").join(name)] {
            assert!(dir.join("db.lck").is_file(), "{} holds a permanent lock", dir.display());
            assert!(
                fs::read_to_string(dir.join("README"))
                    .unwrap()
                    .contains("archix chroot")
            );
            assert!(!dir.join("local").exists());
        }
    }
    let uncovered = Command::new("pacman")
        .args(["--root", &disk.root.to_string_lossy(), "-Sy"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&uncovered.stderr).contains("unable to lock database"),
        "pacman on the root without its binds refuses: {}",
        String::from_utf8_lossy(&uncovered.stderr)
    );
    assert!(!disk.root.join("var/rollback/.split-pending").exists());
    layout::split_var_lib(&disk.root, &var_lib).unwrap();
    assert_eq!(
        fs::read_to_string(var_lib.join("postgres/data/PG_VERSION")).unwrap(),
        "17",
        "running it again changes nothing"
    );
    assert_eq!(
        layout::setup(&disk.root, &disk.top, "abcd", "rw,noatime,subvol=/@").unwrap(),
        layout::Setup::Done
    );
    // The mounts the fstab lines make at boot.
    let device = sh(&format!("findmnt -n -o SOURCE {}", disk.top.display()));
    let device = device.trim().split('[').next().unwrap().to_string();
    sh(&format!(
        "mount -o subvol=/{} {device} {}/var/lib",
        layout::VAR_LIB,
        disk.root.display()
    ));
    sh(&format!(
        "mount --bind {0}/var/rollback/pacman {0}/var/lib/pacman",
        disk.root.display()
    ));
    assert_eq!(
        fs::read_to_string(disk.root.join("var/lib/postgres/data/PG_VERSION")).unwrap(),
        "17"
    );
    let through = sh(&format!("pacman --root {} -Q | wc -l", disk.root.display()));
    assert_eq!(through, packages, "pacman reads its database through the bind");
    assert!(
        !disk.root.join("var/lib/pacman/db.lck").exists(),
        "the bound database has no lock of its own"
    );
    let chroot = disk.top.parent().unwrap().join("chroot");
    layout::mount_all(&device, &chroot).unwrap();
    let mounted = (
        fs::read_to_string(chroot.join("var/lib/postgres/data/PG_VERSION")),
        chroot.join("var/lib/pacman/local").is_dir(),
        chroot.join("var/lib/dkms/nvidia/source").is_file(),
        layout::subvolume_of(&chroot.join(".archix")),
    );
    sh(&format!("umount -R {}", chroot.display()));
    assert_eq!(mounted.0.unwrap(), "17", "archix chroot mounts @varlib");
    assert!(mounted.1 && mounted.2, "and the binds");
    assert_eq!(mounted.3.as_deref(), Some("/@archix"));
    let store = Store::open(&disk.root);
    disk.mount_archix();
    store.create(&disk.root, Kind::Manual, Meta::default()).unwrap();
    let snapshot = store.path(1);
    assert!(
        snapshot.join("var/rollback/pacman/local").is_dir(),
        "a snapshot holds pacman's database"
    );
    assert!(
        !snapshot.join("var/lib/postgres").exists(),
        "and not the services' data"
    );
}

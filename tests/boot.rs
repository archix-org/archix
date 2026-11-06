//! The boot menu, its images and try mode on test machines' ESPs; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::*;

/// systemd-boot 262's LoaderFeatures, so entries get boot counters.
const COUNTING: (&str, &str) = ("ARCHIX_TEST_LOADER_FEATURES", "4194303");
const ROLLBACKS: &str = "rollback = { enable = true, keep_switch = 1, entries = 2 }";

fn entry_id(file: &str) -> String {
    match file.split_once('+') {
        Some((stem, _)) => format!("{stem}.conf"),
        None => file.to_string(),
    }
}

fn entry_text(machine: &Machine, id: &str) -> String {
    let file = machine
        .entries()
        .into_iter()
        .find(|file| entry_id(file) == id)
        .unwrap_or_else(|| panic!("no entry {id} in {:?}", machine.entries()));
    fs::read_to_string(machine.esp().join("loader/entries").join(file)).unwrap()
}

fn image_of(machine: &Machine, id: &str) -> String {
    entry_text(machine, id)
        .lines()
        .find_map(|line| line.strip_prefix("uki ").or_else(|| line.strip_prefix("efi ")))
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_string()
}

fn images(machine: &Machine) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(machine.esp().join("EFI/archix"))
        .into_iter()
        .flatten()
        .map(|file| file.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn sh(script: &str) -> String {
    let output = Command::new("bash").args(["-ec", script]).output().unwrap();
    assert!(
        output.status.success(),
        "{script}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
#[ignore]
fn status_says_when_systemd_boot_is_too_old_to_count_boots() {
    let _system = lock_system();
    // Every LoaderFeatures bit below Type #1 UKI entries, as systemd-boot 257 reports them, and every one 262 has.
    let old = ("ARCHIX_TEST_LOADER_FEATURES", "65535");
    let setup = Setup::new(&["tree"], "");
    setup.first_switch();
    let status_old = setup.ok_env(&[old], &["status"]);
    let status_new = setup.ok_env(&[COUNTING], &["status"]);
    let warning = "boot counting is off: the systemd-boot that started this boot is older than 260";
    assert!(status_old.contains(warning), "{status_old}");
    assert!(!status_new.contains("boot counting is off"), "{status_new}");
}

#[test]
#[ignore]
fn status_in_the_fallbacks_try_boot_says_why_the_current_entry_failed_its_tries() {
    let _system = lock_system();
    let checks = Checks::default();
    let setup = on_btrfs(ROLLBACKS);
    let machine = &setup.machine;
    setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    setup.set_body(
        &[],
        &format!(r#"{ROLLBACKS}, files = {{ ["/etc/archix-boot-test"] = {{ text = "2" }} }}"#),
    );
    setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    let run = tempfile::tempdir().unwrap();
    let trying = archix::system::initrd::Trying {
        snapshot: 2,
        why: None,
        layer: "file".into(),
        size: 1 << 30,
    };
    fs::write(run.path().join("try"), serde_json::to_string(&trying).unwrap()).unwrap();
    let boot = machine.root.join(".archix/boot");
    fs::create_dir_all(&boot).unwrap();
    let missing = archix::system::emergency::MountFailure {
        entry: "archix-current.conf".into(),
        mounts: vec![archix::system::emergency::Unmounted {
            point: "/home".into(),
            device: "UUID=h".into(),
            attached: false,
        }],
        at: 1_700_000_000,
    };
    fs::write(boot.join("mount-failed.json"), serde_json::to_string(&missing).unwrap()).unwrap();
    let fstab = machine.root.join("etc/fstab");
    let wrote = fs::read_to_string(&fstab).unwrap();
    fs::write(&fstab, format!("{wrote}/swap/swapfile none swap nofail 0 0\n")).unwrap();
    let env = [COUNTING, ("ARCHIX_TEST_RUN", run.path().to_str().unwrap())];
    let status = setup.ok_env(&env, &["status"]);
    checks.check(
        !status.contains("/etc/fstab was edited"),
        format!("the try boot's fstab, the snapshot's edited for the overlay, isn't drift: {status}"),
    );
    checks.check(
        status.contains("this boot is snapshot 2, read-only"),
        format!("status names the try boot: {status}"),
    );
    checks.check(
        status.contains(
            "archix-current.conf failed its boot tries because /home (UUID=h) was missing, not because of the system",
        ),
        format!("and blames the missing disk, as the login note does: {status}"),
    );
    fs::remove_file(boot.join("mount-failed.json")).unwrap();
    let failed = archix::system::health::Failure {
        entry: "archix-current.conf".into(),
        target: "\"no unit failed since the last good boot\"".into(),
        why: "archix-secrets.service failed".into(),
        at: 1_700_000_000,
    };
    fs::write(boot.join("health-failed.json"), serde_json::to_string(&failed).unwrap()).unwrap();
    let status = setup.ok_env(&env, &["status"]);
    checks.check(
        status.contains("the boot of archix-current.conf failed the health check")
            && status.contains("archix-secrets.service failed"),
        format!("or the health check that failed: {status}"),
    );
    checks.finish();
}

#[test]
#[ignore]
fn a_switch_gives_the_current_entry_its_tries_and_shares_one_image_until_the_initramfs_changes() {
    let _system = lock_system();
    let checks = Checks::default();
    let setup = Setup::new(&[], "");
    let machine = &setup.machine;
    let switch = || setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    switch();
    checks.eq(
        machine.entries(),
        vec!["archix-current+2.conf".to_string()],
        "one counted entry",
    );
    let first = image_of(machine, "archix-current.conf");
    checks.eq(images(machine), vec![first.clone()], "its image is on the ESP");
    let text = entry_text(machine, "archix-current.conf");
    checks.check(
        text.contains("sort-key archix-0\n") && text.contains(&format!("uki /EFI/archix/{first}\n")),
        format_args!("a uki line, which systemd-boot counts, and the first sort key: {text}"),
    );
    let loader = fs::read_to_string(machine.esp().join("loader/loader.conf")).unwrap_or_default();
    checks.check(
        loader.contains("preferred archix-*\n") && loader.contains("default archix-current.conf\n"),
        format_args!("loader.conf prefers archix's entries in sort-key order: {loader}"),
    );
    checks.eq(
        fs::read_to_string("/var/lib/archix/boot/entries/archix-current.conf").ok(),
        Some(text.clone()),
        "the entry is recorded as written",
    );
    let entries = machine.esp().join("loader/entries");
    fs::rename(
        entries.join("archix-current+2.conf"),
        entries.join("archix-current.conf"),
    )
    .unwrap();
    setup.set_body(&[], r#"files = { ["/etc/archix-boot-test"] = { text = "one" } }"#);
    switch();
    checks.eq(
        machine.entries(),
        vec!["archix-current+2.conf".to_string()],
        "a change gives the blessed entry its tries again",
    );
    checks.eq(
        images(machine),
        vec![first.clone()],
        "the same kernel and initramfs share the image, built and signed once",
    );
    setup.set_body(
        &[],
        r#"files = { ["/etc/archix-boot-test"] = { text = "one" }, ["/etc/mkinitcpio.conf.d/zz-archix-test.conf"] = { text = "FILES=(/etc/archix-boot-test)" } }"#,
    );
    switch();
    let second = image_of(machine, "archix-current.conf");
    checks.check(second != first, "a new initramfs makes a new image");
    checks.eq(
        images(machine),
        vec![second],
        "no entry starts the old one, so it's gone",
    );
    checks.finish();
}

fn on_btrfs(body: &str) -> Setup {
    let machine = Machine::on_btrfs();
    machine.btrfs.as_ref().unwrap().mount_archix();
    Setup::on(machine, &[], body, &[])
}

fn snapshot_numbers(machine: &Machine) -> Vec<u32> {
    archix::snapshot::Store::open(&machine.root)
        .list()
        .unwrap()
        .iter()
        .map(|record| record.number)
        .collect()
}

#[test]
#[ignore]
fn snapshot_entries_boot_the_image_their_system_had_with_the_slot_first_and_the_cap_and_pins_hold() {
    let _system = lock_system();
    let checks = Checks::default();
    let run = tempfile::tempdir().unwrap();
    let run_env = ("ARCHIX_TEST_RUN", run.path().to_str().unwrap());
    let body = |n: u32, initramfs: bool| {
        let extra = match initramfs {
            true => r#", ["/etc/mkinitcpio.conf.d/zz-archix-test.conf"] = { text = "FILES=(/etc/archix-boot-test)" }"#,
            false => "",
        };
        format!(r#"{ROLLBACKS}, files = {{ ["/etc/archix-boot-test"] = {{ text = "{n}" }}{extra} }}"#)
    };
    let setup = on_btrfs(&body(1, false));
    let machine = &setup.machine;
    let switch = |n: u32, initramfs: bool| {
        setup.set_body(&[], &body(n, initramfs));
        setup.ok_env(&[COUNTING], &["switch", "--noconfirm"])
    };
    setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    checks.eq(
        machine.entries(),
        vec!["archix-current+2.conf".to_string()],
        "snapshot 1 holds the system before archix wrote an image, so it gets no entry",
    );
    let complete = setup.run_env(
        &[run_env, ("ARCHIX_TEST_ENTRY", "archix-current.conf")],
        &["boot-complete"],
    );
    checks.check(
        complete.status.success(),
        format_args!("boot-complete: {}", String::from_utf8_lossy(&complete.stderr)),
    );
    let a = image_of(machine, "archix-current.conf");
    switch(2, false);
    switch(3, false);
    switch(4, true);
    let b = image_of(machine, "archix-current.conf");
    switch(5, false);
    checks.check(a != b, "the initramfs change made a second image");
    checks.eq(
        machine.entries().iter().map(|f| entry_id(f)).collect::<Vec<_>>(),
        vec![
            "archix-current.conf".to_string(),
            "archix-snapshot-2.conf".into(),
            "archix-snapshot-4.conf".into(),
            "archix-snapshot-5.conf".into(),
        ],
        "the two newest switch snapshots and the slot, the first snapshot after the good boot",
    );
    checks.eq(
        image_of(machine, "archix-snapshot-2.conf"),
        a.clone(),
        "snapshot 2 boots the image its system had",
    );
    checks.eq(
        image_of(machine, "archix-snapshot-4.conf"),
        a.clone(),
        "so does 4, taken before the initramfs changed",
    );
    checks.eq(
        image_of(machine, "archix-snapshot-5.conf"),
        b.clone(),
        "5 has the new one",
    );
    let mut both = vec![a.clone(), b.clone()];
    both.sort();
    checks.eq(images(machine), both, "two images for four entries");
    checks.check(
        entry_text(machine, "archix-snapshot-2.conf").contains("sort-key archix-1\n"),
        "the slot sorts right after the current entry",
    );
    checks.check(
        entry_text(machine, "archix-snapshot-5.conf").contains("sort-key archix-2\nversion 5")
            || entry_text(machine, "archix-snapshot-5.conf").contains("version 5\nsort-key archix-2"),
        format_args!(
            "the other snapshots after it, newest first by version: {}",
            entry_text(machine, "archix-snapshot-5.conf")
        ),
    );
    checks.check(
        machine.entries().iter().any(|f| f.starts_with("archix-snapshot-4+1")),
        format_args!(
            "a snapshot's entry gets a try of its own, so a fallback that fails passes on: {:?}",
            machine.entries()
        ),
    );
    let numbers = snapshot_numbers(machine);
    checks.check(
        !numbers.contains(&1) && [2, 3, 4, 5].iter().all(|n| numbers.contains(n)),
        format_args!("keep_switch = 1 pruned only snapshot 1; the slot's and the entries' are pinned: {numbers:?}"),
    );
    setup.ok(&["snapshot", "delete", "4"]);
    checks.check(
        !machine.entries().iter().any(|f| f.starts_with("archix-snapshot-4")),
        format_args!("deleting a snapshot takes its entry with it: {:?}", machine.entries()),
    );
    checks.check(
        !Path::new("/var/lib/archix/boot/entries/archix-snapshot-4.conf").exists()
            && !machine
                .root
                .join(".archix/boot/entries/archix-snapshot-4.conf")
                .exists(),
        "and its record",
    );
    checks.finish();
}

/// Unmounts what a boot-mount left: the overlay over the root and everything under the run directory.
fn unmount_try(root: &Path, run: &Path) {
    let _ = Command::new("bash")
        .args([
            "-c",
            &format!(
                "umount -l {root}/usr 2>/dev/null; while [ \"$(findmnt -n -o FSTYPE {root} | tail -1)\" = overlay ]; do umount -l {root}; done; findmnt -rn -o TARGET | grep '^{run}' | sort -r | xargs -r umount -l",
                root = root.display(),
                run = run.display()
            ),
        ])
        .status();
}

#[test]
#[ignore]
fn a_snapshot_entry_boots_read_only_with_a_throwaway_layer_and_pacman_and_switch_refuse() {
    let _system = lock_system();
    let checks = Checks::default();
    let body = |n: u32| format!(r#"{ROLLBACKS}, files = {{ ["/etc/archix-boot-test"] = {{ text = "{n}" }} }}"#);
    // No room for the 4 GiB layer and its margin, and the 1 GiB one fits with room.
    let disk = Machine::on_btrfs_of("3G");
    disk.btrfs.as_ref().unwrap().mount_archix();
    let setup = Setup::on(disk, &[], &body(1), &[]);
    let machine = &setup.machine;
    setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    setup.set_body(&[], &body(2));
    setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    checks.check(
        machine.entries().iter().any(|f| f.starts_with("archix-snapshot-2")),
        format_args!("snapshot 2 has an entry: {:?}", machine.entries()),
    );
    let run = tempfile::tempdir().unwrap();
    let run_dir = run.path().to_str().unwrap();
    let mounted = Command::new(env!("CARGO_BIN_EXE_archix"))
        .args(["boot-mount", machine.root.to_str().unwrap()])
        .env("ARCHIX_TEST_ENTRY", "archix-snapshot-2.conf")
        .env("ARCHIX_TEST_RUN", run_dir)
        .env("ARCHIX_TEST_CMDLINE", "root=x rw")
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&mounted.stdout).into_owned() + &String::from_utf8_lossy(&mounted.stderr);
    println!("boot-mount said:\n{said}");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert!(mounted.status.success(), "boot-mount: {said}");
        checks.eq(
            fs::read_to_string(machine.root.join("etc/archix-boot-test")).ok(),
            Some("1\n".to_string()),
            "the root shows snapshot 2, the system before the second switch",
        );
        let trying: archix::system::initrd::Trying =
            serde_json::from_str(&fs::read_to_string(run.path().join("try")).unwrap()).unwrap();
        checks.eq(trying.snapshot, 2, "the try record names the snapshot");
        checks.check(
            trying.layer == "file" && trying.size == 1 << 30,
            format_args!("a 3 GiB disk has no room for the 4 GiB layer, so it got the 1 GiB one: {trying:?} {said}"),
        );
        let layer = machine.root.join(".archix/try/layer.img");
        let attrs = sh(&format!(
            "lsattr -d {} 2>/dev/null || true",
            machine
                .btrfs
                .as_ref()
                .unwrap()
                .top
                .join("@archix/try/layer.img")
                .display()
        ));
        checks.check(
            attrs.split_whitespace().next().is_some_and(|flags| flags.contains('C')),
            format_args!("the layer file is NOCOW: {attrs} ({})", layer.display()),
        );
        fs::write(machine.root.join("etc/archix-try-write"), "x").unwrap();
        checks.check(
            !machine
                .btrfs
                .as_ref()
                .unwrap()
                .top
                .join("@/etc/archix-try-write")
                .exists()
                && !machine
                    .btrfs
                    .as_ref()
                    .unwrap()
                    .top
                    .join("@archix/snapshots/2/root/etc/archix-try-write")
                    .exists(),
            "a write lands in the layer, not on @ or the snapshot",
        );
        let hook = fs::read_to_string(machine.root.join("etc/pacman.d/hooks/00-archix-try.hook")).unwrap_or_default();
        checks.check(
            hook.contains("AbortOnFail"),
            "the try boot's /etc has the refusing hook",
        );
        sh(&format!("mount --bind -o ro /usr {}/usr", machine.root.display()));
        let package = archix::pacman::read_local_db(&machine.local_db())
            .unwrap()
            .into_keys()
            .find(|name| name == "tree" || name == "sed")
            .unwrap();
        let pacman = Command::new("pacman")
            .arg("--root")
            .arg(&machine.root)
            .arg("--dbpath")
            .arg(machine.root.join("var/lib/pacman"))
            .arg("--hookdir")
            .arg(machine.root.join("etc/pacman.d/hooks"))
            .args(["-Rdd", "--noconfirm", &package])
            .output()
            .unwrap();
        let pacman_said =
            String::from_utf8_lossy(&pacman.stdout).into_owned() + &String::from_utf8_lossy(&pacman.stderr);
        checks.check(
            !pacman.status.success()
                && pacman_said.contains("booted snapshot 2 read-only; `archix rollback 2` to keep, or reboot"),
            format_args!("pacman refuses in try mode: {pacman_said}"),
        );
        checks.check(
            machine
                .local_db()
                .read_dir()
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with(&format!("{package}-"))),
            "and removed nothing",
        );
        let switched = setup.run_env(&[("ARCHIX_TEST_RUN", run_dir)], &["switch", "--noconfirm"]);
        let switch_said = String::from_utf8_lossy(&switched.stderr).into_owned();
        checks.check(
            !switched.status.success() && switch_said.contains("snapshot 2 in try mode"),
            format_args!("a switch refuses in try mode: {switch_said}"),
        );
        let notice = setup.run_env(&[("ARCHIX_TEST_RUN", run_dir)], &["notice"]);
        let notice = String::from_utf8_lossy(&notice.stdout).into_owned();
        checks.check(
            notice.contains("snapshot 2, read-only") && notice.contains(&format!("{} MiB", trying.size >> 20)),
            format_args!("the notice: {notice}"),
        );
    }));
    unmount_try(&machine.root, run.path());
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    checks.check(
        !machine.root.join("etc/archix-try-write").exists(),
        "once the try is over the write is gone",
    );
    let current = Command::new(env!("CARGO_BIN_EXE_archix"))
        .args(["boot-mount", machine.root.to_str().unwrap()])
        .env("ARCHIX_TEST_ENTRY", "archix-current.conf")
        .env("ARCHIX_TEST_RUN", run.path())
        .env("ARCHIX_TEST_CMDLINE", "root=x rw")
        .output()
        .unwrap();
    unmount_try(&machine.root, run.path());
    checks.check(current.status.success(), "the current entry mounts nothing");
    checks.eq(
        fs::read_to_string(machine.root.join("etc/archix-boot-test")).ok(),
        Some("2\n".to_string()),
        "and the root is @ as it is",
    );
    checks.check(
        !machine
            .btrfs
            .as_ref()
            .unwrap()
            .top
            .join("@archix/try/layer.img")
            .exists(),
        "the next boot drops the last try's layer",
    );
    checks.finish();
}

/// A root directory with systemd's own units and archix's boot units, for systemd-analyze verify.
fn unit_root() -> tempfile::TempDir {
    // Not in the container's /tmp, which is noexec, as verify checks ExecStart= can run.
    let dir = tempfile::tempdir_in("/var/tmp").unwrap();
    sh(&format!(
        "mkdir -p {d}/usr/lib && cp -a /usr/lib/systemd {d}/usr/lib/ && mkdir -p {d}/etc/systemd/system",
        d = dir.path().display()
    ));
    let archix = Path::new("/usr/bin/archix");
    for (path, text) in archix::integration::boot_files(archix)
        .into_iter()
        .chain(archix::system::initrd::hook_files())
    {
        let path = match path.strip_prefix("/etc/initcpio/archix") {
            Ok(unit) => Path::new("/usr/lib/systemd/system").join(unit),
            Err(_) => path,
        };
        let target = dir.path().join(path.strip_prefix("/").unwrap());
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, text).unwrap();
    }
    // What the initrd generates or ships beside archix's unit.
    fs::write(
        dir.path().join("usr/lib/systemd/system/sysroot.mount"),
        "[Mount]\nWhat=/dev/disk/by-label/root\nWhere=/sysroot\n",
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("usr/bin")).unwrap();
    fs::copy("/usr/bin/true", dir.path().join("usr/bin/archix")).unwrap();
    for (link, unit) in archix::integration::boot_wants() {
        let target = dir.path().join(link.strip_prefix("/").unwrap());
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(unit, target).unwrap();
    }
    dir
}

fn verify(root: &Path, units: &[&str]) -> String {
    let output = Command::new("systemd-analyze")
        .args(["verify", "--man=no"])
        .arg(format!("--root={}", root.display()))
        .args(units)
        .output()
        .unwrap();
    format!(
        "exit {:?}\n{}{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
#[ignore]
fn the_boot_units_verify_without_ordering_cycles() {
    let dir = unit_root();
    for units in [
        vec![
            "/usr/lib/systemd/system/archix-boot-mount.service",
            "/usr/lib/systemd/system/initrd.target",
        ],
        vec![
            "/etc/systemd/system/archix-boot.service",
            "/etc/systemd/system/archix-health-check.service",
            "/etc/systemd/system/archix-boot-complete.service",
            "/usr/lib/systemd/system/systemd-bless-boot.service",
            "/usr/lib/systemd/system/boot-complete.target",
            "/usr/lib/systemd/system/multi-user.target",
            "/usr/lib/systemd/system/graphical.target",
        ],
        vec![
            "/etc/systemd/system/archix-unblessed.timer",
            "/etc/systemd/system/archix-unblessed.service",
            "/usr/lib/systemd/system/emergency.target",
            "/usr/lib/systemd/system/rescue.target",
            "/usr/lib/systemd/system/sysinit.target",
        ],
    ] {
        let found = verify(dir.path(), &units);
        assert!(
            found.starts_with("exit Some(0)") && !found.contains("cycle") && !found.contains("archix"),
            "{units:?}: {found}"
        );
    }
}

#[test]
#[ignore]
fn a_root_archix_didnt_produce_is_reported_and_refuses_switches() {
    let _system = lock_system();
    let setup = on_btrfs(ROLLBACKS);
    setup.first_switch();
    let btrfs = setup.machine.btrfs.as_ref().unwrap();
    archix::snapshot::record_root(&setup.machine.root.join(".archix"), &btrfs.top).unwrap();
    assert_eq!(
        archix::snapshot::foreign_root(&setup.machine.root),
        None,
        "the root archix recorded"
    );
    sh(&format!(
        "btrfs -q subvolume snapshot {top}/@ {top}/@before",
        top = btrfs.top.display()
    ));
    let before = archix::snapshot::subvolume_uuid(&btrfs.top.join("@before")).unwrap();
    fs::write(
        setup.machine.root.join(".archix/root.json"),
        format!("{{\"format\": 1, \"uuid\": \"{before}\"}}"),
    )
    .unwrap();
    let status = setup.ok(&["status"]);
    assert!(
        status.contains("isn't the one archix last produced") && status.contains("archix resync"),
        "{status}"
    );
    let refused = setup.fails(&["switch", "--noconfirm"]);
    assert!(
        refused.contains("isn't the one archix last produced") && refused.contains("nothing was changed"),
        "{refused}"
    );
    let before = btrfs.top.join("@before/etc");
    let append = |file: &str, line: &str| {
        let mut text = fs::read_to_string(before.join(file)).unwrap();
        text.push_str(line);
        fs::write(before.join(file), text).unwrap();
    };
    append(
        "passwd",
        "archix-carried:x:1500:1500::/home/archix-carried:/usr/bin/bash\n",
    );
    append("group", "archix-carried:x:1500:\n");
    append("shadow", "archix-carried:!:20000::::::\n");
    let resynced = setup.ok(&["resync"]);
    assert!(
        resynced.contains("carried users, groups and passwords over from the root it replaced (@before)"),
        "{resynced}"
    );
    let passwd = fs::read_to_string(setup.machine.system_file("etc/passwd")).unwrap();
    assert!(
        passwd.contains("archix-carried:x:1500"),
        "the account came over: {passwd}"
    );
    assert_eq!(
        archix::snapshot::foreign_root(&setup.machine.root),
        None,
        "the root is recorded now"
    );
    setup.ok(&["switch", "--noconfirm"]);
}

/// Moves the container's Secure Boot keys aside while it lives, which the Secure Boot tests leave there, for a test of a machine without Secure Boot.
struct NoSbctlKeys(Option<PathBuf>);

impl NoSbctlKeys {
    fn new() -> NoSbctlKeys {
        let keys = Path::new("/var/lib/sbctl/keys");
        let aside = PathBuf::from("/var/lib/sbctl/keys.aside");
        NoSbctlKeys(keys.exists().then(|| fs::rename(keys, &aside).map(|()| aside).unwrap()))
    }
}

impl Drop for NoSbctlKeys {
    fn drop(&mut self) {
        if let Some(aside) = &self.0 {
            fs::rename(aside, "/var/lib/sbctl/keys").unwrap();
        }
    }
}

fn sbctl_keys(esp: &Path) {
    if !Path::new("/var/lib/sbctl/keys/db/db.key").exists() {
        let made = Command::new("sbctl")
            .env("ESP_PATH", esp)
            .arg("create-keys")
            .status()
            .unwrap();
        assert!(made.success());
    }
}

#[test]
#[ignore]
fn under_secure_boot_an_image_is_signed_once_and_shared_by_the_entries_that_boot_it() {
    let _system = lock_system();
    let checks = Checks::default();
    let body = |n: u32| format!(r#"{ROLLBACKS}, files = {{ ["/etc/archix-boot-test"] = {{ text = "{n}" }} }}"#);
    let setup = on_btrfs(&body(1));
    let esp = setup.machine.esp();
    sbctl_keys(&esp);
    let boot = format!(
        r#"boot = {{ loader = "systemd-boot", esp = "{}", kernel = "linux", secure_boot = true, rescue_entry = false }}, kernel_params = {{ "root=/dev/vda2 rw" }}"#,
        esp.display()
    );
    let marks = tempfile::tempdir_in("/var/tmp").unwrap();
    let (log, marker) = (marks.path().join("marks"), marks.path().join("mark"));
    write_executable(&marker, &format!("#!/bin/sh\necho \"$1\" >> {}\n", log.display()));
    let signs = || {
        fs::read_to_string(&log)
            .unwrap_or_default()
            .matches("boot-image-sign")
            .count()
    };
    let switch = |n: u32| {
        setup.set_body(&[], &format!("{}, {boot}", body(n)));
        setup.ok_env(
            &[COUNTING, ("ARCHIX_TEST_MARK", marker.to_str().unwrap())],
            &["switch", "--noconfirm"],
        )
    };
    // Unsigned loaders, as a systemd-boot update leaves them.
    for loader in ["EFI/systemd/systemd-bootx64.efi", "EFI/BOOT/BOOTX64.EFI"] {
        fs::create_dir_all(esp.join(loader).parent().unwrap()).unwrap();
        fs::copy("/usr/lib/systemd/boot/efi/linuxx64.efi.stub", esp.join(loader)).unwrap();
    }
    let loaders = || {
        ["EFI/systemd/systemd-bootx64.efi", "EFI/BOOT/BOOTX64.EFI"].map(|loader| fs::read(esp.join(loader)).unwrap())
    };
    let loader_signs = || {
        fs::read_to_string(&log)
            .unwrap_or_default()
            .matches("loader-sign")
            .count()
    };
    switch(1);
    let first_loaders = loaders();
    checks.eq(loader_signs(), 2, "the unsigned loaders were signed, each once");
    let image = image_of(&setup.machine, "archix-current.conf");
    let signed = |name: &str| archix::system::secureboot::is_signed(&esp.join("EFI/archix").join(name)).unwrap();
    checks.check(signed(&image), "the image is signed");
    let recorded = || fs::read_to_string("/var/lib/archix/boot/signed").unwrap_or_default();
    let records = recorded().lines().count();
    switch(2);
    switch(3);
    checks.eq(
        image_of(&setup.machine, "archix-current.conf"),
        image.clone(),
        "the next switches reuse it",
    );
    checks.check(
        setup
            .machine
            .entries()
            .iter()
            .filter(|f| f.starts_with("archix-snapshot-"))
            .count()
            >= 2,
        format_args!("snapshot entries were written: {:?}", setup.machine.entries()),
    );
    checks.eq(
        images(&setup.machine),
        vec![image.clone()],
        "every entry starts the one image",
    );
    checks.eq(
        recorded().lines().count(),
        records,
        "nothing new was recorded as signed",
    );
    checks.eq(signs(), 1, "the image was signed once, at the first switch");
    checks.eq(
        loader_signs(),
        2,
        "the next builds left the loaders signed by its key alone",
    );
    checks.check(loaders() == first_loaders, "and their bytes as they were");
    checks.finish();
}

#[test]
#[ignore]
fn with_revoke_unused_a_switch_that_drops_a_signed_image_revokes_it() {
    let _system = lock_system();
    let checks = Checks::default();
    let setup = Setup::new(&[], "");
    let esp = setup.machine.esp();
    sbctl_keys(&esp);
    let switch = |revoke: bool, initramfs: &str| {
        setup.set_body(
            &[],
            &format!(
                r#"boot = {{ loader = "systemd-boot", esp = "{}", kernel = "linux", secure_boot = true, revoke_unused = {revoke}, rescue_entry = false }}, kernel_params = {{ "root=/dev/vda2 rw" }}, files = {{ ["/etc/mkinitcpio.conf.d/zz-archix-test.conf"] = {{ text = "{initramfs}" }} }}"#,
                esp.display()
            ),
        );
        let output = setup.run_env(&[COUNTING], &["switch", "--noconfirm"]);
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    };
    switch(false, "# one");
    let first = image_of(&setup.machine, "archix-current.conf");
    let off = switch(false, "FILES=(/etc/hostname)");
    checks.check(
        image_of(&setup.machine, "archix-current.conf") != first && !images(&setup.machine).contains(&first),
        format_args!(
            "the initramfs change drops the first image: {:?}",
            images(&setup.machine)
        ),
    );
    checks.check(
        !off.contains("revoking boot images") && !off.contains("\nrevoked "),
        format_args!("with revoke_unused off nothing is revoked: {off}"),
    );
    let second = image_of(&setup.machine, "archix-current.conf");
    let on = switch(true, "FILES=(/etc/hostname /etc/os-release)");
    checks.check(
        !images(&setup.machine).contains(&second),
        format_args!(
            "the next initramfs change drops the second image: {:?}",
            images(&setup.machine)
        ),
    );
    // The container has no dbx to write, so the attempt fails: what matters is that the switch makes it.
    checks.check(
        on.contains("revoking boot images no entry uses"),
        format_args!("with revoke_unused on the switch revokes the signed image it dropped: {on}"),
    );
    checks.finish();
}

/// An EFI variable's file: attributes, then `data`.
fn efivar(dir: &Path, name: &str, data: &[u8]) {
    let mut bytes = vec![7u8, 0, 0, 0];
    bytes.extend_from_slice(data);
    fs::write(dir.join(name), bytes).unwrap();
}

/// A load option named `label` starting `loader` (backslashes) on the ESP.
fn load_option(label: &str, loader: &str) -> Vec<u8> {
    let path: Vec<u8> = loader.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
    let mut list = vec![4u8, 4];
    list.extend_from_slice(&((path.len() + 4) as u16).to_le_bytes());
    list.extend_from_slice(&path);
    list.extend_from_slice(&[0x7f, 0xff, 4, 0]);
    let mut option = 1u32.to_le_bytes().to_vec();
    option.extend_from_slice(&(list.len() as u16).to_le_bytes());
    option.extend(label.encode_utf16().chain([0]).flat_map(u16::to_le_bytes));
    option.extend_from_slice(&list);
    option
}

fn efi_u16s(dir: &Path, name: &str) -> Vec<u16> {
    fs::read(dir.join(name))
        .unwrap_or_default()
        .get(4..)
        .unwrap_or_default()
        .chunks(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect()
}

#[test]
#[ignore]
fn a_limine_machine_moves_to_systemd_boot_only_once_a_boot_through_it_counted_as_good() {
    const GLOBAL: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";
    const LOADER: &str = "4a67b082-0a4c-41cf-b6c7-440b29bb8c4f";
    let _system = lock_system();
    // Its loaders are text, and its checks are of a machine without Secure Boot, so no keys the Secure Boot tests left may sign them.
    let _keys = NoSbctlKeys::new();
    let checks = Checks::default();
    let setup = Setup::new(&[], "");
    let machine = &setup.machine;
    let esp = machine.esp();
    fs::create_dir_all(esp.join("EFI/limine")).unwrap();
    fs::create_dir_all(esp.join("EFI/BOOT")).unwrap();
    fs::write(esp.join("EFI/limine/BOOTX64.EFI"), "limine").unwrap();
    fs::write(esp.join("EFI/BOOT/BOOTX64.EFI"), "limine").unwrap();
    for (dir, name) in [
        ("usr/share/libalpm/hooks", "limine-snapper-sync.hook"),
        ("usr/lib/systemd/system", "limine-snapper-sync.service"),
    ] {
        fs::write(machine.system_file(dir).join(name), "").unwrap();
    }
    let vars = tempfile::TempDir::new().unwrap();
    efivar(
        vars.path(),
        &format!("Boot0000-{GLOBAL}"),
        &load_option("Limine", "\\EFI\\limine\\BOOTX64.EFI"),
    );
    efivar(vars.path(), &format!("BootOrder-{GLOBAL}"), &0u16.to_le_bytes());
    let efi = ("ARCHIX_TEST_EFIVARS", vars.path().to_str().unwrap());
    let run = tempfile::TempDir::new().unwrap();
    let good_boot = [
        efi,
        ("ARCHIX_TEST_RUN", run.path().to_str().unwrap()),
        ("ARCHIX_TEST_ENTRY", "archix-current.conf"),
    ];

    let out = setup.ok_env(&[efi], &["switch", "--noconfirm"]);
    checks.check(
        out.contains("Limine stays first"),
        format_args!("the switch says what it did: {out}"),
    );
    checks.check(
        esp.join("EFI/systemd/systemd-bootx64.efi").is_file(),
        "systemd-boot is on the ESP",
    );
    checks.eq(
        efi_u16s(vars.path(), &format!("BootOrder-{GLOBAL}")),
        vec![0, 1],
        "systemd-boot's entry comes after Limine's",
    );
    checks.eq(
        efi_u16s(vars.path(), &format!("BootNext-{GLOBAL}")),
        vec![1],
        "and the next boot tries it once",
    );
    checks.eq(
        fs::read_to_string(esp.join("EFI/BOOT/BOOTX64.EFI")).ok(),
        Some("limine".to_string()),
        "the removable-media path stays Limine's",
    );

    setup.ok_env(&good_boot, &["boot-complete"]);
    checks.eq(
        efi_u16s(vars.path(), &format!("BootOrder-{GLOBAL}")),
        vec![0, 1],
        "a good boot through Limine changes nothing",
    );

    efivar(
        vars.path(),
        &format!("LoaderInfo-{LOADER}"),
        &"systemd-boot 262"
            .encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<u8>>(),
    );
    setup.ok_env(&good_boot, &["boot-complete"]);
    checks.eq(
        efi_u16s(vars.path(), &format!("BootOrder-{GLOBAL}")),
        vec![1, 0],
        "a good boot through systemd-boot puts it first",
    );
    checks.eq(
        fs::read(esp.join("EFI/BOOT/BOOTX64.EFI")).ok(),
        fs::read(esp.join("EFI/systemd/systemd-bootx64.efi")).ok(),
        "and on the removable-media path",
    );
    checks.eq(
        fs::read_to_string(esp.join("EFI/BOOT/BOOTX64.EFI.before-archix")).ok(),
        Some("limine".to_string()),
        "Limine's loader there is kept",
    );
    for link in [
        "etc/pacman.d/hooks/limine-snapper-sync.hook",
        "etc/systemd/system/limine-snapper-sync.service",
    ] {
        checks.eq(
            fs::read_link(machine.system_file(link)).ok(),
            Some(std::path::PathBuf::from("/dev/null")),
            format_args!("{link} masks Limine's copying of kernels"),
        );
    }
    checks.check(esp.join("EFI/limine/BOOTX64.EFI").is_file(), "Limine stays installed");
    let again = setup.ok_env(&[efi], &["switch", "--noconfirm"]);
    checks.check(
        !again.contains("Limine stays first"),
        format_args!("a later switch has nothing more to do: {again}"),
    );
    checks.finish();
}

/// A tmpfs of `mib` MiB, unmounted with its loop devices when dropped.
struct Scratch(tempfile::TempDir);

impl Scratch {
    fn new(mib: u64) -> Scratch {
        let dir = tempfile::tempdir().unwrap();
        sh(&format!(
            "mount -t tmpfs -o size={mib}M archix-scratch {}",
            dir.path().display()
        ));
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let dir = self.0.path().display();
        let _ = Command::new("bash")
            .args(["-c", &format!("umount -l {dir}/layer 2>/dev/null; umount -l {dir}")])
            .status();
    }
}

#[test]
#[ignore]
fn a_layer_file_whose_filesystem_doesnt_read_back_is_dropped_for_a_smaller_one_naming_why() {
    let _system = lock_system();
    let scratch = Scratch::new(256);
    let (dir, at) = (scratch.0.path().join("try"), scratch.0.path().join("layer"));
    let tamper = |file: &Path, size: u64| {
        if size == 64 << 20 {
            use std::io::Write;
            let mut handle = fs::OpenOptions::new().write(true).open(file).unwrap();
            handle.write_all(&[0u8; 4096]).unwrap();
        }
    };
    let (kind, size, notes) = archix::system::initrd::make_layer_with(&dir, &at, &[64 << 20, 16 << 20], &tamper);
    assert_eq!((kind.as_str(), size), ("file", 16 << 20), "{notes:?}");
    assert!(
        notes
            .iter()
            .any(|note| note.starts_with("no 64 MiB layer file") && note.contains("didn't read back")),
        "{notes:?}"
    );
    assert!(!notes.iter().any(|note| note.contains("I/O error")), "{notes:?}");
    let written = sh(&format!(
        "dd if=/dev/zero of={}/fill bs=1M count=8 2>&1 && sync && echo ok",
        at.display()
    ));
    assert!(written.trim_end().ends_with("ok"), "{written}");
}

#[test]
#[ignore]
fn a_layer_file_isnt_made_without_room_for_it_and_its_margin() {
    let _system = lock_system();
    let scratch = Scratch::new(128);
    let (dir, at) = (scratch.0.path().join("try"), scratch.0.path().join("layer"));
    let (kind, size, notes) = archix::system::initrd::make_layer_with(&dir, &at, &[100 << 20, 16 << 20], &|_, _| {});
    assert_eq!((kind.as_str(), size), ("file", 16 << 20), "{notes:?}");
    assert!(
        notes
            .iter()
            .any(|note| note.starts_with("no 100 MiB layer file") && note.contains("MiB free")),
        "{notes:?}"
    );
}

#[test]
#[ignore]
fn after_rotate_pcr_key_a_switch_with_nothing_else_to_do_builds_images_signed_with_the_new_key() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    let machine = &setup.machine;
    setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    let before = images(machine);
    let out = setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    assert!(
        out.contains("nothing to do"),
        "a second switch has nothing to do: {out}"
    );
    let rotated = setup.ok_env(&[], &["secure-boot", "rotate-pcr-key"]);
    assert!(rotated.contains("run `archix switch`"), "{rotated}");
    let out = setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    let after = images(machine);
    assert!(
        !out.contains("nothing to do") && after != before,
        "the switch rotate-pcr-key asks for builds an image with the new key: {before:?} -> {after:?}\n{out}"
    );
    let again = setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    assert!(
        again.contains("nothing to do"),
        "and the one after has nothing to do again: {again}"
    );
}

/// A root whose efivars say Secure Boot is enforcing, for ARCHIX_TEST_SECUREBOOT_ROOT.
fn enforcing() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let vars = dir.path().join("sys/firmware/efi/efivars");
    fs::create_dir_all(&vars).unwrap();
    for (name, value) in [("SecureBoot", 1u8), ("SetupMode", 0)] {
        fs::write(
            vars.join(format!("{name}-8be4df61-93ca-11d2-aa0d-00e098032b8c")),
            [6, 0, 0, 0, value],
        )
        .unwrap();
    }
    dir
}

#[test]
#[ignore]
fn with_secure_boot_enforcing_a_switch_whose_entry_wouldnt_be_signed_refuses_before_applying() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    let machine = &setup.machine;
    setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    let boot = boot_body(machine).replace("rescue_entry = false", "rescue_entry = false, secure_boot = false");
    setup.set_body(
        &[],
        &format!(r#"{boot}, files = {{ ["/etc/archix-unsigned-test"] = {{ text = "x" }} }}"#),
    );
    let fake = enforcing();
    let out = setup.run_env(
        &[COUNTING, ("ARCHIX_TEST_SECUREBOOT_ROOT", fake.path().to_str().unwrap())],
        &["switch", "--noconfirm"],
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!out.status.success(), "{text}");
    assert!(
        text.contains("wouldn't be signed") && text.contains("nothing was changed"),
        "it refuses naming why: {text}"
    );
    assert!(
        !machine.system_file("etc/archix-unsigned-test").exists(),
        "before applying anything: {text}"
    );
}

/// A FAT image mounted over a machine's ESP, unmounted when dropped.
struct FatEsp(PathBuf, #[allow(dead_code)] tempfile::TempDir);

impl FatEsp {
    fn over(esp: &Path) -> FatEsp {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("esp.img");
        fs::create_dir_all(esp).unwrap();
        sh(&format!(
            "truncate -s 512M {0} && mkfs.fat -F 32 {0} > /dev/null && mount -o loop,umask=0077 {0} {1}",
            image.display(),
            esp.display()
        ));
        FatEsp(esp.to_path_buf(), dir)
    }
}

impl Drop for FatEsp {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg(&self.0).status();
    }
}

#[test]
#[ignore]
fn a_switch_writes_a_fat_esp_flushing_data_before_names_and_never_with_a_per_file_fsync() {
    let _system = lock_system();
    let checks = Checks::default();
    let setup = Setup::new(&[], "");
    let machine = &setup.machine;
    let _esp = FatEsp::over(&machine.esp());
    let fat = [COUNTING, ("ARCHIX_TEST_CHECK_ESP", "1")];
    let out = setup.run_env(&fat, &["switch", "--noconfirm"]);
    checks.check(
        out.status.success() && !String::from_utf8_lossy(&out.stderr).contains("wasn't written"),
        format!(
            "a switch writes its menu on a FAT ESP: {}",
            String::from_utf8_lossy(&out.stderr)
        ),
    );
    checks.eq(
        machine.entries(),
        vec!["archix-current+2.conf".to_string()],
        "the current entry, counted, on FAT",
    );
    checks.check(!images(machine).is_empty(), "and its image");
    let entries = machine.esp().join("loader/entries");
    fs::rename(
        entries.join("archix-current+2.conf"),
        entries.join("archix-current.conf"),
    )
    .unwrap();
    setup.set_body(&[], r#"files = { ["/etc/archix-fat-test"] = { text = "one" } }"#);
    let out = setup.run_env(&fat, &["switch", "--noconfirm"]);
    checks.check(
        out.status.success() && !String::from_utf8_lossy(&out.stderr).contains("wasn't written"),
        format!(
            "a switch that renames the entry for its tries works on FAT: {}",
            String::from_utf8_lossy(&out.stderr)
        ),
    );
    checks.eq(
        machine.entries(),
        vec!["archix-current+2.conf".to_string()],
        "the blessed entry renamed to get its tries again",
    );
    let leftovers = sh(&format!("find {} -name '*.tmp'", machine.esp().display()));
    checks.check(
        leftovers.trim().is_empty(),
        format!("no temporary files left on the ESP: {leftovers}"),
    );
    // On FAT the general helpers take the ESP's way themselves: the data flushed with the filesystem, then the rename, never a per-file fsync.
    archix::durable::write(&machine.esp().join("probe-write"), b"written").unwrap();
    archix::durable::copy(Path::new("/etc/hostname"), &machine.esp().join("probe-copy")).unwrap();
    checks.eq(
        fs::read_to_string(machine.esp().join("probe-write")).ok(),
        Some("written".to_string()),
        "durable::write on FAT writes the file",
    );
    checks.eq(
        fs::read(machine.esp().join("probe-copy")).ok(),
        fs::read("/etc/hostname").ok(),
        "durable::copy on FAT copies it",
    );
    let leftovers = sh(&format!("find {} -name '*.tmp'", machine.esp().display()));
    checks.check(
        leftovers.trim().is_empty(),
        format!("and leaves no temporary file: {leftovers}"),
    );
    checks.finish();
}

#[test]
#[ignore]
fn a_counted_boot_that_finds_the_unfinished_mark_on_a_fat_esp_is_marked_bad_and_reboots_at_once() {
    let _system = lock_system();
    let checks = Checks::default();
    let setup = Setup::new(&[], "");
    let machine = &setup.machine;
    let _esp = FatEsp::over(&machine.esp());
    setup.set_body(
        &[],
        &format!(
            r#"boot = {{ loader = "systemd-boot", esp = "{}", kernel = "linux", rescue_entry = false, health_check = {{ reboot_on_failure = true }} }}, kernel_params = {{ "root=/dev/vda2 rw" }}"#,
            machine.esp().display()
        ),
    );
    let fat = [COUNTING, ("ARCHIX_TEST_CHECK_ESP", "1")];
    setup.ok_env(&fat, &["switch", "--noconfirm"]);
    archix::pacman::hooks::mark_unfinished(
        &setup.paths.state,
        &archix::pacman::hooks::Unfinished {
            pacman: 1,
            started: 1_792_576_985,
            slot: None,
            switch: None,
        },
    )
    .unwrap();
    let record = tempfile::tempdir().unwrap();
    let rebooted = record.path().join("reboot");
    let out = setup.run_env(
        &[
            COUNTING,
            ("ARCHIX_TEST_CHECK_ESP", "1"),
            ("ARCHIX_TEST_COUNTED_BOOT", "1"),
            ("ARCHIX_TEST_ENTRY", "archix-current+2.conf"),
            ("ARCHIX_TEST_REBOOT", rebooted.to_str().unwrap()),
        ],
        &["health-check"],
    );
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    checks.check(
        said.contains("its entry is marked bad, and the machine reboots into the fallback now"),
        format!("the mark fails the check and the entry is marked bad on FAT: {said}"),
    );
    checks.check(
        machine.entries().contains(&"archix-current+0-2.conf".to_string()),
        format!("the entry has no tries left: {:?}", machine.entries()),
    );
    checks.check(
        rebooted.exists(),
        "and the machine reboots at once, not after boot.unblessed_timeout",
    );
    checks.finish();
}

#[test]
#[ignore]
fn a_move_from_limine_cut_short_after_its_generation_is_recorded_is_taken_up_by_the_next_switch() {
    const GLOBAL: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";
    let _system = lock_system();
    let checks = Checks::default();
    let setup = Setup::new(&[], "");
    let machine = &setup.machine;
    let esp = machine.esp();
    fs::create_dir_all(esp.join("EFI/limine")).unwrap();
    fs::create_dir_all(esp.join("EFI/BOOT")).unwrap();
    // Real PE files, as Limine's are: with Secure Boot keys in the container, which the Secure Boot tests leave, archix signs the fallback loader.
    for loader in ["EFI/limine/BOOTX64.EFI", "EFI/BOOT/BOOTX64.EFI"] {
        fs::copy("/usr/lib/systemd/boot/efi/linuxx64.efi.stub", esp.join(loader)).unwrap();
    }
    sbctl_keys(&esp);
    let vars = tempfile::TempDir::new().unwrap();
    efivar(
        vars.path(),
        &format!("Boot0000-{GLOBAL}"),
        &load_option("Limine", "\\EFI\\limine\\BOOTX64.EFI"),
    );
    efivar(vars.path(), &format!("BootOrder-{GLOBAL}"), &0u16.to_le_bytes());
    let efi = ("ARCHIX_TEST_EFIVARS", vars.path().to_str().unwrap());
    // As the VM's cut at limine-loader-copied: the generation is recorded, the move not started.
    let cut = setup.run_env(
        &[efi, ("ARCHIX_TEST_CRASH", "limine-loader-copied")],
        &["switch", "--noconfirm"],
    );
    checks.check(
        !cut.status.success(),
        "the switch dies once systemd-boot is copied to the ESP",
    );
    checks.check(
        efi_u16s(vars.path(), &format!("BootNext-{GLOBAL}")).is_empty(),
        "before the move has a firmware entry",
    );
    let out = setup.ok_env(&[efi], &["switch", "--noconfirm"]);
    checks.check(
        out.contains("Limine stays first"),
        format_args!("the next switch, with nothing else to do, starts the move: {out}"),
    );
    checks.eq(
        efi_u16s(vars.path(), &format!("BootNext-{GLOBAL}")),
        vec![1],
        "and the next boot tries systemd-boot",
    );
    checks.finish();
}

#[test]
#[ignore]
// The race lost 8 of 2000 attaches here before the fix, so a short loop wouldn't see it.
fn a_layer_file_mounts_every_time_as_nothing_tears_its_loop_device_down_first() {
    let _system = lock_system();
    let scratch = Scratch::new(128);
    let (dir, at) = (scratch.0.path().join("try"), scratch.0.path().join("layer"));
    for attempt in 0..400 {
        let (kind, size, notes) = archix::system::initrd::make_layer_with(&dir, &at, &[16 << 20], &|_, _| {});
        assert_eq!(
            (kind.as_str(), size),
            ("file", 16 << 20),
            "attempt {attempt}: {notes:?}"
        );
        sh(&format!("umount {}", at.display()));
    }
}

#[test]
#[ignore]
fn blessing_renames_the_entry_this_boot_counted_on_not_a_temporary_file_a_cut_left() {
    let _system = lock_system();
    let checks = Checks::default();
    let setup = Setup::new(&[], "");
    let machine = &setup.machine;
    setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    let entries = machine.esp().join("loader/entries");
    let text = fs::read_to_string(entries.join("archix-current+2.conf")).unwrap();
    fs::rename(
        entries.join("archix-current+2.conf"),
        entries.join("archix-current+1-1.conf"),
    )
    .unwrap();
    // Sorts first, and fs::read_dir on a plain directory may return it first too; what a cut in the menu's write leaves.
    fs::write(entries.join("archix-current+2-0.conf.tmp"), "torn").unwrap();
    let out = setup.run_env(
        &[COUNTING, ("ARCHIX_TEST_ENTRY", "archix-current+1-1.conf")],
        &["bless"],
    );
    checks.check(
        out.status.success(),
        format!("{}", String::from_utf8_lossy(&out.stderr)),
    );
    checks.eq(
        fs::read_to_string(entries.join("archix-current.conf")).ok(),
        Some(text),
        "the blessed name holds the entry, not the temporary file",
    );
    checks.check(
        !entries.join("archix-current+1-1.conf").exists(),
        format!("and no second name is left: {:?}", machine.entries()),
    );
    checks.finish();
}

#[test]
#[ignore]
fn a_switch_cut_between_its_record_and_its_boot_step_is_finished_by_the_next_one_and_a_lost_image_or_entry_comes_back()
{
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    let machine = &setup.machine;
    let crash = [COUNTING, ("ARCHIX_TEST_CRASH", "switch-recorded")];
    let cut = setup.run_env(&crash, &["switch", "--noconfirm"]);
    assert!(!cut.status.success(), "the cut happens: {cut:?}");
    assert!(machine.entries().is_empty(), "no menu yet: {:?}", machine.entries());
    let status = setup.ok_env(&[COUNTING], &["status"]);
    assert!(status.contains("boot step"), "status says so: {status}");
    let finished = setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    assert!(finished.contains("is finished now"), "{finished}");
    let first = image_of(machine, "archix-current.conf");
    assert_eq!(images(machine), vec![first.clone()], "its image is on the ESP");
    let status = setup.ok_env(&[COUNTING], &["status"]);
    assert!(!status.contains("boot step"), "and status is quiet: {status}");
    let again = setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    assert!(
        again.contains("nothing to do") && !again.contains("is finished now") && !again.contains("isn't whole"),
        "and the next has nothing to do: {again}"
    );

    setup.set_body(
        &[],
        r#"files = { ["/etc/archix-boot-test"] = { text = "one" }, ["/etc/mkinitcpio.conf.d/zz-archix-test.conf"] = { text = "FILES=(/etc/archix-boot-test)" } }"#,
    );
    let cut = setup.run_env(&crash, &["switch", "--noconfirm"]);
    assert!(!cut.status.success(), "the second cut happens: {cut:?}");
    assert_eq!(
        image_of(machine, "archix-current.conf"),
        first,
        "the entry still starts the old image"
    );
    let finished = setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    assert!(finished.contains("is finished now"), "{finished}");
    let second = image_of(machine, "archix-current.conf");
    assert_ne!(
        second, first,
        "an entry and image that are there but stale are built again"
    );

    fs::remove_file(machine.esp().join("EFI/archix").join(&second)).unwrap();
    let back = setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    assert!(back.contains("isn't whole"), "{back}");
    assert!(
        machine.esp().join("EFI/archix").join(&second).is_file(),
        "the image is back: {back}"
    );
    let entries = machine.esp().join("loader/entries");
    for name in machine.entries() {
        fs::remove_file(entries.join(name)).unwrap();
    }
    let back = setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    assert!(back.contains("isn't whole"), "{back}");
    assert!(
        machine.entries().iter().any(|name| name.starts_with("archix-current")),
        "the entry is back: {back}"
    );
    setup.set_body(&[], r#"files = { ["/etc/archix-boot-test"] = { text = "two" } }"#);
    let cut = setup.run_env(
        &[COUNTING, ("ARCHIX_TEST_CRASH", "switch-boot-done")],
        &["switch", "--noconfirm"],
    );
    assert!(!cut.status.success(), "the third cut happens: {cut:?}");
    let status = setup.ok_env(&[COUNTING], &["status"]);
    assert!(
        !status.contains("(tree "),
        "this generation's config isn't recorded yet: {status}"
    );
    setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    let status = setup.ok_env(&[COUNTING], &["status"]);
    assert!(status.contains("(tree "), "the next switch records it: {status}");
    let name = machine
        .entries()
        .into_iter()
        .find(|n| n.starts_with("archix-current"))
        .unwrap();
    let written = fs::read_to_string(entries.join(&name)).unwrap();
    fs::write(entries.join(&name), written.replace("sort-key", "sort-key edited")).unwrap();
    let edited = setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    assert!(
        !edited.contains("isn't whole"),
        "an edited entry is left as it is: {edited}"
    );
    assert!(
        fs::read_to_string(entries.join(&name))
            .unwrap()
            .contains("sort-key edited")
    );
}

#[test]
#[ignore]
fn a_loader_conf_a_cut_left_empty_is_written_again_at_boot_and_by_the_next_switch() {
    let _system = lock_system();
    let _keys = NoSbctlKeys::new();
    let setup = Setup::new(&[], "");
    let machine = &setup.machine;
    setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    let conf = machine.esp().join("loader/loader.conf");
    let written = fs::read_to_string(&conf).unwrap();
    assert!(written.contains("default archix-current.conf\n"), "{written}");
    // What reordered writes leave of a rename over loader.conf's FAT name.
    fs::write(&conf, "").unwrap();
    let status = setup.ok_env(&[COUNTING], &["status"]);
    assert!(status.contains("loader.conf"), "status says so: {status}");
    let boot = setup.ok_env(&[COUNTING], &["boot-check"]);
    assert_eq!(
        fs::read_to_string(&conf).unwrap(),
        written,
        "the boot writes it again: {boot}"
    );
    assert!(boot.contains("loader.conf"), "and says so: {boot}");
    fs::write(&conf, "timeout 3\n").unwrap();
    let again = setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    assert!(
        again.contains("isn't whole") && again.contains("loader.conf"),
        "{again}"
    );
    let now = fs::read_to_string(&conf).unwrap();
    assert!(
        now.contains("timeout 3\n")
            && now.contains("default archix-current.conf\n")
            && now.contains("preferred archix-*\n"),
        "the next switch points it at archix's entries again, keeping the user's lines: {now}"
    );
}

#[test]
#[ignore]
fn a_switch_waits_for_the_boot_units_short_runs_and_the_minutely_unblessed_check_skips_a_busy_minute() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    let hold = |seconds: u32| {
        Command::new("sh")
            .arg("-c")
            .arg(format!(
                "echo 'pid 1, `/usr/bin/archix unblessed`' > /var/lib/archix/running && exec flock /var/lib/archix/running sleep {seconds}"
            ))
            .spawn()
            .unwrap()
    };
    let mut held = hold(5);
    std::thread::sleep(std::time::Duration::from_millis(500));
    let began = std::time::Instant::now();
    let out = setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    assert!(
        began.elapsed().as_secs() >= 3,
        "it waited for the unblessed check: {out}"
    );
    held.wait().unwrap();
    let mut held = hold(5);
    std::thread::sleep(std::time::Duration::from_millis(500));
    let skipped = setup.run_env(&[COUNTING], &["unblessed"]);
    held.wait().unwrap();
    assert!(
        skipped.status.success(),
        "the unblessed check skips a minute another run holds: {skipped:?}"
    );
}

#[test]
#[ignore]
fn a_loader_conf_a_cut_left_empty_comes_back_with_the_users_lines() {
    let _system = lock_system();
    let _keys = NoSbctlKeys::new();
    let setup = Setup::new(&[], "");
    let machine = &setup.machine;
    setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    let conf = machine.esp().join("loader/loader.conf");
    let with_users = format!("timeout 3\nconsole-mode max\n{}", fs::read_to_string(&conf).unwrap());
    fs::write(&conf, &with_users).unwrap();
    setup.ok_env(&[COUNTING], &["boot-check"]);
    fs::write(&conf, "").unwrap();
    let boot = setup.ok_env(&[COUNTING], &["boot-check"]);
    assert_eq!(
        fs::read_to_string(&conf).unwrap(),
        with_users,
        "the boot writes it again as it was: {boot}"
    );
    fs::write(&conf, "").unwrap();
    setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    assert_eq!(
        fs::read_to_string(&conf).unwrap(),
        with_users,
        "and so does the next switch"
    );
}

#[test]
#[ignore]
fn a_try_boot_writes_again_a_rescue_entry_a_cut_left_empty() {
    let _system = lock_system();
    let _keys = NoSbctlKeys::new();
    let setup = Setup::new(&[], "");
    let machine = &setup.machine;
    let esp = machine.esp();
    setup.set_body(
        &[],
        &boot_body(machine).replace("rescue_entry = false", "rescue_entry = true"),
    );
    // The container can't build a rescue image, so its entry, image and record are made by hand, as a switch leaves them.
    let _ = setup.run_env(&[COUNTING], &["switch", "--noconfirm"]);
    fs::create_dir_all(esp.join("EFI/archix-rescue")).unwrap();
    fs::write(esp.join("EFI/archix-rescue/rescue-abc.efi"), "uki").unwrap();
    fs::write(
        "/var/lib/archix/boot/rescue.json",
        r#"{"kernel": "7.2.9-arch1-1", "key": "k", "uki": "rescue-abc.efi"}"#,
    )
    .unwrap();
    let entry = esp.join("loader/entries/archix-rescue.conf");
    fs::write(&entry, "").unwrap();
    let run = tempfile::tempdir().unwrap();
    fs::write(
        run.path().join("try"),
        r#"{"snapshot": 1, "layer": "tmpfs", "size": 0}"#,
    )
    .unwrap();
    let boot = setup.ok_env(
        &[COUNTING, ("ARCHIX_TEST_RUN", run.path().to_str().unwrap())],
        &["boot-check"],
    );
    assert_eq!(
        fs::read_to_string(&entry).unwrap(),
        "title archix rescue (kernel 7.2.9-arch1-1)\nsort-key archix-rescue\nefi /EFI/archix-rescue/rescue-abc.efi\n",
        "a try boot, when rescue may be wanted, writes it again: {boot}"
    );
}

#[test]
#[ignore]
fn the_boot_units_wait_for_a_users_run_however_long_it_takes_rather_than_fail() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    for unit in ["health-check", "boot-check", "bless", "boot-complete"] {
        let mut held = Command::new("sh")
            .arg("-c")
            .arg("echo 'pid 1, `/usr/bin/archix switch --noconfirm`' > /var/lib/archix/running && exec flock /var/lib/archix/running sleep 5")
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        let began = std::time::Instant::now();
        let run = setup.run_env(&[COUNTING], &[unit]);
        held.wait().unwrap();
        let said = format!("{run:?}");
        assert!(
            !said.contains("already running") && began.elapsed().as_secs() >= 3,
            "{unit} waits for the user's switch: {said}"
        );
    }
}

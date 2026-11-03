//! The boot menu, its images and try mode on test machines' ESPs; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

mod common;

use std::fs;
use std::path::Path;
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
    let setup = on_btrfs(&body(1));
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
            trying.layer == "file" && trying.size < 4 << 30,
            format_args!("a 2 GiB disk has no room for the 4 GiB layer, so it got a smaller one: {trying:?} {said}"),
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
    switch(1);
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
    checks.finish();
}

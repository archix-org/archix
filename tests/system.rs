//! Manages /etc files, users and services; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

mod common;

use std::fs;
use std::process::Command;

use common::*;

const UNIT: &str = "[Unit]\nDescription=archix test\n[Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=/usr/bin/true\n[Install]\nWantedBy=multi-user.target";

fn body(conf: &str) -> String {
    format!(
        r#"files = {{ ["/etc/archix-test.conf"] = {{ text = {conf:?} }}, ["/etc/issue"] = {{ text = "archix test issue" }} }},
        units = {{ ["archix-test.service"] = {UNIT:?} }},
        services = {{ ["archix-test.service"] = true }},
        users = {{ archixtest = {{ groups = {{ "wheel", "archixgroup" }} }} }},
        groups = {{ archixgroup = {{}} }},
        sysctl = {{ ["kernel.domainname"] = "archix.test" }},
        timezone = "Europe/Istanbul""#
    )
}

fn systemctl(args: &[&str]) -> String {
    let output = Command::new("systemctl").args(args).output().unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn read(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

#[test]
#[ignore]
fn files_users_and_services_follow_the_config() {
    let _system = lock_system();
    let original_issue = read("/etc/issue");
    let setup = Setup::new(&[], &body("one"));
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(
        out.contains("create     /etc/archix-test.conf") && out.contains("replace    /etc/issue"),
        "{out}"
    );
    assert_eq!(read("/etc/archix-test.conf"), "one\n");
    assert_eq!(read("/etc/issue"), "archix test issue\n");
    assert_eq!(
        read("/var/lib/archix/backups/etc/issue"),
        original_issue,
        "the replaced original is kept"
    );
    assert_eq!(systemctl(&["is-enabled", "archix-test.service"]), "enabled");
    assert_eq!(systemctl(&["is-active", "archix-test.service"]), "active");
    let groups = Command::new("id").args(["-nG", "archixtest"]).output().unwrap();
    let groups = String::from_utf8_lossy(&groups.stdout);
    assert!(groups.contains("wheel") && groups.contains("archixgroup"), "{groups}");
    assert_eq!(
        fs::read_link("/etc/localtime").unwrap().to_string_lossy(),
        "/usr/share/zoneinfo/Europe/Istanbul"
    );
    assert_eq!(
        read("/etc/sysctl.d/90-archix.conf"),
        "kernel.domainname = archix.test\n"
    );
    assert!(setup.ok(&["diff"]).contains("matches the lock and the config"));

    fs::write("/etc/archix-test.conf", "edited by hand\n").unwrap();
    assert!(
        setup
            .ok(&["diff"])
            .contains("/etc/archix-test.conf (edited outside archix")
    );
    setup.set_body(&[], &body("two"));
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(read("/etc/archix-test.conf"), "two\n");
    let saved = fs::read_dir("/var/lib/archix/edited")
        .unwrap()
        .flatten()
        .map(|dir| dir.path().join("etc/archix-test.conf"))
        .find(|path| path.exists())
        .expect("the hand edit was saved");
    assert_eq!(fs::read_to_string(saved).unwrap(), "edited by hand\n");

    setup.ok(&["rollback", "--noconfirm"]);
    assert_eq!(
        read("/etc/archix-test.conf"),
        "one\n",
        "rollback restores managed files too"
    );

    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert!(!std::path::Path::new("/etc/archix-test.conf").exists());
    assert_eq!(
        read("/etc/issue"),
        original_issue,
        "the original comes back when the file leaves the config"
    );
    assert!(!std::path::Path::new("/etc/systemd/system/archix-test.service").exists());
    assert_ne!(systemctl(&["is-active", "archix-test.service"]), "active");
    assert!(
        !Command::new("id").arg("archixtest").output().unwrap().status.success(),
        "the user is removed"
    );
}

#[test]
#[ignore]
fn a_failing_service_does_not_undo_the_switch() {
    let _system = lock_system();
    let broken = UNIT.replace("/usr/bin/true", "/usr/bin/false");
    let setup = Setup::new(
        &[],
        &format!(
            r#"files = {{ ["/etc/archix-soft.conf"] = {{ text = "kept" }} }}, units = {{ ["archix-broken.service"] = {broken:?} }}, services = {{ ["archix-broken.service"] = true }}"#
        ),
    );
    let output = setup.run(&["switch", "--noconfirm"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "the command reports the failure");
    assert!(
        stderr.contains("archix-broken.service") && stderr.contains("the new generation is current"),
        "{stderr}"
    );
    assert_eq!(read("/etc/archix-soft.conf"), "kept\n", "everything else was applied");
    assert!(
        !std::path::Path::new("/var/lib/archix/journal.json").exists(),
        "the switch finished"
    );
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
}

fn hooks_body(conf: &str, extra_hook: &str) -> String {
    format!(
        r#"files = {{ ["/etc/archix-hook.conf"] = {{ text = {conf:?}, on_change = "echo changed >> /var/tmp/archix-on-change" }} }},
        users = {{ archixhook = {{}} }},
        hooks = {{
          z = {{ run = "echo z $USER >> /var/tmp/archix-hooks" }},
          a = {{ run = "echo a $USER $HOME >> /var/tmp/archix-hooks", user = "archixhook", after = {{ "z" }} }},
          {extra_hook}
        }}"#
    )
}

#[test]
#[ignore]
fn hooks_run_on_every_switch_and_on_change_only_when_the_file_changed() {
    let _system = lock_system();
    for log in ["/var/tmp/archix-hooks", "/var/tmp/archix-on-change"] {
        let _ = fs::remove_file(log);
    }
    let setup = Setup::new(&[], &hooks_body("one", ""));
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(out.contains("run hooks  z, a"), "{out}");
    assert_eq!(
        read("/var/tmp/archix-hooks"),
        "z root\na archixhook /home/archixhook\n",
        "after orders hooks, and user hooks run as that user"
    );
    assert_eq!(read("/var/tmp/archix-on-change"), "changed\n");

    assert!(setup.ok(&["switch", "--noconfirm"]).contains("nothing to do"));
    assert_eq!(
        read("/var/tmp/archix-hooks").lines().count(),
        4,
        "hooks run on every switch, like NixOS activation"
    );
    assert_eq!(read("/var/tmp/archix-on-change"), "changed\n", "the file didn't change");

    fs::write("/etc/archix-hook.conf", "edited by hand\n").unwrap();
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(
        read("/var/tmp/archix-on-change"),
        "changed\nchanged\n",
        "restoring a hand-edited file changes it on disk"
    );

    setup.set_body(&[], &hooks_body("two", ""));
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(read("/var/tmp/archix-on-change").lines().count(), 3);

    setup.set_body(&[], &hooks_body("three", r#"broken = { run = "false" },"#));
    let output = setup.run(&["switch", "--noconfirm"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "a failing hook is reported");
    assert!(stderr.contains("hook broken"), "{stderr}");
    assert_eq!(read("/etc/archix-hook.conf"), "three\n", "the switch still finished");
    assert!(!std::path::Path::new("/var/lib/archix/journal.json").exists());

    setup.ok(&["rollback", "--noconfirm"]);
    assert_eq!(
        read("/var/tmp/archix-on-change").lines().count(),
        5,
        "a rollback runs the target generation's on_change too"
    );
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
}

#[test]
#[ignore]
fn files_taken_over_unchanged_survive_being_unmanaged() {
    let _system = lock_system();
    let path = "/etc/archix-taken-over.conf";
    fs::write(path, "already here\n").unwrap();
    fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    let setup = Setup::new(
        &[],
        &format!("files = {{ [\"{path}\"] = {{ text = \"already here\" }} }}"),
    );
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(out.contains("no package changes were needed"), "{out}");
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(
        read(path),
        "already here\n",
        "unmanaging puts the original back instead of deleting it"
    );
    fs::remove_file(path).unwrap();
}

fn nft_table() -> Option<String> {
    let output = Command::new("nft")
        .args(["list", "table", "inet", "archix"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[test]
#[ignore]
fn the_firewall_and_environment_come_from_the_config() {
    let _system = lock_system();
    let body = |port: u32| {
        format!(
            r#"firewall = {{ enable = true, allowed_tcp_ports = {{ {port} }}, trusted_interfaces = {{ "wg0" }} }},
            environment = {{ variables = {{ EDITOR = "nvim", PATHS = {{ "/a", "/b" }} }}, shell_aliases = {{ ll = "ls -l" }} }}"#
        )
    };
    let setup = Setup::new(&[], &body(2222));
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(systemctl(&["is-active", "archix-firewall.service"]), "active");
    let table = nft_table().expect("the archix table is loaded");
    assert!(
        table.contains("tcp dport 2222 accept") && table.contains("\"wg0\""),
        "{table}"
    );
    assert_eq!(
        read("/etc/profile.d/archix-environment.sh"),
        "export EDITOR=\"nvim\"\nexport PATHS=\"/a:/b\"\n"
    );
    assert_eq!(read("/etc/environment.d/90-archix.conf"), "EDITOR=nvim\nPATHS=/a:/b\n");
    assert_eq!(read("/etc/profile.d/archix-aliases.sh"), "alias ll='ls -l'\n");

    setup.set_body(&[], &body(2223));
    setup.ok(&["switch", "--noconfirm"]);
    let table = nft_table().unwrap();
    assert!(
        table.contains("tcp dport 2223 accept") && !table.contains("2222"),
        "changed rules are reloaded:\n{table}"
    );

    setup.set_body(
        &[],
        r#"firewall = { enable = true, allowed_tcp_ports = { 2223 }, extra_input_rules = "tcp dportt 9 accept" }"#,
    );
    let err = setup.fails(&["switch", "--noconfirm"]);
    assert!(err.contains("the firewall rules don't load"), "{err}");
    assert!(
        nft_table().unwrap().contains("tcp dport 2223 accept"),
        "a bad rule changes nothing"
    );

    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert!(
        nft_table().is_none(),
        "turning the firewall off removes only archix's table"
    );
    assert!(!std::path::Path::new("/etc/profile.d/archix-environment.sh").exists());
}

/// Seconds since the epoch, for `journalctl --since @…`.
fn now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    format!("@{}", secs - 1)
}

/// What pid 1 logged since `since`.
fn pid1_log(since: &str) -> String {
    let output = Command::new("journalctl")
        .args(["--since", since, "-o", "cat", "_PID=1"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
#[ignore]
fn tmpfiles_sysusers_and_modules_load_apply_on_the_switch_that_changes_them() {
    let _system = lock_system();
    let _ = fs::remove_dir("/run/archix-tmpfiles-test");
    let modules_load = || systemctl(&["show", "-P", "InvocationID", "systemd-modules-load.service"]);
    let before = modules_load();
    let body = r##"files = {
        ["/etc/tmpfiles.d/archix-test.conf"] = { text = "d /run/archix-tmpfiles-test 0755 root root -\n" },
        ["/etc/sysusers.d/archix-test.conf"] = { text = "u archix-sysuser - \"archix test\"\n" },
        ["/etc/modules-load.d/archix-test.conf"] = { text = "# no modules, so loading them can't fail in a container\n" },
    }"##;
    let setup = Setup::new(&[], body);
    setup.ok(&["switch", "--noconfirm"]);
    let user = Command::new("getent")
        .args(["passwd", "archix-sysuser"])
        .output()
        .unwrap();
    let after = modules_load();
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    let _ = Command::new("userdel").arg("archix-sysuser").status();
    assert!(
        fs::metadata("/run/archix-tmpfiles-test").is_ok_and(|meta| meta.is_dir()),
        "systemd-tmpfiles --create should run when tmpfiles.d changes"
    );
    assert!(
        user.status.success(),
        "systemd-sysusers should run when sysusers.d changes"
    );
    assert!(
        !after.is_empty() && after != before,
        "systemd-modules-load should run again when modules-load.d changes ({before:?} -> {after:?})"
    );
}

#[test]
#[ignore]
fn dbus_and_logind_config_reloads_them_without_a_restart() {
    let _system = lock_system();
    let pids = || {
        (
            systemctl(&["show", "-P", "MainPID", "dbus.service"]),
            systemctl(&["show", "-P", "MainPID", "systemd-logind.service"]),
        )
    };
    let before = pids();
    assert!(
        before.0 != "0" && before.1 != "0",
        "dbus and logind should be running: {before:?}"
    );
    let since = now();
    let body = r#"files = {
        ["/etc/dbus-1/system.d/archix-test.conf"] = { text = "<busconfig></busconfig>\n" },
        ["/etc/systemd/logind.conf.d/archix-test.conf"] = { text = "[Login]\n" },
    }"#;
    let setup = Setup::new(&[], body);
    setup.ok(&["switch", "--noconfirm"]);
    let log = pid1_log(&since);
    let after = pids();
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(
        after, before,
        "dbus and logind keep their processes, since a restart ends every session"
    );
    for unit in ["D-Bus System Message Bus", "User Login Management"] {
        assert!(
            log.lines()
                .any(|line| line.starts_with("Reloaded") && line.contains(unit)),
            "{unit} should be reloaded:\n{log}"
        );
    }
}

#[test]
#[ignore]
fn changed_user_units_reload_the_running_user_managers() {
    let _system = lock_system();
    let unit = |text: &str| {
        format!(
            r#"files = {{ ["/etc/systemd/user/archix-test-user.service"] = {{ text = "[Service]\nType=oneshot\nExecStart=/usr/bin/echo {text}\n" }} }}"#
        )
    };
    assert!(
        Command::new("systemctl")
            .args(["start", "user@0.service"])
            .status()
            .unwrap()
            .success()
    );
    let user = |args: &[&str]| {
        let output = Command::new("systemctl")
            .args(["--user", "-M", "root@"])
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    let setup = Setup::new(&[], &unit("one"));
    setup.ok(&["switch", "--noconfirm"]);
    assert!(user(&["show", "-P", "ExecStart", "archix-test-user.service"]).contains("echo one"));
    setup.set_body(&[], &unit("two"));
    setup.ok(&["switch", "--noconfirm"]);
    let stale = user(&["show", "-P", "NeedDaemonReload", "archix-test-user.service"]);
    let exec = user(&["show", "-P", "ExecStart", "archix-test-user.service"]);
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(stale, "no", "root's user manager should have reloaded the changed unit");
    assert!(
        exec.contains("echo two"),
        "root's user manager should run the new unit: {exec}"
    );
}

#[test]
#[ignore]
fn changing_a_drop_in_restarts_its_service() {
    let _system = lock_system();
    let unit = "[Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=/bin/sh -c 'echo $X > /run/archix-test-dropin'\n[Install]\nWantedBy=multi-user.target";
    let body = |x: &str| {
        format!(
            r#"units = {{ ["archix-test-dropin.service"] = {unit:?} }},
            services = {{ ["archix-test-dropin.service"] = true }},
            files = {{ ["/etc/systemd/system/archix-test-dropin.service.d/10-x.conf"] = {{ text = "[Service]\nEnvironment=X={x}\n" }} }}"#
        )
    };
    let setup = Setup::new(&[], &body("1"));
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(read("/run/archix-test-dropin").trim(), "1");
    setup.set_body(&[], &body("2"));
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(
        read("/run/archix-test-dropin").trim(),
        "2",
        "the service runs with its new drop-in"
    );
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
}

#[test]
#[ignore]
fn a_service_account_goes_away_after_its_service_stops() {
    let _system = lock_system();
    let unit = "[Service]\nUser=archixsvc\nExecStart=/usr/bin/sleep infinity\n[Install]\nWantedBy=multi-user.target";
    let setup = Setup::new(
        &[],
        &format!(
            r#"units = {{ ["archix-test-user.service"] = {unit:?} }},
            services = {{ ["archix-test-user.service"] = true }},
            users = {{ archixsvc = {{ system = true }} }}"#
        ),
    );
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(systemctl(&["is-active", "archix-test-user.service"]), "active");
    let uid = |name: &str| {
        String::from_utf8_lossy(&Command::new("id").args(["-u", name]).output().unwrap().stdout)
            .trim()
            .to_string()
    };
    let first = uid("archixsvc");
    setup.set_body(&[], "");
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(
        !out.contains("userdel"),
        "userdel ran while the service still held the account:\n{out}"
    );
    assert!(
        !Command::new("id").arg("archixsvc").output().unwrap().status.success(),
        "the account is gone"
    );
    setup.set_body(&[], "users = { archixsvc = { system = true } }");
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(
        uid("archixsvc"),
        first,
        "a user created again gets its old uid, so its leftover files are still its own"
    );
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
}

#[test]
#[ignore]
fn files_on_fat_are_not_edited_just_because_fat_has_no_modes() {
    let _system = lock_system();
    let dir = tempfile::tempdir().unwrap();
    let (image, esp) = (dir.path().join("esp.img"), "/mnt/archix-test-esp");
    let run = |cmd: &str, args: &[&str]| {
        assert!(
            Command::new(cmd).args(args).status().unwrap().success(),
            "{cmd} {args:?}"
        )
    };
    run("truncate", &["-s", "64M", image.to_str().unwrap()]);
    run("mkfs.fat", &["-F", "32", image.to_str().unwrap()]);
    fs::create_dir_all(esp).unwrap();
    run("mount", &["-o", "loop,fmask=0022", image.to_str().unwrap(), esp]);
    let setup = Setup::new(
        &[],
        &format!("files = {{ [\"{esp}/loader/entries/extra.conf\"] = {{ text = \"title extra\" }} }}"),
    );
    setup.ok(&["switch", "--noconfirm"]);
    let again = setup.ok(&["switch", "--noconfirm"]);
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    run("umount", &[esp]);
    assert!(
        again.contains("nothing to do") && !again.contains("edited outside archix"),
        "FAT reports 0755 root for every file, which isn't an edit:\n{again}"
    );
}

#[test]
#[ignore]
fn a_symlink_over_a_mounted_file_is_left_for_later() {
    let _system = lock_system();
    let before = read("/etc/resolv.conf");
    let setup = Setup::new(
        &[],
        r#"files = { ["/etc/resolv.conf"] = { link = "/run/systemd/resolve/stub-resolv.conf" } }"#,
    );
    let output = setup.run(&["switch", "--noconfirm"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("/etc/resolv.conf is mounted over here"), "{stderr}");
    assert_eq!(
        read("/etc/resolv.conf"),
        before,
        "Docker's mounted resolv.conf is untouched"
    );
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
}

#[test]
#[ignore]
fn a_btrfs_whose_data_chunks_fill_the_device_gets_room_back_before_a_switch() {
    let dir = tempfile::TempDir::new().unwrap();
    let (image, mount) = (dir.path().join("btrfs.img"), dir.path().join("mnt"));
    fs::create_dir_all(&mount).unwrap();
    let sh = |command: &str| {
        let ok = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .status()
            .unwrap()
            .success();
        assert!(ok, "{command}");
    };
    // Several data chunks, as on a real disk: btrfs never gives up the only chunk of a type.
    sh(&format!("truncate -s 6G {0} && mkfs.btrfs -q {0}", image.display()));
    // commit=300 keeps the kernel from reclaiming the emptied chunks by itself before the check.
    sh(&format!(
        "mount -o loop,commit=300 {} {}",
        image.display(),
        mount.display()
    ));
    // Some data to keep, as any root has; with nothing at all, btrfs refuses to balance away its last data chunk.
    for i in 0..300 {
        fs::write(mount.join(format!("file{i}")), vec![7u8; 4096]).unwrap();
    }
    let free = archix::conflicts::free_space(&mount).unwrap();
    sh(&format!(
        "fallocate -l {} {}/fill && rm {1}/fill",
        free - (16 << 20),
        mount.display()
    ));
    let stuck = archix::system::btrfs::unallocated(&mount).unwrap();
    println!(
        "unallocated after filling and emptying the data chunks: {} MiB",
        stuck >> 20
    );
    assert!(
        stuck < 64 << 20,
        "the data chunks should still hold the device: {} MiB unallocated",
        stuck >> 20
    );
    archix::system::btrfs::ensure_metadata_room(&mount).unwrap();
    let after = archix::system::btrfs::unallocated(&mount).unwrap();
    println!("unallocated after ensure_metadata_room: {} MiB", after >> 20);
    sh(&format!("umount {}", mount.display()));
    assert!(
        after > 2 << 30,
        "the empty data chunks should be back in the pool: {} MiB unallocated",
        after >> 20
    );
}

//! Manages /etc files, users and services in deployments; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

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

/// What `systemctl is-enabled` says about `unit` in the deployment that boots next.
fn enabled(unit: &str) -> String {
    let root = system_path("/etc").parent().unwrap().to_path_buf();
    let output = Command::new("systemctl")
        .arg(format!("--root={}", root.display()))
        .args(["is-enabled", unit])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A file of the system that boots next.
fn read(path: &str) -> String {
    fs::read_to_string(system_path(path)).unwrap_or_default()
}

/// The groups a user is in, by the account files of the system that boots next.
fn groups_of(user: &str) -> Vec<String> {
    read("/etc/group")
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(':').collect();
            (fields.len() == 4 && fields[3].split(',').any(|member| member == user)).then(|| fields[0].to_string())
        })
        .collect()
}

/// A user's uid in the system that boots next.
fn uid(user: &str) -> Option<String> {
    read("/etc/passwd").lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        (fields.first() == Some(&user)).then(|| fields[2].to_string())
    })
}

#[test]
#[ignore]
fn files_users_and_services_follow_the_config() {
    let _system = lock_system();
    let setup = Setup::new(&[], &body("one"));
    let original_issue = read("/etc/issue");
    let out = setup.lay_out();
    assert!(
        out.contains("create     /etc/archix-test.conf") && out.contains("replace    /etc/issue"),
        "{out}"
    );
    assert_eq!(read("/etc/archix-test.conf"), "one\n");
    assert_eq!(read("/etc/issue"), "archix test issue\n");
    assert_eq!(
        fs::read_to_string("/var/lib/archix/backups/etc/issue").unwrap_or_default(),
        original_issue,
        "the replaced original is kept"
    );
    assert_eq!(enabled("archix-test.service"), "enabled");
    let groups = groups_of("archixtest");
    assert!(
        groups.contains(&"wheel".to_string()) && groups.contains(&"archixgroup".to_string()),
        "{groups:?}"
    );
    assert!(
        setup.machine.root.join("home/archixtest").is_dir(),
        "the new user's home is made on the live system"
    );
    assert_eq!(
        fs::read_link(system_path("/etc/localtime")).unwrap().to_string_lossy(),
        "/usr/share/zoneinfo/Europe/Istanbul"
    );
    assert_eq!(
        read("/etc/sysctl.d/90-archix.conf"),
        "kernel.domainname = archix.test\n"
    );
    assert!(setup.ok(&["diff"]).contains("matches the lock and the config"));

    write_system_path("/etc/archix-test.conf", "edited by hand\n");
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
        "edited by hand\n",
        "a rollback boots the deployment as it was left"
    );

    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(
        read("/etc/archix-test.conf"),
        "edited by hand\n",
        "unmanaging leaves a file someone edited"
    );
    assert_eq!(
        read("/etc/issue"),
        original_issue,
        "the original comes back when the file leaves the config"
    );
    assert!(!system_path("/etc/systemd/system/archix-test.service").exists());
    assert_eq!(uid("archixtest"), None, "the user is removed");
}

#[test]
#[ignore]
fn a_unit_that_cant_be_enabled_does_not_undo_the_switch() {
    let _system = lock_system();
    let setup = Setup::new(
        &[],
        r#"files = { ["/etc/archix-soft.conf"] = { text = "kept" } }, services = { ["archix-missing.service"] = true }"#,
    );
    let output = setup.run(&["switch", "--noconfirm"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "the command reports the failure");
    assert!(
        stderr.contains("archix-missing.service") && stderr.contains("is committed and boots at the next restart"),
        "{stderr}"
    );
    assert_eq!(
        read("/etc/archix-soft.conf"),
        "kept\n",
        "everything else is in the new deployment"
    );
    assert!(setup.machine.leftovers().is_empty());
}

fn hooks_body(conf: &str, extra_hook: &str) -> String {
    format!(
        r#"files = {{ ["/etc/archix-hook.conf"] = {{ text = {conf:?}, on_change = "echo changed >> /var/tmp/archix-on-change" }} }},
        hooks = {{
          z = {{ run = "echo z $USER >> /var/tmp/archix-hooks" }},
          a = {{ run = "echo a $USER $HOME >> /var/tmp/archix-hooks", user = "nobody", after = {{ "z" }} }},
          {extra_hook}
        }}"#
    )
}

/// Boots the machine's current deployment, as its entry would, up to activation; returns what activation said.
fn boot_current(setup: &Setup) -> String {
    let machine = &setup.machine;
    machine.reboot(Some(&format!("archix-{}.conf", machine.current().unwrap())));
    setup.ok(&["deploy-activate", "--root", machine.root.to_str().unwrap()])
}

#[test]
#[ignore]
fn hooks_run_when_the_deployment_boots_and_on_change_only_when_the_file_changed() {
    let _system = lock_system();
    let setup = Setup::new(&[], &hooks_body("one", ""));
    let root = setup.machine.root.clone();
    let log = |path: &str| fs::read_to_string(root.join(path)).unwrap_or_default();
    let out = setup.lay_out();
    assert!(out.contains("run hooks  z, a"), "{out}");
    assert_eq!(log("var/tmp/archix-hooks"), "", "hooks wait for the deployment's boot");
    boot_current(&setup);
    assert_eq!(
        log("var/tmp/archix-hooks"),
        "z root\na nobody /\n",
        "after orders hooks, and user hooks run as that user"
    );
    assert_eq!(log("var/tmp/archix-on-change"), "changed\n");
    boot_current(&setup);
    assert_eq!(
        log("var/tmp/archix-hooks").lines().count(),
        2,
        "hooks run once per switch"
    );

    assert!(setup.ok(&["switch", "--noconfirm"]).contains("nothing to do"));
    assert_eq!(
        log("var/tmp/archix-hooks").lines().count(),
        4,
        "a switch with nothing to build runs the hooks right away"
    );
    assert_eq!(log("var/tmp/archix-on-change"), "changed\n", "the file didn't change");

    write_system_path("/etc/archix-hook.conf", "edited by hand\n");
    setup.ok(&["switch", "--noconfirm"]);
    boot_current(&setup);
    assert_eq!(
        log("var/tmp/archix-on-change"),
        "changed\nchanged\n",
        "restoring a hand-edited file changes it"
    );

    setup.set_body(&[], &hooks_body("two", ""));
    setup.ok(&["switch", "--noconfirm"]);
    boot_current(&setup);
    assert_eq!(log("var/tmp/archix-on-change").lines().count(), 3);

    setup.set_body(&[], &hooks_body("three", r#"broken = { run = "false" },"#));
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(read("/etc/archix-hook.conf"), "three\n");
    let out = boot_current(&setup);
    assert!(
        out.contains("hook broken") && out.contains("runs again at the next boot"),
        "{out}"
    );
    let notes = fs::read_to_string(setup.paths.notes()).unwrap_or_default();
    assert!(notes.contains("hook broken"), "{notes}");
    let before = log("var/tmp/archix-on-change").lines().count();
    boot_current(&setup);
    assert_eq!(
        log("var/tmp/archix-on-change").lines().count(),
        before + 1,
        "what failed stays pending and runs again"
    );

    setup.ok(&["rollback", "--noconfirm"]);
    let before = log("var/tmp/archix-on-change").lines().count();
    boot_current(&setup);
    assert_eq!(
        log("var/tmp/archix-on-change").lines().count(),
        before + 1,
        "a rollback runs the target generation's on_change at its boot too"
    );
}

#[test]
#[ignore]
fn a_hook_for_a_user_the_switch_declares_runs_at_the_deployments_first_boot() {
    let _system = lock_system();
    let setup = Setup::new(
        &[],
        r#"users = { archixhook = {} }, hooks = { a = { run = "id -un > /var/tmp/archix-hook-user", user = "archixhook" } }"#,
    );
    let ran = setup.machine.root.join("var/tmp/archix-hook-user");
    setup.ok(&["switch", "--noconfirm"]);
    assert!(uid("archixhook").is_some(), "the user is in the new deployment");
    assert!(!ran.exists(), "the hook ran before the boot that makes its user");
    boot_current(&setup);
    assert_eq!(fs::read_to_string(&ran).unwrap_or_default(), "archixhook\n");
}

#[test]
#[ignore]
fn files_taken_over_unchanged_survive_being_unmanaged() {
    let _system = lock_system();
    let path = "/etc/archix-taken-over.conf";
    let setup = Setup::new(&[], "");
    let file = system_path(path);
    fs::write(&file, "already here\n").unwrap();
    fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    setup.set_body(
        &[],
        &format!("files = {{ [\"{path}\"] = {{ text = \"already here\" }} }}"),
    );
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(
        setup.machine.deployments(),
        [1],
        "a file already as wanted changes nothing"
    );
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(
        read(path),
        "already here\n",
        "unmanaging puts the original back instead of deleting it"
    );
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
    setup.lay_out();
    assert_eq!(enabled("archix-firewall.service"), "enabled");
    let rules = read(archix::system::firewall::RULES);
    assert!(
        rules.contains("tcp dport { 2222 } accept") && rules.contains("\"wg0\""),
        "{rules}"
    );
    assert_eq!(
        read("/etc/profile.d/archix-environment.sh"),
        "export EDITOR=\"nvim\"\nexport PATHS=\"/a:/b\"\n"
    );
    assert_eq!(read("/etc/environment.d/90-archix.conf"), "EDITOR=nvim\nPATHS=/a:/b\n");
    assert_eq!(read("/etc/profile.d/archix-aliases.sh"), "alias ll='ls -l'\n");

    setup.set_body(&[], &body(2223));
    setup.ok(&["switch", "--noconfirm"]);
    let rules = read(archix::system::firewall::RULES);
    assert!(
        rules.contains("tcp dport { 2223 } accept") && !rules.contains("2222"),
        "{rules}"
    );

    setup.set_body(
        &[],
        r#"firewall = { enable = true, allowed_tcp_ports = { 2223 }, extra_input_rules = "tcp dportt 9 accept" }"#,
    );
    let deployments = setup.machine.deployments();
    let err = setup.fails(&["switch", "--noconfirm"]);
    assert!(err.contains("the firewall rules don't load"), "{err}");
    assert_eq!(setup.machine.deployments(), deployments, "a bad rule builds nothing");

    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert!(!system_path(archix::system::firewall::RULES).exists());
    assert!(!system_path("/etc/profile.d/archix-environment.sh").exists());
}

#[test]
#[ignore]
fn a_removed_system_user_comes_back_with_its_old_uid() {
    let _system = lock_system();
    let setup = Setup::new(
        &[],
        "users = { archixa = { system = true }, archixsvc = { system = true } }",
    );
    setup.lay_out();
    let first = uid("archixsvc").expect("the system user is in the deployment");
    // useradd hands out system uids from the top, so without archix's map archixsvc would get archixa's old one.
    assert!(uid("archixa").unwrap() > first);
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(uid("archixsvc"), None, "the account is gone");
    setup.set_body(&[], "users = { archixsvc = { system = true } }");
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(
        uid("archixsvc"),
        Some(first),
        "a user created again gets its old uid, so its leftover files are still its own"
    );
}

#[test]
#[ignore]
fn files_on_fat_are_not_edited_just_because_fat_has_no_modes() {
    let _system = lock_system();
    let dir = tempfile::tempdir().unwrap();
    let setup = Setup::new(&[], "");
    let image = dir.path().join("esp.img");
    let esp = "/mnt/archix-test-esp";
    let mount = setup.machine.root.join(esp.trim_start_matches('/'));
    let run = |cmd: &str, args: &[&str]| {
        assert!(
            Command::new(cmd).args(args).status().unwrap().success(),
            "{cmd} {args:?}"
        )
    };
    run("truncate", &["-s", "64M", image.to_str().unwrap()]);
    run("mkfs.fat", &["-F", "32", image.to_str().unwrap()]);
    fs::create_dir_all(&mount).unwrap();
    run(
        "mount",
        &[
            "-o",
            "loop,fmask=0022",
            image.to_str().unwrap(),
            mount.to_str().unwrap(),
        ],
    );
    setup.set_body(
        &[],
        &format!("files = {{ [\"{esp}/loader/entries/extra.conf\"] = {{ text = \"title extra\" }} }}"),
    );
    setup.ok(&["switch", "--noconfirm"]);
    let again = setup.ok(&["switch", "--noconfirm"]);
    let written = fs::read_to_string(mount.join("loader/entries/extra.conf")).unwrap_or_default();
    run("umount", &[mount.to_str().unwrap()]);
    assert_eq!(
        written, "title extra\n",
        "a file outside the deployment trees is written on the live system"
    );
    assert!(
        again.contains("nothing to do") && !again.contains("edited outside archix"),
        "FAT reports 0755 root for every file, which isn't an edit:\n{again}"
    );
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

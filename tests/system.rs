//! Manages /etc files, users and services on the root; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

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

/// What `systemctl is-enabled` says about `unit` on the machine.
fn enabled(unit: &str) -> String {
    let root = system_path("/etc").parent().unwrap().to_path_buf();
    let output = Command::new("systemctl")
        .arg(format!("--root={}", root.display()))
        .args(["is-enabled", unit])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A file of the machine.
fn read(path: &str) -> String {
    fs::read_to_string(system_path(path)).unwrap_or_default()
}

/// The groups a user is in, by the machine's account files.
fn groups_of(user: &str) -> Vec<String> {
    read("/etc/group")
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(':').collect();
            (fields.len() == 4 && fields[3].split(',').any(|member| member == user)).then(|| fields[0].to_string())
        })
        .collect()
}

/// A user's uid on the machine.
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
    let out = setup.first_switch();
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
        "the new user's home is made"
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
        "one\n",
        "a rollback applies the older generation's file in place"
    );

    write_system_path("/etc/archix-test.conf", "edited by hand\n");
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
        stderr.contains("archix-missing.service") && stderr.contains("is applied and current"),
        "{stderr}"
    );
    assert_eq!(read("/etc/archix-soft.conf"), "kept\n", "everything else is applied");
    assert_eq!(setup.machine.current(), Some(1));
}

fn hooks_body(conf: &str, extra_hook: &str) -> String {
    format!(
        r#"files = {{ ["/etc/archix-hook.conf"] = {{ text = {conf:?}, on_change = "echo changed >> /var/tmp/archix-on-change" }} }},
        hooks = {{
          z = {{ run = "echo z $USER >> /var/tmp/archix-hooks; chmod 0666 /var/tmp/archix-hooks" }},
          a = {{ run = "echo a $USER $HOME >> /var/tmp/archix-hooks", user = "nobody", after = {{ "z" }} }},
          {extra_hook}
        }}"#
    )
}

#[test]
#[ignore]
fn hooks_run_after_each_switch_and_on_change_only_when_the_file_changed() {
    let _system = lock_system();
    let setup = Setup::new(&[], &hooks_body("one", ""));
    let root = setup.machine.root.clone();
    let log = |path: &str| fs::read_to_string(root.join(path)).unwrap_or_default();
    let out = setup.first_switch();
    assert!(out.contains("run hooks  z, a"), "{out}");
    assert_eq!(
        log("var/tmp/archix-hooks"),
        "z root\na nobody /\n",
        "after orders hooks, and user hooks run as that user"
    );
    assert_eq!(log("var/tmp/archix-on-change"), "changed\n");

    assert!(setup.ok(&["switch", "--noconfirm"]).contains("nothing to do"));
    assert_eq!(
        log("var/tmp/archix-hooks").lines().count(),
        4,
        "a switch with nothing to apply runs the hooks too"
    );
    assert_eq!(log("var/tmp/archix-on-change"), "changed\n", "the file didn't change");

    write_system_path("/etc/archix-hook.conf", "edited by hand\n");
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(
        log("var/tmp/archix-on-change"),
        "changed\nchanged\n",
        "restoring a hand-edited file changes it"
    );

    setup.set_body(&[], &hooks_body("two", ""));
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(log("var/tmp/archix-on-change").lines().count(), 3);

    setup.set_body(&[], &hooks_body("three", r#"broken = { run = "false" },"#));
    let output = setup.run(&["switch", "--noconfirm"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success() && stderr.contains("hook broken") && stderr.contains("is applied and current"),
        "a failing hook is a soft failure: {stderr}"
    );
    assert_eq!(read("/etc/archix-hook.conf"), "three\n");
    assert_eq!(log("var/tmp/archix-on-change").lines().count(), 4);

    setup.ok(&["rollback", "--noconfirm"]);
    assert_eq!(read("/etc/archix-hook.conf"), "two\n");
    assert_eq!(
        log("var/tmp/archix-on-change").lines().count(),
        5,
        "a rollback runs the target generation's on_change"
    );
}

#[test]
#[ignore]
fn a_hook_for_a_user_the_switch_declares_runs_once_the_user_is_made() {
    let _system = lock_system();
    let setup = Setup::new(
        &[],
        r#"users = { archixhook = {} }, hooks = { a = { run = "id -un > /var/tmp/archix-hook-user", user = "archixhook" } }"#,
    );
    let ran = setup.machine.root.join("var/tmp/archix-hook-user");
    setup.ok(&["switch", "--noconfirm"]);
    assert!(uid("archixhook").is_some());
    assert_eq!(fs::read_to_string(&ran).unwrap_or_default(), "archixhook\n");
}

fn on_change_body(text: &str) -> String {
    format!(
        r#"users = {{ archixlive = {{}} }},
        files = {{
          ["/etc/archix-boot.conf"] = {{ text = {text:?}, on_change = "echo etc >> /var/tmp/archix-on-change-etc" }},
          ["/srv/archix-live.conf"] = {{ text = {text:?}, on_change = "echo srv >> /var/tmp/archix-on-change-srv" }},
        }},
        home = {{ user = "archixlive", files = {{ [".config/archix-live"] = {{ text = {text:?}, on_change = "echo $USER >> ~/archix-on-change" }} }} }}"#
    )
}

#[test]
#[ignore]
fn on_change_runs_after_the_switch_that_writes_its_file_and_after_a_rollback() {
    let _system = lock_system();
    let setup = Setup::new(&[], "users = { archixlive = {} }");
    setup.first_switch();
    let root = setup.machine.root.clone();
    let runs = || -> [usize; 3] {
        [
            "var/tmp/archix-on-change-etc",
            "var/tmp/archix-on-change-srv",
            "home/archixlive/archix-on-change",
        ]
        .map(|log| fs::read_to_string(root.join(log)).unwrap_or_default().lines().count())
    };

    setup.set_body(&[], &on_change_body("one"));
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(runs(), [1, 1, 1]);
    assert_eq!(
        fs::read_to_string(root.join("home/archixlive/archix-on-change")).unwrap(),
        "archixlive\n",
        "a home file's on_change runs as its user"
    );
    assert!(setup.ok(&["switch", "--noconfirm"]).contains("nothing to do"));
    assert_eq!(runs(), [1, 1, 1], "nothing runs twice");

    setup.set_body(&[], &on_change_body("two"));
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(runs(), [2, 2, 2]);
    setup.ok(&["rollback", "--noconfirm"]);
    assert_eq!(runs(), [3, 3, 3]);
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
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(
        !out.contains("/etc/archix-taken-over.conf"),
        "a file already as wanted changes nothing:\n{out}"
    );
    assert_eq!(read(path), "already here\n");
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
            r#"firewall = {{ enable = true, allowed_tcp_ports = {{ {port}, 22 }}, trusted_interfaces = {{ "wg0" }}, allowed_services = {{ "ssh", "kdeconnect", "steam", "game" }}, service_ports = {{ game = {{ udp = {{ 2456 }} }} }} }},
            environment = {{ variables = {{ EDITOR = "nvim", PATHS = {{ "/a", "/b" }} }}, shell_aliases = {{ ll = "ls -l" }} }}"#
        )
    };
    let setup = Setup::new(&[], &body(2222));
    setup.first_switch();
    assert_eq!(enabled("archix-firewall.service"), "enabled");
    let rules = read(archix::system::firewall::RULES);
    assert!(
        rules.contains("tcp dport { 22, 2222 } accept\n") && rules.contains("\"wg0\""),
        "{rules}"
    );
    for rule in [
        "\n    ip saddr @archix_lan4 tcp dport { 1714-1764 } accept comment \"kdeconnect\"\n",
        "\n    ip6 saddr @archix_lan6 udp dport { 1714-1764 } accept comment \"kdeconnect\"\n",
        "\n    ip saddr @archix_lan4 udp dport { 10400, 10401, 27031-27035, 27036 } accept comment \"steam\"\n",
        "\n    udp dport { 2456 } accept comment \"game\"\n",
    ] {
        assert!(rules.contains(rule), "{rule} in {rules}");
    }
    assert!(!rules.contains("comment \"ssh\""), "22 is open already: {rules}");
    let check = Command::new("nft")
        .args(["-c", "-f"])
        .arg(system_path(archix::system::firewall::RULES))
        .output()
        .unwrap();
    assert!(check.status.success(), "{}", String::from_utf8_lossy(&check.stderr));
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
        rules.contains("tcp dport { 22, 2223 } accept") && !rules.contains("2222"),
        "{rules}"
    );

    setup.set_body(
        &[],
        r#"firewall = { enable = true, allowed_tcp_ports = { 2223 }, extra_input_rules = "tcp dportt 9 accept" }"#,
    );
    let current = setup.machine.current();
    let err = setup.fails(&["switch", "--noconfirm"]);
    assert!(err.contains("the firewall rules don't load"), "{err}");
    assert_eq!(setup.machine.current(), current, "a bad rule applies nothing");

    setup.set_body(
        &[],
        &format!(
            r#"firewall = {{ enable = true, allowed_services = {{ "ssh" }} }},
            units = {{ ["ufw.service"] = {UNIT:?} }},
            services = {{ ["ufw.service"] = true }}"#
        ),
    );
    let err = setup.fails(&["switch", "--noconfirm"]);
    assert!(
        err.contains("ufw.service would run beside the archix firewall (the config enables it)"),
        "{err}"
    );
    assert_eq!(setup.machine.current(), current, "ufw beside it applies nothing");

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
    setup.first_switch();
    let first = uid("archixsvc").expect("the system user is made");
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
        "a file on another filesystem is written there"
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

#[test]
#[ignore]
fn a_swap_file_on_btrfs_gets_a_subvolume_of_its_own_and_never_shares_one() {
    let dir = tempfile::TempDir::new().unwrap();
    let (image, root) = (dir.path().join("btrfs.img"), dir.path().join("root"));
    fs::create_dir_all(&root).unwrap();
    let sh = |command: &str| {
        let ok = Command::new("sh").arg("-c").arg(command).status().unwrap().success();
        assert!(ok, "{command}");
    };
    sh(&format!(
        "truncate -s 512M {0} && mkfs.btrfs -q {0} && mount -o loop {0} {1}",
        image.display(),
        root.display()
    ));
    fs::create_dir_all(root.join("data")).unwrap();
    let line = |file: &str| archix::system::fstab::FstabLine {
        device: file.into(),
        point: "none".into(),
        fs_type: "swap".into(),
        options: "nofail".into(),
        passno: 0,
        size: Some(64 << 20),
    };
    let checks = Checks::default();
    let shared = archix::system::fstab::check(&root, &[line("/data/swapfile")], "", "");
    checks.check(
        shared
            .as_ref()
            .is_err_and(|e| e.to_string().contains("isn't a subvolume of its own")),
        format!("a swap file archix would make in a plain btrfs directory is refused: {shared:?}"),
    );
    checks.check(
        archix::system::fstab::check(&root, &[line("/swap/swapfile")], "", "").is_ok(),
        "a missing directory is fine: archix creates it as a subvolume",
    );
    let made = archix::system::fstab::make_swapfile(&root.join("swap/swapfile"), 64 << 20);
    checks.check(made.is_ok(), format!("making the swap file: {made:?}"));
    let subvolume = Command::new("btrfs")
        .args(["subvolume", "show"])
        .arg(root.join("swap"))
        .output()
        .unwrap();
    checks.check(subvolume.status.success(), "its directory is a subvolume of its own");
    let attrs = Command::new("lsattr").arg(root.join("swap/swapfile")).output().unwrap();
    let attrs = String::from_utf8_lossy(&attrs.stdout).into_owned();
    checks.check(
        attrs.split_whitespace().next().is_some_and(|flags| flags.contains('C')),
        format!("btrfs filesystem mkswapfile makes it NOCOW: {attrs}"),
    );
    checks.check(
        archix::system::fstab::check(&root, &[line("/swap/swapfile")], "", "").is_ok(),
        "the next switch accepts it",
    );
    sh(&format!("umount {}", root.display()));
    checks.finish();
}

/// What `getcap` prints for `path`, empty when it has no capabilities.
fn getcap(path: &std::path::Path) -> String {
    let output = Command::new("getcap").arg(path).output().unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
#[ignore]
fn capabilities_go_on_a_copy_of_their_own_and_follow_switches_and_rollbacks() {
    let _system = lock_system();
    let with_caps = r#"capabilities = { ["/usr/bin/tree"] = "cap_sys_nice+ep" }"#;
    let setup = Setup::new(&["tree"], "");
    setup.first_switch();
    let machine = &setup.machine;
    let tree = system_path("/usr/bin/tree");
    assert_eq!(getcap(&tree), "");

    setup.set_body(&["tree"], with_caps);
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(out.contains("setcap     /usr/bin/tree cap_sys_nice=ep"), "{out}");
    assert!(getcap(&tree).ends_with("cap_sys_nice=ep"), "{}", getcap(&tree));
    let capped = machine.current().unwrap();
    assert!(setup.ok(&["switch", "--noconfirm"]).contains("nothing to do"));

    setup.set_body(
        &["tree"],
        &format!(r#"{with_caps}, files = {{ ["/etc/archix-caps"] = {{ text = "x" }} }}"#),
    );
    setup.ok(&["switch", "--noconfirm"]);
    let again = machine.current().unwrap();
    assert!(getcap(&tree).ends_with("cap_sys_nice=ep"), "a switch keeps them");

    setup.set_body(&["tree"], "");
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(out.contains("setcap     /usr/bin/tree (capabilities removed)"), "{out}");
    assert_eq!(getcap(&tree), "");

    setup.ok(&["rollback", &again.to_string(), "--noconfirm"]);
    assert_eq!(machine.current(), Some(again));
    assert!(getcap(&tree).ends_with("cap_sys_nice=ep"));
    assert_ne!(capped, again);

    setup.set_body(
        &["tree"],
        r#"capabilities = { ["/usr/bin/archix-no-such-file"] = "cap_sys_nice+ep", ["/usr/bin/tree"] = "cap_sys_nice+ep" }"#,
    );
    let err = setup.fails(&["switch", "--noconfirm"]);
    assert!(
        err.contains("capabilities: /usr/bin/archix-no-such-file doesn't exist"),
        "{err}"
    );
    assert_eq!(machine.current(), Some(again));
    setup.set_body(&["tree"], r#"capabilities = { ["/usr/bin/tree"] = "cap_sys_nice+e" }"#);
    let err = setup.fails(&["switch", "--noconfirm"]);
    assert!(err.contains("leaves the file without capabilities"), "{err}");
}

const MOUNTS: &str = r#"file_systems = {
  ["/mnt/data"] = { device = "UUID=archix-test-data", fs_type = "ext4", check = true },
  ["/srv/archix-www"] = { device = "/var/archix-www", options = { "bind" } },
  ["/mnt/secret"] = { device = "/dev/mapper/archixsecret", fs_type = "ext4", encrypted = { device = "UUID=archix-test-luks", key_file = "/etc/archix-secret.key" } },
},
swap = { { file = "/swap/swapfile", size = "64M" } }"#;

#[test]
#[ignore]
fn file_systems_and_swap_are_the_fstab_after_archixs_own_root_and_esp_lines() {
    let _system = lock_system();
    let setup = Setup::new(&[], MOUNTS);
    let machine = &setup.machine;
    let esp = machine.esp().display().to_string();
    fs::write(
        machine.root.join("etc/fstab"),
        format!("# from genfstab\nUUID=archix-root / ext4 rw,relatime 0 1\nUUID=AB12-CD34 {esp} vfat rw 0 0\nUUID=archix-home /home ext4 rw 0 2\n"),
    )
    .unwrap();
    let checks = Checks::default();
    let refused = setup.fails(&["switch", "--noconfirm"]);
    checks.check(
        refused.contains("  UUID=archix-home /home ext4 rw 0 2    (/home is protected)\n")
            && refused.contains("[\"/home\"] = { device = \"UUID=archix-home\", fs_type = \"ext4\", options = { \"rw\" }, check = true },")
            && refused.contains("drop_mounts = { \"/home\" }; nothing was changed"),
        format!("a switch that would drop a hand-added /home line is refused, naming it:\n{refused}"),
    );
    checks.eq(machine.current(), None, "the refused switch changes nothing");
    setup.set_body(&[], &format!("{MOUNTS},\ndrop_mounts = {{ \"/home\" }}"));
    let out = setup.ok(&["switch", "--noconfirm"]);
    let first = machine.current().unwrap();
    checks.eq(
        read("/etc/fstab"),
        format!(
            "{}\nUUID=archix-root / ext4 rw,relatime 0 1\nUUID=AB12-CD34 {esp} vfat rw,x-systemd.automount,x-systemd.idle-timeout=60,fmask=0077,dmask=0077 0 2\n\
             UUID=archix-test-data /mnt/data ext4 nofail,x-systemd.device-timeout=30s 0 2\n\
             /dev/mapper/archixsecret /mnt/secret ext4 nofail 0 0\n\
             /var/archix-www /srv/archix-www none bind 0 0\n\
             /swap/swapfile none swap nofail 0 0\n",
            archix::system::fstab::HEADER
        ),
        "fstab is the root's line, the ESP's with its protections, then the declared lines",
    );
    for line in [
        "fstab      mount /mnt/data (UUID=archix-test-data) at the next boot",
        "fstab      swap on /swap/swapfile at the next boot",
        "fstab      drop /home (UUID=archix-home) at the next boot",
        "fstab      the ESP at",
        "UUID=archix-test-data for /mnt/data isn't attached now",
    ] {
        checks.check(out.contains(line), format!("the switch should say {line:?}:\n{out}"));
    }
    checks.eq(
        read("/etc/crypttab"),
        "# Written by archix from file_systems' encrypted devices.\narchixsecret UUID=archix-test-luks /etc/archix-secret.key luks,nofail\n".to_string(),
        "an encrypted mount gets its crypttab line",
    );
    checks.check(
        read("/etc/systemd/system/archix-swapfile-swap-swapfile.service")
            .contains("ExecStart=/usr/bin/archix swapfile /swap/swapfile 67108864\n")
            && read("/etc/systemd/system/swap-swapfile.swap.d/archix-swapfile.conf")
                .contains("Requires=archix-swapfile-swap-swapfile.service"),
        "a swap file with a size is made by a unit before its swap unit",
    );
    let diff = setup.ok(&["diff"]);
    checks.check(
        diff.contains("matches the lock and the config"),
        format!("nothing is left to change:\n{diff}"),
    );
    let status = setup.ok(&["status"]);
    checks.check(
        !status.contains("/etc/fstab"),
        format!("the fstab archix writes isn't carried by hand:\n{status}"),
    );

    let refused = [
        (
            r#"file_systems = { ["/mnt/x"] = { device = "/dev/mapper/archixnothing" } }"#,
            "no /etc/crypttab line opens",
        ),
        (
            r#"files = { ["/etc/fstab"] = { text = "UUID=1 / ext4 rw 0 1" } }"#,
            "declare mounts in file_systems and swap",
        ),
        (r#"file_systems = { ["/usr"] = { device = "UUID=1" } }"#, "own /usr"),
    ];
    for (body, says) in refused {
        setup.set_body(&[], body);
        let error = setup.fails(&["switch", "--noconfirm"]);
        checks.check(error.contains(says), format!("{body}: {error}"));
        checks.eq(machine.current(), Some(first), "a refused switch changes nothing");
    }

    setup.set_body(&[], MOUNTS);
    let fstab = system_path("/etc/fstab");
    let crypttab = system_path("/etc/crypttab");
    let mut text = read("/etc/fstab");
    text.push_str("UUID=archix-usb /mnt/archix-usb ext4 rw,nofail 0 0\n/dev/mapper/archixhome /home/archix-crypt ext4 defaults 0 0\n");
    fs::write(&fstab, text).unwrap();
    let mut text = read("/etc/crypttab");
    text.push_str("archixhome UUID=archix-test-home none luks\n");
    fs::write(&crypttab, text).unwrap();
    for (command, out) in [("status", setup.ok(&["status"])), ("diff", setup.ok(&["diff"]))] {
        checks.check(
            out.contains("/etc/fstab was edited since archix wrote it")
                && out.contains("  + UUID=archix-usb /mnt/archix-usb ext4 rw,nofail 0 0    (dropped then, unless file_systems declares it)")
                && out.contains("/etc/crypttab was edited since archix wrote it"),
            format!("{command} names the hand-added lines the next switch drops:\n{out}"),
        );
    }
    let usb = machine.root.join("mnt/archix-usb");
    fs::create_dir_all(&usb).unwrap();
    let mounted = Command::new("mount")
        .args(["-t", "tmpfs", "tmpfs"])
        .arg(&usb)
        .status()
        .unwrap();
    assert!(mounted.success());
    let error = setup.fails(&["switch", "--noconfirm"]);
    let unmounted = Command::new("umount").arg(&usb).status().unwrap();
    assert!(unmounted.success());
    for says in [
        "  UUID=archix-usb /mnt/archix-usb ext4 rw,nofail 0 0    (/mnt/archix-usb is mounted now)\n",
        "  /dev/mapper/archixhome /home/archix-crypt ext4 defaults 0 0    (/home/archix-crypt is under /home)\n",
        "  archixhome UUID=archix-test-home none luks    (it backs /home/archix-crypt)\n",
        "drop_mounts = { \"/mnt/archix-usb\", \"/home/archix-crypt\", \"/dev/mapper/archixhome\" }; nothing was changed",
    ] {
        checks.check(
            error.contains(says),
            format!("the refusal should name {says:?}:\n{error}"),
        );
    }
    checks.eq(machine.current(), Some(first), "a refused drop changes nothing");
    let error = setup.fails(&["switch", "--noconfirm"]);
    checks.check(
        !error.contains("/mnt/archix-usb") && error.contains("/home/archix-crypt"),
        format!("unmounted, a line outside /home, /var and /srv may go:\n{error}"),
    );
    setup.set_body(&[], &format!("{MOUNTS},\ndrop_mounts = {{ \"/home/archix-crypt\" }}"));
    setup.ok(&["switch", "--noconfirm"]);
    checks.check(
        !read("/etc/fstab").contains("/home/archix-crypt")
            && !read("/etc/fstab").contains("/mnt/archix-usb")
            && !read("/etc/crypttab").contains("archixhome")
            && read("/etc/crypttab").contains("archixsecret"),
        format!(
            "drop_mounts lets the mount and the volume backing it go:\n{}\n{}",
            read("/etc/fstab"),
            read("/etc/crypttab")
        ),
    );
    setup.set_body(
        &[],
        r#"file_systems = { ["/srv/archix-www"] = { device = "/var/archix-www", options = { "bind" } } }"#,
    );
    let out = setup.ok(&["switch", "--noconfirm"]);
    checks.check(
        !read("/etc/fstab").contains("/mnt/data") && out.contains("fstab      drop /mnt/data (UUID=archix-test-data)"),
        format!("a mount left out of the config leaves fstab:\n{out}"),
    );
    checks.check(
        !system_path("/etc/crypttab").exists() || !read("/etc/crypttab").contains("archixsecret"),
        "and its crypttab line goes with it",
    );
    setup.ok(&["rollback", "--noconfirm"]);
    checks.check(
        read("/etc/fstab").contains("UUID=archix-test-data /mnt/data"),
        "a rollback brings back the fstab of the generation it rolls back to",
    );
    checks.finish();
}

#[test]
#[ignore]
fn a_swap_file_is_made_once_and_again_only_at_another_size() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("swap/swapfile");
    let make = |bytes: u64| {
        let output = Command::new(env!("CARGO_BIN_EXE_archix"))
            .arg("swapfile")
            .arg(&file)
            .arg(bytes.to_string())
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let signature = |file: &std::path::Path| fs::read(file).unwrap()[4086..4096].to_vec();
    use std::os::unix::fs::MetadataExt;
    assert!(make(16 << 20).contains("made the swap file"));
    let meta = fs::metadata(&file).unwrap();
    assert_eq!((meta.len(), meta.mode() & 0o777), (16 << 20, 0o600));
    assert_eq!(signature(&file), b"SWAPSPACE2");
    assert_eq!(make(16 << 20), "", "a swap file of the right size is left alone");
    assert_eq!(fs::metadata(&file).unwrap().ino(), meta.ino());
    assert!(make(8 << 20).contains("again"));
    assert_eq!(fs::metadata(&file).unwrap().len(), 8 << 20);
    assert_eq!(signature(&file), b"SWAPSPACE2");
    assert!(
        fs::read_dir(file.parent().unwrap()).unwrap().count() == 1,
        "no temporary file is left"
    );
}

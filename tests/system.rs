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
          z = {{ run = "echo z $USER >> /var/tmp/archix-hooks; chmod 0666 /var/tmp/archix-hooks" }},
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
fn on_change_runs_now_for_files_written_now_and_at_boot_for_the_deployments() {
    let _system = lock_system();
    let setup = Setup::new(&[], "users = { archixlive = {} }");
    setup.lay_out();
    boot_current(&setup);
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
    assert_eq!(
        runs(),
        [0, 1, 1],
        "files outside the deployment are written now, so their on_change runs now, and the /etc one waits"
    );
    assert_eq!(
        fs::read_to_string(root.join("home/archixlive/archix-on-change")).unwrap(),
        "archixlive\n",
        "a home file's on_change runs as its user"
    );
    boot_current(&setup);
    assert_eq!(runs(), [1, 1, 1], "the /etc one runs at the boot");
    boot_current(&setup);
    assert_eq!(runs(), [1, 1, 1], "nothing runs twice");

    setup.set_body(&[], &on_change_body("two"));
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(runs(), [1, 2, 2]);
    boot_current(&setup);
    assert_eq!(runs(), [2, 2, 2]);
    setup.ok(&["rollback", "--noconfirm"]);
    assert_eq!(
        runs(),
        [2, 3, 3],
        "a rollback writes the files outside the deployment now, and runs their on_change now too"
    );
    boot_current(&setup);
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
            r#"firewall = {{ enable = true, allowed_tcp_ports = {{ {port}, 22 }}, trusted_interfaces = {{ "wg0" }}, allowed_services = {{ "ssh", "kdeconnect", "steam", "game" }}, service_ports = {{ game = {{ udp = {{ 2456 }} }} }} }},
            environment = {{ variables = {{ EDITOR = "nvim", PATHS = {{ "/a", "/b" }} }}, shell_aliases = {{ ll = "ls -l" }} }}"#
        )
    };
    let setup = Setup::new(&[], &body(2222));
    setup.lay_out();
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
    let deployments = setup.machine.deployments();
    let err = setup.fails(&["switch", "--noconfirm"]);
    assert!(err.contains("the firewall rules don't load"), "{err}");
    assert_eq!(setup.machine.deployments(), deployments, "a bad rule builds nothing");

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
    assert_eq!(setup.machine.deployments(), deployments, "ufw beside it builds nothing");

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

fn usr_local_body(text: &str) -> String {
    format!(
        r#"files = {{ ["/usr/local/bin/archix-declared"] = {{ text = {text:?}, mode = "0755", on_change = "echo ran >> /var/tmp/archix-usr-local-on-change" }} }}"#
    )
}

#[test]
#[ignore]
fn usr_local_is_shared_writable_written_at_once_and_never_rolled_back() {
    let _system = lock_system();
    let setup = Setup::new(&[], &usr_local_body("one"));
    let machine = &setup.machine;
    let root = machine.root.clone();
    let live = |rel: &str| fs::read_to_string(root.join("usr/local").join(rel)).unwrap_or_default();
    let ran = || fs::read_to_string(root.join("var/tmp/archix-usr-local-on-change")).unwrap_or_default();
    setup.ok(&["switch", "--noconfirm"]);
    let first = machine.current().unwrap();
    assert_eq!(
        fs::read_to_string(root.join("var/usrlocal/bin/archix-declared")).unwrap(),
        "one\n",
        "a /usr/local file is written to the shared /var/usrlocal right after the commit"
    );
    assert_eq!(
        live("bin/archix-declared"),
        "one\n",
        "the first layout bound /var/usrlocal over the running /usr/local"
    );
    assert_eq!(ran(), "ran\n", "its on_change runs right away");
    for n in machine.deployments() {
        assert_eq!(
            archix::system::usr_local::staged_files(&machine.deployment(n)),
            Vec::<String>::new(),
            "deployment {n}'s own /usr/local holds only the filesystem package's skeleton"
        );
    }
    machine.reboot(Some(&format!("archix-{first}.conf")));
    assert_eq!(
        fs::read_to_string(root.join("run/archix/usr-local")).unwrap(),
        "mounted\n"
    );
    fs::write(root.join("usr/local/bin/handmade"), "curl | sh\n").unwrap();
    assert!(
        root.join("var/usrlocal/bin/handmade").exists(),
        "/usr/local is writable and shared"
    );
    let err = fs::write(root.join("usr/handmade"), "x").unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EROFS), "/usr stays read-only");
    assert!(setup.ok(&["diff"]).contains("matches the lock and the config"));

    fs::write(root.join("usr/local/bin/archix-declared"), "edited by hand\n").unwrap();
    assert!(
        setup
            .ok(&["diff"])
            .contains("/usr/local/bin/archix-declared (edited outside archix")
    );
    setup.set_body(&[], &usr_local_body("two"));
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(
        live("bin/archix-declared"),
        "two\n",
        "written now, not at the next boot"
    );
    assert_eq!(ran(), "ran\nran\n");

    setup.ok(&["rollback", &first.to_string(), "--noconfirm"]);
    assert_eq!(machine.current(), Some(first));
    assert_eq!(
        live("bin/archix-declared"),
        "one\n",
        "a managed file follows the generation, as /srv's do"
    );
    assert_eq!(
        live("bin/handmade"),
        "curl | sh\n",
        "a rollback leaves what was installed to /usr/local by hand"
    );
    machine.reboot(Some(&format!("archix-{first}.conf")));
    assert_eq!(
        live("bin/handmade"),
        "curl | sh\n",
        "and so does the boot of the rolled-back deployment"
    );

    setup.set_body(&[], &usr_local_body("three"));
    setup.ok(&["switch", "--noconfirm"]);
    let newest = machine.current().unwrap();
    let out = machine.reboot(Some(&format!("archix-{first}.conf")));
    assert!(out.contains("leaving /usr/local unmounted"), "{out}");
    assert!(
        !root.join("usr/local/bin/handmade").exists(),
        "a boot of an older deployment than current leaves /usr/local out"
    );
    let cmdline = ("ARCHIX_TEST_CMDLINE", "root=/dev/vda2 rw");
    let entry = format!("archix-{first}.conf");
    setup.ok_env(&[cmdline, ("ARCHIX_TEST_ENTRY", entry.as_str())], &["boot-check"]);
    let notes = fs::read_to_string(setup.paths.notes()).unwrap_or_default();
    assert!(
        notes.contains("left /usr/local unmounted") && notes.contains(&format!("older than the current {newest}")),
        "{notes}"
    );
    machine.reboot(Some(&format!("archix-{first}.conf")));
    setup.ok_env(&[cmdline, ("ARCHIX_TEST_ENTRY", entry.as_str())], &["boot-check"]);
    assert_eq!(
        live("bin/handmade"),
        "curl | sh\n",
        "the next boot, of what is current now, binds it again"
    );
    let notes = fs::read_to_string(setup.paths.notes()).unwrap_or_default();
    assert!(!notes.contains("/usr/local"), "{notes}");

    let unit = root.join("usr/local/lib/systemd/system/archix-local.service");
    fs::create_dir_all(unit.parent().unwrap()).unwrap();
    fs::write(&unit, "[Service]\nExecStart=/usr/bin/true\n").unwrap();
    for command in ["status", "verify"] {
        let out = setup.ok(&[command]);
        assert!(
            out.contains("/usr/local/lib/systemd/system/archix-local.service")
                && out.contains("no rollback undoes them"),
            "{command}: {out}"
        );
    }

    // A boot that left /usr/local out with a boot-time file in it holds it out of the boots after, though they boot what is current then.
    setup.set_body(&[], &usr_local_body("four"));
    setup.ok(&["switch", "--noconfirm"]);
    assert_ne!(machine.current(), Some(first));
    machine.reboot(Some(&format!("archix-{first}.conf")));
    setup.ok_env(&[cmdline, ("ARCHIX_TEST_ENTRY", entry.as_str())], &["boot-check"]);
    assert_eq!(
        machine.current(),
        Some(first),
        "booting the older entry made it current"
    );
    let held = archix::system::usr_local::hold_path(&setup.paths.state);
    assert!(fs::read_to_string(&held).unwrap().starts_with("held "));
    for _ in 0..2 {
        let out = machine.reboot(Some(&format!("archix-{first}.conf")));
        assert!(
            out.contains("leaving /usr/local unmounted: it is held out since"),
            "{out}"
        );
        assert!(!unit.exists() && !root.join("usr/local/bin/handmade").exists());
        setup.ok_env(&[cmdline, ("ARCHIX_TEST_ENTRY", entry.as_str())], &["boot-check"]);
        let notes = fs::read_to_string(setup.paths.notes()).unwrap_or_default();
        assert!(
            notes.contains("archix-local.service") && notes.contains("sudo archix usr-local quarantine"),
            "{notes}"
        );
    }
    assert!(setup.ok(&["status"]).contains("/usr/local: every boot leaves it out"));
    let said = setup.ok(&["usr-local", "quarantine"]);
    assert!(said.contains("archix-local.service"), "{said}");
    let out = machine.reboot(Some(&format!("archix-{first}.conf")));
    assert!(out.contains("binding /usr/local again"), "{out}");
    setup.ok_env(&[cmdline, ("ARCHIX_TEST_ENTRY", entry.as_str())], &["boot-check"]);
    assert_eq!(
        live("bin/handmade"),
        "curl | sh\n",
        "the boot after the quarantine binds it"
    );
    assert!(!unit.exists());
    assert!(fs::read_to_string(&held).unwrap().starts_with("trying "));
    setup.ok_env(&[cmdline, ("ARCHIX_TEST_ENTRY", entry.as_str())], &["boot-complete"]);
    assert!(!held.exists(), "reaching multi-user.target lifts the hold");
    let notes = fs::read_to_string(setup.paths.notes()).unwrap_or_default();
    assert!(!notes.contains("/usr/local"), "{notes}");
}

/// What `getcap` prints for `path`, empty when it has no capabilities.
fn getcap(path: &std::path::Path) -> String {
    let output = Command::new("getcap").arg(path).output().unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn ino(path: &std::path::Path) -> u64 {
    std::os::unix::fs::MetadataExt::ino(&fs::metadata(path).unwrap())
}

#[test]
#[ignore]
fn capabilities_go_on_a_copy_in_the_new_deployment_and_follow_switches_and_rollbacks() {
    let _system = lock_system();
    let keep = "deployments = { keep = 6 }";
    let with_caps = format!(r#"{keep}, capabilities = {{ ["/usr/bin/tree"] = "cap_sys_nice+ep" }}"#);
    let setup = Setup::new(&["tree"], keep);
    setup.lay_out();
    let machine = &setup.machine;
    let tree = |number: u32| machine.deployment(number).join("usr/bin/tree");
    let plain = machine.current().unwrap();
    assert_eq!(getcap(&tree(plain)), "");

    setup.set_body(&["tree"], &with_caps);
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(out.contains("setcap     /usr/bin/tree cap_sys_nice=ep"), "{out}");
    let capped = machine.current().unwrap();
    assert!(
        getcap(&tree(capped)).ends_with("cap_sys_nice=ep"),
        "{}",
        getcap(&tree(capped))
    );
    assert_eq!(
        getcap(&tree(plain)),
        "",
        "the deployment it was staged from, which shared the file's inode, is unchanged"
    );
    assert_ne!(ino(&tree(plain)), ino(&tree(capped)));
    assert!(setup.ok(&["switch", "--noconfirm"]).contains("nothing to do"));

    setup.set_body(
        &["tree"],
        &format!(r#"{with_caps}, files = {{ ["/etc/archix-caps"] = {{ text = "x" }} }}"#),
    );
    setup.ok(&["switch", "--noconfirm"]);
    let again = machine.current().unwrap();
    assert!(getcap(&tree(again)).ends_with("cap_sys_nice=ep"), "a switch keeps them");
    assert_eq!(
        ino(&tree(again)),
        ino(&tree(capped)),
        "an unchanged file is shared again"
    );

    setup.set_body(&["tree"], keep);
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(out.contains("setcap     /usr/bin/tree (capabilities removed)"), "{out}");
    let removed = machine.current().unwrap();
    assert_eq!(getcap(&tree(removed)), "");
    assert!(
        getcap(&tree(again)).ends_with("cap_sys_nice=ep"),
        "dropping them in a new deployment leaves the older ones theirs"
    );
    setup.ok(&["verify"]);

    setup.ok(&["rollback", &again.to_string(), "--noconfirm"]);
    assert_eq!(machine.current(), Some(again));
    assert!(getcap(&system_path("/usr/bin/tree")).ends_with("cap_sys_nice=ep"));

    machine.store_writable(true);
    archix::deploy::xattr::remove(&tree(again), c"security.capability").unwrap();
    machine.store_writable(false);
    let output = setup.run(&["verify"]);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success()
            && text.contains(&format!(
                "deployment {again}: /usr/bin/tree lacks its capabilities cap_sys_nice=ep"
            )),
        "{text}"
    );
    setup.ok(&["verify", "--repair"]);
    assert!(getcap(&tree(again)).ends_with("cap_sys_nice=ep"));

    let deployments = machine.deployments();
    setup.set_body(
        &["tree"],
        r#"capabilities = { ["/usr/bin/archix-no-such-file"] = "cap_sys_nice+ep", ["/usr/bin/tree"] = "cap_sys_nice+ep" }"#,
    );
    let err = setup.fails(&["switch", "--noconfirm"]);
    assert!(
        err.contains("capabilities: /usr/bin/archix-no-such-file isn't in the new deployment; nothing was changed"),
        "{err}"
    );
    assert_eq!(machine.deployments(), deployments);
    assert_eq!(machine.current(), Some(again));
    setup.set_body(&["tree"], r#"capabilities = { ["/usr/bin/tree"] = "cap_sys_nice+e" }"#);
    let err = setup.fails(&["switch", "--noconfirm"]);
    assert!(err.contains("leaves the file without capabilities"), "{err}");
}

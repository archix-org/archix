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

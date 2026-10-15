//! The yay-style commands; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

mod common;

use std::fs;

use common::*;

fn installed_lua(setup: &Setup) -> Option<String> {
    fs::read_to_string(setup.dir.path().join("lua/hosts/box/installed.lua")).ok()
}

#[test]
#[ignore]
fn install_and_remove_edit_installed_lua_and_switch() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");

    let aborted = setup.answering_raw("n\n", &["install", "tree"]);
    assert!(!aborted.status.success());
    assert!(installed_lua(&setup).is_none(), "an aborted install leaves no trace");
    assert!(installed("tree").is_none());

    setup.ok(&["install", "tree", "--noconfirm"]);
    assert!(installed_lua(&setup).unwrap().contains("\"tree\""));
    assert!(installed("tree").is_some());
    assert!(setup.lock().declared.contains(&"tree".to_string()));

    let out = setup.fails(&["remove", "base"]);
    assert!(out.contains("declared elsewhere in the config"), "{out}");
    let out = setup.fails(&["install", "lemon", "--path", "/tmp"]);
    assert!(out.contains("must be inside"), "{out}");

    setup.ok(&["-R", "tree", "--noconfirm"]);
    assert!(!installed_lua(&setup).unwrap().contains("\"tree\""));
    assert!(installed("tree").is_none());
}

#[test]
#[ignore]
fn temporary_installs_last_until_the_next_switch() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    let out = setup.ok(&["install", "--temp", "tree", "--noconfirm"]);
    assert!(out.contains("without declaring it"), "{out}");
    assert!(installed("tree").is_some());
    assert!(installed_lua(&setup).is_none(), "--temp doesn't touch the config");
    assert!(setup.ok(&["diff"]).contains("remove     tree"));
    setup.ok(&["switch", "--noconfirm"]);
    assert!(installed("tree").is_none());
}

#[test]
#[ignore]
fn search_covers_the_repos_and_the_aur() {
    let _system = lock_system();
    let mut aur = LocalAur::new();
    aur.publish(TestPkg::new("archix-test-searchable", "1.0"));
    let setup = Setup::new(&[], &format!("aur = {{ url = {:?} }}", aur.url()));
    let out = setup.ok(&["search", "tree"]);
    assert!(out.lines().any(|l| l.starts_with("extra/tree ")), "{out}");
    let out = setup.ok(&["-Ss", "archix-test-searchable"]);
    assert!(out.contains("aur/archix-test-searchable 1.0-1"), "{out}");
}

#[test]
#[ignore]
fn syu_moves_aur_packages_to_their_newest_recipe() {
    let _system = lock_system();
    let mut aur = LocalAur::new();
    aur.publish(TestPkg::new("archix-test-upgradable", "1.0"));
    let body = format!("aur = {{ url = {:?} }}", aur.url());
    let setup = Setup::new(&["archix-test-upgradable"], &body);
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(installed("archix-test-upgradable").as_deref(), Some("1.0-1"));

    aur.publish(TestPkg::new("archix-test-upgradable", "1.1"));
    setup.ok(&["-Syu", "--noconfirm"]);
    assert_eq!(installed("archix-test-upgradable").as_deref(), Some("1.1-1"));
    assert_eq!(setup.lock().packages["archix-test-upgradable"].version, "1.1-1");

    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
}

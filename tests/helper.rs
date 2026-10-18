//! The yay-style commands; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

mod common;

use std::fs;
use std::os::unix::process::ExitStatusExt;

use archix::generations::Store;
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

fn current(setup: &Setup) -> Option<u32> {
    Store::new(&setup.paths).current().unwrap()
}

#[test]
#[ignore]
fn declining_a_switch_leaves_the_lock_byte_for_byte() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    let before = setup.read("archix.lock").unwrap();

    let declined = setup.answering_raw("n\n", &["install", "tree"]);
    assert!(!declined.status.success());
    let after = setup.read("archix.lock").unwrap();
    println!(
        "archix.lock after declining `archix install tree`:\n{}",
        line_diff(&before, &after)
    );
    assert!(
        after == before,
        "declining changed archix.lock:\n{}",
        line_diff(&before, &after)
    );
    assert!(installed_lua(&setup).is_none());
    assert!(installed("tree").is_none());

    let init = setup.read("init.lua").unwrap();
    let edited = init.replacen("packages = { ", "packages = { \"tree\", ", 1);
    fs::write(setup.dir.path().join("init.lua"), &edited).unwrap();
    let declined = setup.answering_raw("n\n", &["switch"]);
    assert!(!declined.status.success());
    let after = setup.read("archix.lock").unwrap();
    println!(
        "archix.lock after declining `archix switch` with tree added by hand:\n{}",
        line_diff(&before, &after)
    );
    assert!(
        after == before,
        "declining changed archix.lock:\n{}",
        line_diff(&before, &after)
    );
    assert_eq!(
        setup.read("init.lua").unwrap(),
        edited,
        "the hand edit to the config is the user's and stays"
    );
    fs::write(setup.dir.path().join("init.lua"), init).unwrap();
}

#[test]
#[ignore]
fn a_crash_after_the_lock_is_written_puts_the_config_and_lock_back() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    let before = setup.read("archix.lock").unwrap();
    let generation = current(&setup);

    let crashed = setup.run_env(
        &[("ARCHIX_TEST_CRASH", "lock-written")],
        &["install", "tree", "--noconfirm"],
    );
    assert_eq!(
        crashed.status.signal(),
        Some(9),
        "archix should die right after writing the lock"
    );
    let torn = setup.read("archix.lock").unwrap();
    println!("archix.lock right after the kill:\n{}", line_diff(&before, &torn));
    println!("installed.lua right after the kill: {:?}", installed_lua(&setup));

    let next = setup.ok(&["generations"]);
    println!("the next archix run:\n{next}");
    let after = setup.read("archix.lock").unwrap();
    println!("archix.lock after the next run:\n{}", line_diff(&before, &after));
    assert!(
        after == before,
        "the lock still differs:\n{}",
        line_diff(&before, &after)
    );
    assert!(
        installed_lua(&setup).is_none(),
        "installed.lua should be gone again: {:?}",
        installed_lua(&setup)
    );
    assert!(installed("tree").is_none());
    assert_eq!(current(&setup), generation);
}

#[test]
#[ignore]
fn a_switch_under_sudo_leaves_the_config_owned_by_its_owner() {
    use std::os::unix::fs::MetadataExt;
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    let chown = std::process::Command::new("chown")
        .args(["-R", "1000:1000"])
        .arg(setup.dir.path())
        .status()
        .unwrap();
    assert!(chown.success());
    setup.ok(&["install", "tree", "--noconfirm"]);
    for file in ["archix.lock", "lua/hosts/box/installed.lua", "lua/hosts/box", "lua/hosts", "lua"] {
        let meta = fs::metadata(setup.dir.path().join(file)).unwrap();
        println!(
            "{file}: uid {} gid {} mode {:o}",
            meta.uid(),
            meta.gid(),
            meta.mode() & 0o7777
        );
        assert_eq!(
            (meta.uid(), meta.gid()),
            (1000, 1000),
            "{file} should stay owned by the config's owner"
        );
    }
    setup.ok(&["remove", "tree", "--noconfirm"]);
}

#[test]
#[ignore]
fn installing_a_package_the_config_already_declares_adds_no_duplicate() {
    let _system = lock_system();
    let setup = Setup::new(&["tree"], "");
    let out = setup.ok(&["install", "tree", "--noconfirm"]);
    println!("{out}");
    println!("installed.lua: {:?}", installed_lua(&setup));
    assert!(
        installed_lua(&setup).is_none(),
        "nothing to add, so no installed.lua: {:?}",
        installed_lua(&setup)
    );
    assert!(
        installed_lua(&setup).is_none_or(|text| !text.contains("\"tree\"")),
        "tree is declared in init.lua, so installed.lua mustn't list it again: {:?}",
        installed_lua(&setup)
    );
    assert!(installed("tree").is_some());
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
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

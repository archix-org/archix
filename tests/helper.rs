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
    for file in [
        "archix.lock",
        "lua/hosts/box/installed.lua",
        "lua/hosts/box",
        "lua/hosts",
        "lua",
    ] {
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
        if meta.is_dir() {
            assert_eq!(
                meta.mode() & 0o7777,
                0o755,
                "{file} should be created 0755 whatever the umask"
            );
        }
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

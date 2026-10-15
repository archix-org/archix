//! Changes the installed packages; run only inside the throwaway Arch container via `tests/arch/run.sh`.

mod common;

use std::fs;

use archix::journal::{Journal, Kind, Transaction};
use archix::pacman::read_local_db;
use archix::paths::Paths;
use common::*;

/// Writes the journal a crash during "install tree" would leave behind, as if pacman had died mid-transaction.
fn fake_interrupted_switch(paths: &Paths) {
    let before = read_local_db(&paths.local_db()).unwrap();
    let tree = cached_file(&paths.packages(), "tree");
    let version = tree
        .file_name()
        .unwrap()
        .to_string_lossy()
        .trim_start_matches("tree-")
        .to_string();
    let version = version.trim_end_matches("-x86_64.pkg.tar.zst").to_string();
    let mut versions: std::collections::BTreeMap<String, String> = before
        .iter()
        .map(|(name, pkg)| (name.clone(), pkg.version.clone()))
        .collect();
    versions.insert("tree".into(), version);
    let mut reasons: std::collections::BTreeMap<String, bool> =
        before.iter().map(|(name, pkg)| (name.clone(), pkg.explicit)).collect();
    reasons.insert("tree".into(), true);
    let transaction = Transaction {
        system: None,
        boot: None,
        reinstall: false,
        restart: true,
        install: vec![tree],
        remove: vec![],
        versions,
        reasons,
    };
    Journal::new(Kind::Switch, "box", "snapshot 2026-10-10".into(), before, transaction)
        .save(&paths.journal())
        .unwrap();
}

/// The lock file a pacman killed mid-transaction leaves behind.
fn stale_pacman_lock(paths: &Paths) {
    fs::write(paths.pacman_lock(), "").unwrap();
}

#[test]
#[ignore]
fn switch_installs_then_removes_and_is_idempotent() {
    let _system = lock_system();
    let setup = Setup::new(&["tree"], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert!(installed("tree").is_some());
    assert!(!setup.paths.journal().exists(), "a finished switch leaves no journal");
    assert!(setup.ok(&["switch", "--noconfirm"]).contains("nothing to do"));
    assert!(setup.ok(&["status"]).contains("the system matches the lock"));

    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert!(
        installed("tree").is_none(),
        "undeclared packages are removed by default"
    );
}

#[test]
#[ignore]
fn warn_mode_keeps_undeclared_packages() {
    let _system = lock_system();
    let setup = Setup::new(&["tree"], "");
    setup.ok(&["switch", "--noconfirm"]);
    setup.set_body(&[], "undeclared_packages = \"warn\"");
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(
        out.contains("kept because undeclared_packages = \"warn\": tree"),
        "{out}"
    );
    assert!(installed("tree").is_some());

    setup.set_body(&[], "ignored_packages = { \"tree\" }");
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(
        out.contains("no package changes"),
        "a settings change is a new generation:\n{out}"
    );
    assert!(installed("tree").is_some());
    pacman(&["-R", "tree"]);
}

#[test]
#[ignore]
fn interrupted_switch_blocks_until_recovered() {
    let _system = lock_system();
    let setup = Setup::new(&["tree"], "");
    setup.ok(&["switch", "--noconfirm"]);
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    let paths = &setup.paths;

    fake_interrupted_switch(paths);
    stale_pacman_lock(paths);
    assert!(
        setup
            .fails(&["switch", "--noconfirm"])
            .contains("was interrupted while removing packages")
    );
    assert!(setup.fails(&["lock"]).contains("run `archix recover`"));
    assert!(setup.ok(&["status"]).contains("run `archix recover`"));
    setup.ok(&["recover", "--continue"]);
    assert!(installed("tree").is_some(), "continue finishes the install");
    assert!(!paths.journal().exists() && !paths.pacman_lock().exists());

    pacman(&["-R", "tree"]);
    fake_interrupted_switch(paths);
    pacman(&["-U", cached_file(&paths.packages(), "tree").to_str().unwrap()]);
    stale_pacman_lock(paths);
    setup.ok(&["recover", "--rollback"]);
    assert!(
        installed("tree").is_none(),
        "rollback removes what the interrupted switch added"
    );
    assert!(!paths.journal().exists());

    fake_interrupted_switch(paths);
    let out = setup.ok(&["recover", "--abandon"]);
    assert!(out.contains("tree") && out.contains("is not installed"), "{out}");
    assert!(installed("tree").is_none(), "abandon changes nothing");
    assert!(!paths.journal().exists());
}

/// Installs lemon 3.54.0 from a later snapshot with plain pacman, the way `pacman -Syu` or yay would.
fn upgrade_lemon_behind_archix() {
    let file = std::env::temp_dir().join("lemon-3.54.0-1-x86_64.pkg.tar.zst");
    if !file.exists() {
        let url = "https://archive.archlinux.org/repos/2026/10/14/core/os/x86_64/lemon-3.54.0-1-x86_64.pkg.tar.zst";
        assert!(
            archix::archive::Archive::new(archix::archive::ARCHIVE_URL)
                .fetch(url, &file)
                .unwrap()
        );
    }
    pacman(&["-U", file.to_str().unwrap()]);
}

#[test]
#[ignore]
fn manual_upgrades_are_recorded_and_adopted_as_pins() {
    let _system = lock_system();
    let setup = Setup::new(&["lemon"], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(installed("lemon").as_deref(), Some("3.53.4-1"));
    assert!(
        setup.paths.pacman_hooks.join("archix.hook").is_file(),
        "switch installs the pacman hook"
    );

    upgrade_lemon_behind_archix();
    assert!(setup.ok(&["status"]).contains("changed outside archix"));
    assert!(setup.ok(&["diff"]).contains("newer than the lock"));

    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(
        out.contains("the system is newer than the lock for 1 package (lemon)"),
        "{out}"
    );
    let lock = setup.lock();
    assert_eq!(
        lock.date.to_string(),
        IMAGE_SNAPSHOT,
        "the rest of the system still matches the old snapshot"
    );
    assert_eq!(lock.pinned, ["lemon"]);
    assert_eq!(lock.packages["lemon"].version, "3.54.0-1");
    assert_eq!(
        installed("lemon").as_deref(),
        Some("3.54.0-1"),
        "adopting never downgrades"
    );
    assert!(
        !setup.ok(&["status"]).contains("changed outside archix"),
        "a switch clears the record"
    );

    setup.ok(&["lock"]);
    assert_eq!(
        setup.lock().pinned,
        ["lemon"],
        "pins survive re-locking at the same date"
    );
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
}

#[test]
#[ignore]
fn declining_adoption_reverts_to_the_lock() {
    let _system = lock_system();
    let setup = Setup::new(&["lemon"], "");
    setup.ok(&["switch", "--noconfirm"]);
    upgrade_lemon_behind_archix();
    let out = setup.answering("n\ny\n", &["switch"]);
    assert!(out.contains("downgrade  lemon 3.54.0-1 -> 3.53.4-1"), "{out}");
    assert_eq!(installed("lemon").as_deref(), Some("3.53.4-1"));
    assert!(setup.lock().pinned.is_empty());
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
}

#[test]
#[ignore]
fn packages_installed_with_pacman_are_offered_for_declaring() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    let installed_lua = setup.dir.path().join("lua/hosts/box/installed.lua");

    upgrade_lemon_behind_archix();
    let out = setup.answering("n\ny\n", &["switch"]);
    assert!(
        out.contains("installed with pacman or an AUR helper but not declared: lemon"),
        "{out}"
    );
    assert!(installed("lemon").is_none(), "declining removes it");
    assert!(!installed_lua.exists());

    upgrade_lemon_behind_archix();
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(out.contains("not declared: lemon"), "{out}");
    assert_eq!(archix::installed::read(&installed_lua).unwrap(), ["lemon"]);
    assert_eq!(
        installed("lemon").as_deref(),
        Some("3.54.0-1"),
        "declaring keeps the installed version"
    );
    assert_eq!(setup.lock().pinned, ["lemon"]);

    fs::remove_file(&installed_lua).unwrap();
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert!(installed("lemon").is_none());
}

fn current_generation() -> Option<u32> {
    archix::generations::Store::new(&Paths::system()).current().unwrap()
}

#[test]
#[ignore]
fn every_switch_is_a_generation_and_rollback_returns_to_one() {
    let _system = lock_system();
    let setup = Setup::new(&["tree"], "");
    setup.ok(&["switch", "--noconfirm"]);
    let with_tree = current_generation().unwrap();
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    let without_tree = current_generation().unwrap();
    assert!(without_tree > with_tree);
    assert!(installed("tree").is_none());

    let listing = setup.ok(&["generations"]);
    assert!(listing.contains(&format!("* {without_tree:>4}")), "{listing}");
    assert!(
        setup
            .ok(&["status"])
            .contains(&format!("current generation: {without_tree}"))
    );

    setup.ok(&["rollback", "--noconfirm"]);
    assert_eq!(
        current_generation(),
        Some(with_tree),
        "rollback goes to the previous generation"
    );
    assert!(installed("tree").is_some());
    setup.ok(&["rollback", &without_tree.to_string(), "--noconfirm"]);
    assert_eq!(current_generation(), Some(without_tree));
    assert!(installed("tree").is_none());
    assert!(
        setup.ok(&["switch", "--noconfirm"]).contains("nothing to do"),
        "the system already matches the config"
    );
}

#[test]
#[ignore]
fn gc_keeps_only_what_the_limits_allow() {
    let _system = lock_system();
    let limits = "generations = { keep = 1, keep_days = 0 }";
    let setup = Setup::new(&["tree"], limits);
    setup.ok(&["switch", "--noconfirm"]);
    setup.set_body(&[], limits);
    setup.ok(&["switch", "--noconfirm"]);
    let store = archix::generations::Store::new(&setup.paths);
    let numbers: Vec<u32> = store.list().unwrap().iter().map(|g| g.number).collect();
    assert_eq!(
        numbers,
        [current_generation().unwrap()],
        "the switch's automatic gc keeps only the current one"
    );
    let tree_cached = fs::read_dir(setup.paths.packages())
        .unwrap()
        .flatten()
        .any(|entry| entry.file_name().to_string_lossy().starts_with("tree-"));
    assert!(
        !tree_cached,
        "no kept generation uses tree, so its cached package is gone"
    );
    assert!(setup.ok(&["gc"]).contains("freed 0.0 MiB"));
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
}

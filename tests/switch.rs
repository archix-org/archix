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
        config: Vec::new(),
        system: None,
        boot: None,
        reinstall: false,
        at_boot: false,
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
fn a_package_pacman_would_reject_stops_the_switch_before_anything_is_removed() {
    let _system = lock_system();
    let setup = Setup::new(&["tree"], "");
    setup.ok(&["switch", "--noconfirm"]);
    setup.set_body(&["lemon"], "");
    setup.ok(&["switch", "--noconfirm"]);
    let sig = format!("{}.sig", cached_file(&setup.paths.packages(), "tree").display());
    fs::write(&sig, "not a signature").unwrap();
    setup.set_body(&["tree"], "");
    let err = setup.fails(&["switch", "--noconfirm"]);
    assert!(err.contains("nothing was changed"), "{err}");
    assert!(
        installed("lemon").is_some(),
        "the Remove step must not run when pacman would refuse the install"
    );
    assert!(!setup.paths.journal().exists(), "a refused switch leaves no journal");
    fs::remove_file(&sig).unwrap();
    setup.ok(&["switch", "--noconfirm"]);
    assert!(installed("tree").is_some() && installed("lemon").is_none());
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
}

#[test]
#[ignore]
fn an_unowned_file_in_the_way_stops_the_switch_before_anything_changes() {
    let _system = lock_system();
    let setup = Setup::new(&["lemon"], "");
    setup.ok(&["switch", "--noconfirm"]);
    fs::write("/usr/bin/tree", "mine").unwrap();
    setup.set_body(&["tree"], "");
    let err = setup.fails(&["switch", "--noconfirm"]);
    assert!(
        err.contains("/usr/bin/tree from tree exists on disk") || err.contains("usr/bin/tree from tree exists on disk"),
        "{err}"
    );
    assert!(installed("lemon").is_some(), "nothing was removed");
    assert_eq!(
        fs::read_to_string("/usr/bin/tree").unwrap(),
        "mine",
        "the user's file is untouched"
    );
    fs::remove_file("/usr/bin/tree").unwrap();
    setup.ok(&["switch", "--noconfirm"]);
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
}

#[test]
#[ignore]
fn archive_url_points_locking_at_a_mirror() {
    let _system = lock_system();
    let setup = Setup::new(&["tree"], "");
    setup.ok(&["switch", "--noconfirm"]);
    setup.set_body(&["tree"], r#"archive_url = "http://127.0.0.1:9/""#);
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(
        archix::archive::configured(&setup.paths.state),
        "http://127.0.0.1:9",
        "switch records the archive for rollback and recover, which don't evaluate the config"
    );
    let err = setup.fails(&["update", "--to", "2026-09-09", "--noconfirm"]);
    assert!(err.contains("127.0.0.1:9/repos/2026/09/09/"), "{err}");
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(
        archix::archive::configured(&setup.paths.state),
        archix::archive::ARCHIVE_URL
    );
}

#[test]
#[ignore]
fn the_initramfs_repair_restores_what_a_cut_off_switch_was_changing() {
    let _system = lock_system();
    let setup = Setup::new(&["tree"], "");
    setup.ok(&["switch", "--noconfirm"]);
    let paths = &setup.paths;
    let before = read_local_db(&paths.local_db()).unwrap();
    let mut versions: std::collections::BTreeMap<String, String> = before
        .iter()
        .map(|(name, pkg)| (name.clone(), pkg.version.clone()))
        .collect();
    versions.remove("tree");
    let transaction = Transaction {
        config: Vec::new(),
        remove: vec!["tree".into()],
        versions,
        ..Transaction::default()
    };
    Journal::new(Kind::Switch, "box", "snapshot 2026-10-10".into(), before, transaction)
        .save(&paths.journal())
        .unwrap();
    fs::remove_file("/usr/bin/tree").unwrap();
    // As if a newer tree had been half-installed: its entry lists a file the restored version doesn't ship.
    let entry = fs::read_dir(paths.local_db())
        .unwrap()
        .flatten()
        .find(|e| archix::pacman::name_from_entry(&e.file_name().to_string_lossy()) == "tree")
        .unwrap()
        .path();
    let files = fs::read_to_string(entry.join("files")).unwrap();
    fs::write(
        entry.join("files"),
        files.replacen("%FILES%\n", "%FILES%\nusr/share/archix-test-dropped\n", 1),
    )
    .unwrap();
    fs::write("/usr/share/archix-test-dropped", "from the newer version").unwrap();

    let out = setup.ok(&["rescue", "--initrd"]);
    assert!(out.contains("restored tree"), "{out}");
    assert!(
        fs::exists("/usr/bin/tree").unwrap(),
        "the half-removed package is whole again"
    );
    assert!(
        !fs::exists("/usr/share/archix-test-dropped").unwrap(),
        "files only the replaced version had are removed, or pacman later trips over them"
    );
    assert!(
        !setup.ok(&["rescue", "--initrd"]).contains("restored"),
        "the repair runs once per interrupted switch, not on every boot"
    );
    Journal::delete(&paths.journal()).unwrap();
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
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
            .contains("was interrupted while installing packages")
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

    // A killed pacman leaves files of a package whose database entry it never wrote.
    fake_interrupted_switch(paths);
    fs::write("/usr/bin/tree", "half-written").unwrap();
    let out = setup.ok(&["recover", "--continue"]);
    assert!(out.contains("left behind by the interrupted pacman run"), "{out}");
    assert!(installed("tree").is_some(), "the leftover no longer blocks the install");
    pacman(&["-R", "tree"]);

    // A power cut mid-pacman leaves the half-written entry's desc empty, as a VM test saw.
    for rollback in [false, true] {
        fake_interrupted_switch(paths);
        pacman(&["-U", cached_file(&paths.packages(), "tree").to_str().unwrap()]);
        let entry = fs::read_dir(paths.local_db())
            .unwrap()
            .flatten()
            .find(|e| archix::pacman::name_from_entry(&e.file_name().to_string_lossy()) == "tree")
            .unwrap();
        fs::write(entry.path().join("desc"), "").unwrap();
        assert!(setup.fails(&["status"]).contains("archix recover"));
        let mode = if rollback { "--rollback" } else { "--continue" };
        let out = setup.ok(&["recover", mode]);
        assert!(out.contains("was torn by the crash; rewrote it"), "{out}");
        assert!(!paths.journal().exists());
        assert_eq!(installed("tree").is_some(), !rollback, "{mode} after a torn entry");
        assert_eq!(
            fs::exists("/usr/bin/tree").unwrap(),
            !rollback,
            "{mode} leaves no unowned files behind"
        );
        assert!(
            read_local_db(&paths.local_db()).is_ok(),
            "{mode} leaves pacman's database readable"
        );
        if !rollback {
            pacman(&["-R", "tree"]);
        }
    }
}

#[test]
#[ignore]
fn recover_rebuilds_a_keyring_a_crash_tore() {
    let _system = lock_system();
    let setup = Setup::new(&["tree"], "");
    setup.ok(&["switch", "--noconfirm"]);
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    let paths = &setup.paths;
    fake_interrupted_switch(paths);
    // What a power cut left in the VM test: the file's size, but zeros where the keys were.
    let pubring = "/etc/pacman.d/gnupg/pubring.gpg";
    let size = fs::metadata(pubring).unwrap().len();
    fs::write(pubring, vec![0u8; size as usize]).unwrap();
    let out = setup.ok(&["recover", "--continue"]);
    assert!(
        out.contains("pacman's keyring was torn by the crash; rebuilt it"),
        "{out}"
    );
    assert!(
        installed("tree").is_some(),
        "with the keyring rebuilt, pacman takes the signed package"
    );
    pacman(&["-R", "tree"]);
}

#[test]
#[ignore]
fn ignored_packages_that_arent_installed_are_named() {
    let _system = lock_system();
    let setup = Setup::new(&[], "ignored_packages = { \"archix-test-elsewhere\" }");
    let message = "ignored_packages lists 1 package that isn't installed here, and archix never installs ignored ones: archix-test-elsewhere";
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(out.contains(message), "{out}");
    let status = setup.ok(&["status"]);
    assert!(status.contains(message), "{status}");
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
}

#[test]
#[ignore]
fn a_journal_from_a_newer_archix_is_handed_to_the_archix_that_wrote_it() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    let paths = &setup.paths;
    let journal = Journal::new(
        Kind::Switch,
        "box",
        "snapshot 2026-10-10".into(),
        Default::default(),
        Transaction::default(),
    );
    let mut newer = serde_json::to_value(&journal).unwrap();
    newer["version"] = 99.into();
    fs::create_dir_all(&paths.state).unwrap();
    fs::write(paths.journal(), newer.to_string()).unwrap();
    let writer = paths.state.join("journal-archix");
    fs::write(&writer, "#!/bin/sh\necho \"handed over: $* ($ARCHIX_HANDED_OVER)\"\n").unwrap();
    fs::set_permissions(&writer, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

    let out = setup.ok(&["recover", "--continue"]);
    assert!(
        out.contains("handed over:") && out.contains("recover --continue (1)"),
        "{out}"
    );
    assert!(setup.ok(&["status"]).contains("status (1)"));

    fs::remove_file(&writer).unwrap();
    let out = setup.fails(&["recover", "--continue"]);
    assert!(out.contains("version 99"), "{out}");
    fs::remove_file(paths.journal()).unwrap();
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

#[test]
#[ignore]
fn gc_keeps_the_generation_that_booted_through_15_switches() {
    let _system = lock_system();
    let body = |n: u32| {
        format!(
            r#"generations = {{ keep = 3, keep_days = 0 }}, files = {{ ["/etc/archix-test-gc"] = {{ text = "{n}" }} }}"#
        )
    };
    let setup = Setup::new(&[], &body(0));
    setup.ok(&["switch", "--noconfirm"]);
    let booted = current_generation().unwrap();
    let cmdline = format!("root=/dev/vda2 rw archix.generation={booted}");
    let env = [("ARCHIX_TEST_CMDLINE", cmdline.as_str())];
    for n in 1..=15 {
        let init = setup.read("init.lua").unwrap();
        fs::write(setup.dir.path().join("init.lua"), init.replace(&body(n - 1), &body(n))).unwrap();
        setup.ok_env(&env, &["switch", "--noconfirm"]);
    }
    println!(
        "archix gc, booted into generation {booted}:\n{}",
        setup.ok_env(&env, &["gc"])
    );
    println!("archix generations:\n{}", setup.ok_env(&env, &["generations"]));
    let numbers: Vec<u32> = archix::generations::Store::new(&setup.paths)
        .list()
        .unwrap()
        .iter()
        .map(|g| g.number)
        .collect();
    assert!(
        numbers.contains(&booted),
        "the booted generation {booted} was collected: {numbers:?}"
    );
    assert_eq!(numbers.len(), 4, "the newest 3 plus the booted one: {numbers:?}");
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
}

#[test]
#[ignore]
fn unsafe_downgrades_ask_and_noconfirm_refuses_them() {
    let _system = lock_system();
    let body = r#"unsafe_downgrades = { lemon = { "3.54" } }"#;
    let setup = Setup::new(&["lemon"], body);
    setup.ok(&["switch", "--noconfirm"]);
    upgrade_lemon_behind_archix();

    let output = setup.answering_raw("n\n\n", &["switch"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success(), "{text}");
    assert!(
        text.contains("unsafe downgrades") && text.contains("lemon 3.54.0-1 -> 3.53.4-1 crosses 3.54"),
        "{text}"
    );
    assert!(text.contains("downgrade anyway? [y/N]"), "{text}");
    assert_eq!(
        installed("lemon").as_deref(),
        Some("3.54.0-1"),
        "the default answer is no"
    );

    setup.answering("n\ny\n", &["switch", "--allow-unsafe-downgrade"]);
    assert_eq!(installed("lemon").as_deref(), Some("3.53.4-1"));

    upgrade_lemon_behind_archix();
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(installed("lemon").as_deref(), Some("3.54.0-1"), "--noconfirm adopts");
    let err = setup.fails(&["rollback", "--noconfirm"]);
    assert!(err.contains("--allow-unsafe-downgrade"), "{err}");
    assert_eq!(installed("lemon").as_deref(), Some("3.54.0-1"));
    setup.ok(&["rollback", "--noconfirm", "--allow-unsafe-downgrade"]);
    assert_eq!(installed("lemon").as_deref(), Some("3.53.4-1"));

    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
}

#[test]
#[ignore]
fn bare_terms_search_and_install_the_picks_like_yay() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    let installed_lua = setup.dir.path().join("lua/hosts/box/installed.lua");

    let out = setup.answering("\n", &["tree"]);
    assert!(out.contains("==> Packages to install (eg: 1 2 3, 1-3 or ^4):"), "{out}");
    assert!(installed("tree").is_none(), "an empty answer installs nothing");

    let out = setup.answering("1\n", &["tree", "--noconfirm"]);
    let last_result = out.lines().rfind(|line| line.starts_with("1 ")).unwrap_or_default();
    assert!(
        last_result.starts_with("1 extra/tree "),
        "the best match is 1, printed last:\n{out}"
    );
    assert!(installed("tree").is_some());
    assert_eq!(
        archix::installed::read(&installed_lua).unwrap(),
        ["tree"],
        "picks are declared"
    );

    assert!(
        setup.ok(&["status"]).contains("current generation"),
        "subcommands still win over search terms"
    );
    fs::remove_file(&installed_lua).unwrap();
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
}

#[test]
#[ignore]
fn a_second_archix_waits_its_turn() {
    let _system = lock_system();
    let setup = Setup::new(&["tree"], "");
    let mut first = std::process::Command::new(env!("CARGO_BIN_EXE_archix"))
        .args([
            "--config",
            setup.dir.path().to_str().unwrap(),
            "--host",
            "box",
            "switch",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_secs(3));
    let err = setup.fails(&["switch", "--noconfirm"]);
    assert!(
        err.contains("another archix is already running") && err.contains("switch"),
        "{err}"
    );
    use std::io::Write;
    first.stdin.take().unwrap().write_all(b"n\n").unwrap();
    assert!(
        !first.wait_with_output().unwrap().status.success(),
        "the first one was declined"
    );
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
}

#[test]
#[ignore]
fn a_lock_made_for_a_newer_cpu_is_refused_not_rewritten() {
    let _system = lock_system();
    let setup = Setup::new(&["tree"], "");
    let path = setup.dir.path().join("archix.lock");
    let text = fs::read_to_string(&path).unwrap();
    let foreign = text.replace("x86_64.pkg.tar.zst", "x86_64_v9.pkg.tar.zst");
    assert_ne!(text, foreign);
    fs::write(&path, &foreign).unwrap();

    let err = setup.fails(&["switch", "--noconfirm"]);
    assert!(err.contains("newer CPU level") && err.contains("archix lock"), "{err}");
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        foreign,
        "switch doesn't rewrite the lock it applies"
    );

    setup.ok(&["lock"]);
    assert!(
        !fs::read_to_string(&path).unwrap().contains("x86_64_v9"),
        "archix lock re-resolves it for this CPU"
    );
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
}

#[test]
#[ignore]
fn a_full_mount_other_than_root_stops_the_switch() {
    let _system = lock_system();
    let recipe = "pkgname=archix-test-big\npkgver=1\npkgrel=1\narch=(any)\nlicense=(MIT)\n\
                  package() {\n  install -d \"$pkgdir/opt/archix-test-mount\"\n  head -c 4194304 /dev/zero > \"$pkgdir/opt/archix-test-mount/blob\"\n}\n";
    let setup = Setup::with_files(
        &["archix-test-big"],
        "",
        &[("pkgbuilds/archix-test-big/PKGBUILD", recipe)],
    );
    let mount = "/opt/archix-test-mount";
    fs::create_dir_all(mount).unwrap();
    let mounted = std::process::Command::new("mount")
        .args(["-t", "tmpfs", "-o", "size=1m", "tmpfs", mount])
        .status()
        .unwrap();
    assert!(mounted.success());
    let err = setup.fails(&["switch", "--noconfirm"]);
    std::process::Command::new("umount").arg(mount).status().unwrap();
    assert!(err.contains(mount) && err.contains("nothing was changed"), "{err}");
    assert!(installed("archix-test-big").is_none());
    setup.ok(&["switch", "--noconfirm"]);
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    fs::remove_dir(mount).ok();
}

#[test]
#[ignore]
fn an_old_lock_is_called_out() {
    let _system = lock_system();
    let setup = Setup::new(&["tree"], "lock_warn_after_days = 1");
    let status = setup.ok(&["status"]);
    assert!(
        status.contains("days old") && status.contains("archix update"),
        "{status}"
    );
    setup.set_body(&["tree"], "");
    assert!(
        !setup.ok(&["status"]).contains("days old"),
        "the default allows a month"
    );
}

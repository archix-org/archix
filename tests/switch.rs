//! Switches test machines in place; run only inside the throwaway Arch container via `tests/arch/run.sh`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::*;

#[test]
#[ignore]
fn a_package_pacman_would_reject_stops_the_switch_before_anything_changes() {
    let _system = lock_system();
    let setup = Setup::new(&["tree"], "");
    setup.first_switch();
    let machine = &setup.machine;
    setup.set_body(&["lemon"], "");
    setup.ok(&["switch", "--noconfirm"]);
    let sig = PathBuf::from(format!(
        "{}.sig",
        cached_file(&setup.paths.packages(), "tree").display()
    ));
    // The cache is shared with later tests in this container, so the real signature comes back even if this test panics.
    let restore = RestoreFile::new(&sig);
    let shared = Path::new("/var/cache/archix/pkg").join(sig.file_name().unwrap());
    let restore_shared = (shared != sig).then(|| RestoreFile::new(&shared));
    fs::write(&sig, "not a signature").unwrap();
    setup.set_body(&["tree"], "");
    let current = machine.current();
    let err = setup.fails(&["switch", "--noconfirm"]);
    drop((restore, restore_shared));
    assert!(err.contains("nothing was changed"), "{err}");
    assert_eq!(machine.current(), current, "no generation was recorded");
    assert!(installed("lemon").is_some() && installed("tree").is_none());
    setup.ok(&["switch", "--noconfirm"]);
    assert!(installed("tree").is_some() && installed("lemon").is_none());
}

#[test]
#[ignore]
fn an_unowned_file_in_the_way_stops_the_switch_before_anything_changes() {
    let _system = lock_system();
    let setup = Setup::new(&["lemon"], "");
    setup.first_switch();
    let stray = setup.machine.system_file("usr/bin/tree");
    fs::write(&stray, "mine").unwrap();
    setup.set_body(&["tree"], "");
    let current = setup.machine.current();
    let err = setup.fails(&["switch", "--noconfirm"]);
    assert!(err.contains("usr/bin/tree from tree exists on disk"), "{err}");
    assert_eq!(setup.machine.current(), current);
    assert!(installed("lemon").is_some(), "nothing was removed");
    assert_eq!(
        fs::read_to_string(&stray).unwrap(),
        "mine",
        "the user's file is untouched"
    );
    fs::remove_file(&stray).unwrap();
    setup.ok(&["switch", "--noconfirm"]);
    assert!(installed("tree").is_some());
}

#[test]
#[ignore]
fn archive_mirrors_stand_in_when_the_archive_is_down() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    setup.first_switch();
    let body = r#"archive_url = "http://127.0.0.1:9", archive_mirrors = { "https://archive.archlinux.org" }"#;
    setup.set_body(&[], body);
    setup.ok(&["switch", "--noconfirm"]);
    let mirrors = archix::archive::configured_mirrors(&setup.paths.state);
    // A snapshot no other test fetches, which only the mirror can serve.
    let updated = setup.run(&["update", "--to", "2026-09-08", "--noconfirm"]);
    let date = setup.lock().date.to_string();
    assert_eq!(
        mirrors,
        ["https://archive.archlinux.org"],
        "switch records the mirrors for rollback"
    );
    assert!(updated.status.success(), "{}", String::from_utf8_lossy(&updated.stderr));
    assert_eq!(date, "2026-09-08");
}

#[test]
#[ignore]
fn archive_url_points_locking_at_a_mirror() {
    let _system = lock_system();
    let setup = Setup::new(&["tree"], "");
    setup.first_switch();
    setup.set_body(&["tree"], r#"archive_url = "http://127.0.0.1:9/""#);
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(
        archix::archive::configured(&setup.paths.state),
        "http://127.0.0.1:9",
        "switch records the archive for rollback, which doesn't evaluate the config"
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
fn warn_mode_keeps_undeclared_packages() {
    let _system = lock_system();
    let setup = Setup::new(&["tree"], "");
    setup.first_switch();
    setup.set_body(&[], "undeclared_packages = \"warn\"");
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(
        out.contains("kept because undeclared_packages = \"warn\": tree"),
        "{out}"
    );
    assert!(installed("tree").is_some());
    let before = setup.machine.current().unwrap();
    setup.set_body(&[], "ignored_packages = { \"tree\" }");
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(
        setup.machine.current().unwrap() > before,
        "a settings change is a new generation:\n{out}"
    );
    assert!(installed("tree").is_some());
}

const STALL_HOOK: &str = "/etc/pacman.d/hooks/zz-archix-test-stall.hook";
/// Under /var/cache, which the machine binds from the container, so the test sees what pacman's hook touches in the machine.
const STALLED: &str = "/var/cache/archix/test-stalled";

/// Holds the next transaction that installs tree in a pacman hook for a few seconds, and marks when it starts.
fn stall_on_tree() {
    let _ = fs::remove_file(STALLED);
    let hook = system_path(STALL_HOOK);
    fs::create_dir_all(hook.parent().unwrap()).unwrap();
    fs::write(
        hook,
        format!("[Trigger]\nOperation = Install\nType = Package\nTarget = tree\n\n[Action]\nWhen = PostTransaction\nExec = /bin/sh -c 'touch {STALLED}; sleep 5'\n"),
    )
    .unwrap();
}

fn wait_for(what: &str, done: impl Fn() -> bool) {
    for _ in 0..600 {
        if done() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    panic!("timed out waiting for {what}");
}

#[test]
#[ignore]
fn a_switch_started_in_a_login_session_runs_outside_it() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    setup.first_switch();
    setup.set_body(&["tree"], "");
    stall_on_tree();
    // Named like logind's session scopes, which logind may kill when the session ends.
    let mut session = Command::new("systemd-run")
        .args(["--scope", "--quiet", "--unit=session-archixtest.scope", "--"])
        .arg(env!("CARGO_BIN_EXE_archix"))
        .arg("--config")
        .arg(setup.dir.path())
        .args(["--host", "box", "switch", "--noconfirm"])
        .envs(setup.machine.env())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    wait_for("pacman to install tree", || Path::new(STALLED).exists());
    let pid = String::from_utf8_lossy(&Command::new("pgrep").args(["-xo", "archix"]).output().unwrap().stdout)
        .trim()
        .to_string();
    let cgroup = fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap_or_default();
    assert!(session.wait().unwrap().success());
    let _ = fs::remove_file(STALLED);
    assert!(
        cgroup.contains("/archix-") && !cgroup.contains("session-archixtest"),
        "the switch should run in a scope of its own: {cgroup}"
    );
    assert!(installed("tree").is_some());
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
}

const LEMON_URL: &str =
    "https://archive.archlinux.org/repos/2026/10/14/core/os/x86_64/lemon-3.54.0-1-x86_64.pkg.tar.zst";

/// Installs lemon 3.54.0 from a later snapshot with plain pacman on the machine, the way `pacman -U` or yay would.
fn upgrade_lemon_behind_archix(machine: &Machine) {
    let file = std::env::temp_dir().join("lemon-3.54.0-1-x86_64.pkg.tar.zst");
    if !file.exists() {
        assert!(
            archix::archive::Archive::new(archix::archive::ARCHIVE_URL)
                .fetch(LEMON_URL, &file)
                .unwrap()
        );
    }
    machine.pacman(&["-U", file.to_str().unwrap()]);
}

#[test]
#[ignore]
fn manual_upgrades_are_recorded_and_adopted_as_pins() {
    let _system = lock_system();
    let setup = Setup::new(&["lemon"], "");
    setup.first_switch();
    assert_eq!(installed("lemon").as_deref(), Some("3.53.4-1"));
    assert!(
        system_path("/usr/share/libalpm/hooks/95-archix-post.hook").is_file(),
        "the machine has the pacman hook"
    );
    upgrade_lemon_behind_archix(&setup.machine);
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
}

/// A snapshot after the image's, in which lemon is 3.54.0-1 and curl-rustls needs librustls's next soname.
const MOVED_SNAPSHOT: &str = "2026-10-20";

#[test]
#[ignore]
fn a_hold_keeps_a_package_at_its_version_from_the_archive_while_updates_move_the_rest() {
    let _system = lock_system();
    let setup = Setup::new(&["lemon"], r#"holds = { lemon = "3.53.3" }"#);
    let lock = setup.lock();
    assert_eq!(
        lock.packages["lemon"].version, "3.53.3-1",
        "the newest pkgrel of 3.53.3"
    );
    assert_eq!(lock.packages["lemon"].repo, "archive");
    assert_eq!(lock.held["lemon"].hold, "3.53.3");
    assert_eq!(lock.held["lemon"].newest, "3.53.4-1");
    assert!(lock.pinned.is_empty());
    setup.first_switch();
    assert_eq!(installed("lemon").as_deref(), Some("3.53.3-1"));
    let status = setup.ok(&["status"]);
    assert!(
        status.contains("hold: lemon held at 3.53.3-1, behind the snapshot's 3.53.4-1"),
        "{status}"
    );
    assert!(setup.ok(&["diff"]).contains("hold: lemon held at 3.53.3-1"));

    let before = setup.lock();
    let out = setup.ok(&["update", "--to", MOVED_SNAPSHOT, "--noconfirm"]);
    let after = setup.lock();
    assert_eq!(after.date.to_string(), MOVED_SNAPSHOT, "{out}");
    assert_eq!(after.packages["lemon"], before.packages["lemon"], "{out}");
    assert_eq!(after.held["lemon"].newest, "3.54.0-1");
    assert!(
        after
            .packages
            .iter()
            .any(|(name, pkg)| before.packages.get(name).is_some_and(|old| old.version != pkg.version)),
        "the rest of the lock moved with the snapshot"
    );
    assert!(
        setup
            .ok(&["status"])
            .contains("hold: lemon held at 3.53.3-1, behind the snapshot's 3.54.0-1")
    );

    let lockfile = setup.read("archix.lock").unwrap();
    let init = setup.read("init.lua").unwrap();
    setup.write(
        "init.lua",
        &init.replace(r#"lemon = "3.53.3""#, r#"lemon = "3.53.3", tree = "2.2.1-1""#),
    );
    let err = setup.fails(&["update", "--to", MOVED_SNAPSHOT, "--noconfirm"]);
    assert!(
        err.contains("holds.tree: tree isn't among this config's packages or their dependencies"),
        "{err}"
    );
    setup.write(
        "init.lua",
        &init.replace(r#"lemon = "3.53.3""#, r#"lemon = "3.40.9-7""#),
    );
    let err = setup.fails(&["update", "--to", MOVED_SNAPSHOT, "--noconfirm"]);
    assert!(
        err.contains("holds.lemon: the Arch Linux Archive has no lemon 3.40.9-7; newest there: ")
            && err.contains("3.54.0-1"),
        "{err}"
    );
    assert_eq!(setup.read("archix.lock").unwrap(), lockfile, "nothing was changed");
}

#[test]
#[ignore]
fn a_held_library_the_moved_snapshot_needs_a_newer_soname_of_is_refused_naming_both() {
    let _system = lock_system();
    let setup = Setup::new(&["curl-rustls"], r#"holds = { librustls = "0.15.4-1" }"#);
    let lockfile = setup.read("archix.lock").unwrap();
    let err = setup.fails(&["update", "--to", MOVED_SNAPSHOT, "--noconfirm"]);
    assert!(
        err.contains(
            "curl-rustls 8.23.0-1 needs `librustls.so=0.15.5-64`, but holds keep librustls at 0.15.4-1 (the snapshot has 0.15.5-1)"
        ) && err.contains("nothing was changed"),
        "{err}"
    );
    assert_eq!(setup.read("archix.lock").unwrap(), lockfile);
}

#[test]
#[ignore]
fn declining_adoption_reverts_to_the_lock() {
    let _system = lock_system();
    let setup = Setup::new(&["lemon"], "");
    setup.first_switch();
    upgrade_lemon_behind_archix(&setup.machine);
    let out = setup.answering("n\ny\n", &["switch"]);
    assert!(out.contains("downgrade  lemon 3.54.0-1 -> 3.53.4-1"), "{out}");
    assert_eq!(installed("lemon").as_deref(), Some("3.53.4-1"));
    assert!(setup.lock().pinned.is_empty());
}

#[test]
#[ignore]
fn the_drift_policy_asks_adds_or_keeps_packages_installed_with_pacman_untracked() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    setup.first_switch();
    let checks = Checks::default();
    let installed_lua = setup.dir.path().join("lua/hosts/box/installed.lua");
    upgrade_lemon_behind_archix(&setup.machine);
    let out = setup.answering("n\ny\n", &["switch"]);
    checks.check(
        out.contains("installed with pacman or an AUR helper since the last switch, not declared: lemon"),
        format!("ask names the package: {out}"),
    );
    checks.eq(
        installed("lemon").as_deref(),
        Some("3.54.0-1"),
        "declining keeps it, untracked",
    );
    checks.check(!installed_lua.exists(), "and declares nothing");
    let out = setup.ok(&["switch", "--noconfirm"]);
    checks.check(
        !out.contains("not declared: lemon"),
        format!("an untracked package isn't asked about again: {out}"),
    );
    checks.eq(
        installed("lemon").as_deref(),
        Some("3.54.0-1"),
        "no later switch removes it",
    );

    setup.machine.pacman(&["-R", "lemon"]);
    setup.ok(&["switch", "--noconfirm"]);
    upgrade_lemon_behind_archix(&setup.machine);
    let out = setup.ok(&["switch", "--noconfirm"]);
    checks.check(
        out.contains("not declared: lemon"),
        format!("once removed it's drift again: {out}"),
    );
    checks.eq(
        archix::installed::read(&installed_lua).unwrap(),
        vec!["lemon".to_string()],
        "ask with --noconfirm adds it to installed.lua",
    );
    checks.eq(
        installed("lemon").as_deref(),
        Some("3.54.0-1"),
        "declaring keeps the installed version",
    );
    checks.eq(setup.lock().pinned, vec!["lemon".to_string()], "and pins it");

    fs::remove_file(&installed_lua).unwrap();
    setup.set_body(&[], "drift = \"keep\"");
    setup.ok(&["switch", "--noconfirm"]);
    upgrade_lemon_behind_archix(&setup.machine);
    let out = setup.answering("", &["switch"]);
    checks.check(
        out.contains("keeping them installed and untracked"),
        format!("keep doesn't ask: {out}"),
    );
    checks.check(!installed_lua.exists(), "keep declares nothing");
    checks.eq(installed("lemon").as_deref(), Some("3.54.0-1"), "keep keeps it");

    setup.machine.pacman(&["-R", "lemon"]);
    setup.set_body(&[], "drift = \"add\"");
    setup.ok(&["switch", "--noconfirm"]);
    upgrade_lemon_behind_archix(&setup.machine);
    fs::create_dir_all(installed_lua.parent().unwrap()).unwrap();
    let elsewhere = setup.dir.path().join("lua/hosts/box/elsewhere.lua");
    fs::write(&elsewhere, "return { packages = {} }\n").unwrap();
    std::os::unix::fs::symlink("elsewhere.lua", &installed_lua).unwrap();
    let refused = setup.answering_raw("", &["switch"]);
    let said = String::from_utf8_lossy(&refused.stderr);
    checks.check(
        !refused.status.success() && said.contains("is a symlink, which archix doesn't follow"),
        format!("add refuses an installed.lua that's a symlink: {said}"),
    );
    checks.eq(
        fs::read_to_string(&elsewhere).unwrap(),
        "return { packages = {} }\n".to_string(),
        "and writes nothing through it",
    );
    fs::remove_file(&installed_lua).unwrap();
    fs::remove_file(&elsewhere).unwrap();
    let out = setup.answering("y\ny\n", &["switch"]);
    checks.check(
        out.contains("adding them to installed.lua, as drift = \"add\" says")
            && !out.contains("add them to installed.lua?"),
        format!("add doesn't ask: {out}"),
    );
    checks.eq(
        archix::installed::read(&installed_lua).unwrap(),
        vec!["lemon".to_string()],
        "add declares it",
    );
    checks.finish();
    setup.machine.pacman(&["-R", "lemon"]);
}

#[test]
#[ignore]
fn a_drifted_package_only_an_aur_package_of_another_name_provides_is_declared_once() {
    let _system = lock_system();
    let mut aur = LocalAur::new();
    aur.publish(TestPkg {
        provides: &["archix-test-drifted"],
        ..TestPkg::new("archix-test-provider", "1.0")
    });
    let setup = Setup::new(&[], &format!("aur = {{ url = {:?} }}", aur.url()));
    setup.first_switch();
    let packages = Packages::new();
    let drifted = packages.build("archix-test-drifted", "");
    setup.machine.pacman(&["-U", drifted.to_str().unwrap()]);
    let out = setup.run(&["switch", "--noconfirm"]);
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "the switch adding it applies: {said}");
    assert_eq!(
        said.matches("adding them to installed.lua").count(),
        1,
        "it's added once, though the lock names its provider: {said}"
    );
    assert_eq!(
        archix::installed::read(&setup.dir.path().join("lua/hosts/box/installed.lua")).unwrap(),
        vec!["archix-test-drifted".to_string()]
    );
    assert!(setup.lock().packages.contains_key("archix-test-provider"));
    let again = setup.ok(&["switch", "--noconfirm"]);
    assert!(
        !again.contains("not declared"),
        "and isn't drift at the next switch: {again}"
    );
}

#[test]
#[ignore]
fn a_switch_with_rollbacks_on_refuses_a_root_on_the_btrfs_top_level_until_its_moved_into_at() {
    let _system = lock_system();
    let setup = Setup::on(Machine::on_btrfs_top("3G"), &[], "rollback = { enable = true }", &[]);
    let out = setup.run(&["switch", "--noconfirm"]);
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && said.contains("top level") && said.contains("--move-root"),
        "the switch refuses, naming why and the way out: {said}"
    );
    assert!(
        archix::generations::Store::new(&setup.paths)
            .current()
            .unwrap()
            .is_none(),
        "and applies nothing"
    );
    setup.set_body(&[], "rollback = { enable = false }");
    setup.ok(&["switch", "--noconfirm"]);
}

#[test]
#[ignore]
fn unsafe_downgrades_ask_and_noconfirm_refuses_them() {
    let _system = lock_system();
    let body = r#"unsafe_downgrades = { lemon = { "3.54" } }"#;
    let setup = Setup::new(&["lemon"], body);
    setup.first_switch();
    upgrade_lemon_behind_archix(&setup.machine);

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

    upgrade_lemon_behind_archix(&setup.machine);
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(installed("lemon").as_deref(), Some("3.54.0-1"), "--noconfirm adopts");
    setup.ok(&["gc"]);
    let err = setup.fails(&["revert", "--noconfirm"]);
    assert!(err.contains("--allow-unsafe-downgrade"), "{err}");
    assert_eq!(installed("lemon").as_deref(), Some("3.54.0-1"));
    setup.ok(&["revert", "--noconfirm", "--allow-unsafe-downgrade"]);
    assert_eq!(
        installed("lemon").as_deref(),
        Some("3.53.4-1"),
        "the generation rolled back to has its own lemon"
    );
}

#[test]
#[ignore]
fn bare_terms_search_and_install_the_picks_like_yay() {
    let _system = lock_system();
    let mut aur = LocalAur::new();
    aur.publish(TestPkg::new("tree-git", "2.3.2"));
    let setup = Setup::new(&[], &format!("aur = {{ url = {:?} }}", aur.url()));
    setup.first_switch();
    let installed_lua = setup.dir.path().join("lua/hosts/box/installed.lua");
    let out = setup.answering("\n", &["tree"]);
    assert!(
        out.contains("==> Install which? Numbers (1 3), ranges (2-5), ^ to leave one out:"),
        "{out}"
    );
    assert!(installed("tree").is_none(), "an empty answer installs nothing");
    let out = setup.answering("1\n", &["tree", "--noconfirm"]);
    let last_result = out.lines().rfind(|line| line.starts_with("1 ")).unwrap_or_default();
    assert!(
        last_result.starts_with("1 extra/tree "),
        "the best match is 1, printed last:\n{out}"
    );
    assert!(
        out.contains(" aur/tree-git "),
        "the AUR's matches are listed too:\n{out}"
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
        .envs(setup.machine.env())
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
    assert!(setup.machine.current().is_none(), "no generation was recorded");
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
}

#[test]
#[ignore]
fn downloads_the_package_store_cant_hold_are_refused_before_any_starts() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    setup.first_switch();
    setup.set_body(&["tree"], "");
    for cached in fs::read_dir("/var/cache/pacman/pkg").unwrap().flatten() {
        if cached.file_name().to_string_lossy().starts_with("tree-") {
            fs::remove_file(cached.path()).unwrap();
        }
    }
    let store = setup.paths.packages();
    for cached in fs::read_dir(&store).into_iter().flatten().flatten() {
        if cached.file_name().to_string_lossy().starts_with("tree-") {
            fs::remove_file(cached.path()).unwrap();
        }
    }
    let mounted = std::process::Command::new("mount")
        .args(["-t", "tmpfs", "-o", "size=16k", "tmpfs"])
        .arg(&store)
        .status()
        .unwrap();
    assert!(mounted.success());
    let current = setup.machine.current();
    let err = setup.fails(&["switch", "--noconfirm"]);
    std::process::Command::new("umount").arg(&store).status().unwrap();
    assert!(
        err.contains(&*store.to_string_lossy())
            && err.contains("nothing was changed")
            && !err.contains("downloading tree"),
        "{err}"
    );
    assert_eq!(setup.machine.current(), current);
    assert!(installed("tree").is_none());
}

#[test]
#[ignore]
fn a_switch_copies_its_packages_to_package_cache_push() {
    let _system = lock_system();
    let dir = std::path::Path::new("/var/tmp/archix-push-test");
    let _ = fs::remove_dir_all(dir);
    let body = format!(r#"package_cache_push = "file://{}""#, dir.display());
    let setup = Setup::new(&["tree"], &body);
    let out = setup.ok(&["switch", "--noconfirm"]);
    let pushed = dir.join(&setup.lock().packages["tree"].file).is_file();
    fs::remove_dir_all(dir).ok();
    assert!(pushed, "{out}");
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

#[test]
#[ignore]
fn recover_is_gone_and_rescue_runs_only_from_the_rescue_boot() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    let err = setup.fails(&["recover", "--continue"]);
    assert!(
        err.contains("archix recover` is gone") && err.contains("says what it applied"),
        "{err}"
    );
    let err = setup.fails(&["rescue", "check"]);
    assert!(
        err.contains("runs in the rescue boot") && err.contains("archix-rescue.conf"),
        "{err}"
    );
    let err = setup.fails(&["rescue", "--root-dir", "/", "check"]);
    assert!(err.contains("the running system"), "{err}");
}

#[test]
#[ignore]
fn a_package_signed_by_a_disabled_key_stops_the_transaction_and_no_generation_is_recorded() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    setup.first_switch();
    let machine = &setup.machine;
    // As pacman-key --populate does to revoked packager keys, in the machine's keyring only.
    let gnupg = machine.root.join("etc/pacman.d/gnupg");
    let script = format!(
        "h={}; for k in $(gpg --homedir \"$h\" --batch --with-colons --list-keys | awk -F: '$1 == \"pub\" {{ getline; print $10 }}'); do \
         printf 'disable\\nsave\\n' | gpg --homedir \"$h\" --batch --command-fd 0 --edit-key \"$k\" > /dev/null 2>&1 || true; done; \
         gpgconf --homedir \"$h\" --kill all",
        gnupg.display()
    );
    let disabled = Command::new("bash").args(["-c", &script]).status().unwrap();
    assert!(disabled.success());
    let current = machine.current();
    setup.set_body(&["tree"], "");
    let err = setup.fails(&["switch", "--noconfirm"]);
    assert!(
        err.contains("the package transaction failed before anything was changed"),
        "the machine's pacman took tree: {err}"
    );
    assert_eq!(machine.current(), current, "{err}");
    assert!(installed("tree").is_none());
}

/// The paths `archix diff` or `status` lists under the carried section titled `title`.
fn carried_section(out: &str, title: &str) -> Vec<String> {
    out.lines()
        .skip_while(|line| *line != format!("  {title}:"))
        .skip(1)
        .take_while(|line| line.starts_with("    "))
        .map(|line| line.trim().to_string())
        .collect()
}

#[test]
#[ignore]
fn what_the_machine_carries_but_the_config_doesnt_declare_is_named_before_and_after_switches_and_a_revert() {
    let _system = lock_system();
    let declared = r#"files = { ["/etc/xattr.conf"] = { text = "declared\n" } }"#;
    let setup = Setup::new(&[], declared);
    let machine = &setup.machine;
    let checks = Checks::default();
    for (path, text) in [
        ("opt/foo/bin/foo", "#!/bin/sh\n"),
        ("opt/foo/share/data", "data\n"),
        ("etc/wireguard/wg0.conf", "[Interface]\n"),
    ] {
        let path = machine.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let edited = machine.root.join("etc/e2scrub.conf");
    let shipped = fs::read_to_string(&edited).unwrap();
    fs::write(&edited, format!("{shipped}# edited by hand\n")).unwrap();
    fs::write(machine.root.join("etc/xattr.conf"), "by hand\n").unwrap();
    let check = |out: &str, when: &str| {
        let carried = out
            .split_once(
                "carried on this machine but not in the config (a machine built from this config won't have them):",
            )
            .map(|(_, carried)| carried);
        checks.check(carried.is_some(), format!("{when}: no carried section:\n{out}"));
        checks.check(
            carried_section(out, "no package's, in /usr and /opt").contains(&"/opt/foo/ (2 files)".to_string()),
            format!("{when}: /opt/foo isn't named:\n{out}"),
        );
        checks.check(
            carried_section(out, "edited from what their package shipped, in /etc")
                .contains(&"/etc/e2scrub.conf".to_string()),
            format!("{when}: the edited /etc/e2scrub.conf isn't named:\n{out}"),
        );
        checks.check(
            carried_section(out, "secrets, which no config carries").contains(&"/etc/wireguard/ (1 file)".to_string()),
            format!("{when}: the secret /etc/wireguard isn't named:\n{out}"),
        );
        checks.check(
            !carried.unwrap_or_default().contains("/etc/xattr.conf"),
            format!("{when}: the declared /etc/xattr.conf is named:\n{out}"),
        );
    };
    check(&setup.ok(&["diff", "--all"]), "before the first switch");

    setup.first_switch();
    setup.set_body(&["tree"], declared);
    setup.ok(&["switch", "--noconfirm"]);
    let second = machine.current().unwrap();
    checks.check(
        machine.root.join("opt/foo/bin/foo").is_file(),
        "/opt/foo was dropped by the switches",
    );
    check(&setup.ok(&["diff", "--all"]), "after two switches");
    check(&setup.ok(&["status", "--all"]), "status after two switches");
    let cut = setup.ok(&["diff"]);
    checks.check(
        cut.lines().count() <= setup.ok(&["diff", "--all"]).lines().count(),
        format!("plain diff lists more than --all:\n{cut}"),
    );

    setup.ok(&["revert", "--noconfirm"]);
    checks.check(machine.current() < Some(second), "the rollback went back");
    checks.check(
        machine.root.join("opt/foo/bin/foo").is_file(),
        "/opt/foo was dropped by the rollback",
    );
    check(&setup.ok(&["diff", "--all"]), "after the rollback");
    checks.finish();
}

fn said(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
#[ignore]
fn the_prompt_shows_the_plan_beside_the_source_diff_and_never_a_password_hash() {
    let _system = lock_system();
    const OLD: &str =
        "$6$oldsaltoldsalt$XhmZ3VZFIhQfYh4lKgoNS5PvBOwS2o0B1E/SRuTM8GQpyC6sIh2rvkkgAuFSYi5RqBFAvQ2pnRfvqCNFh9UNt.";
    const NEW: &str =
        "$6$newsaltnewsalt$XhmZ3VZFIhQfYh4lKgoNS5PvBOwS2o0B1E/SRuTM8GQpyC6sIh2rvkkgAuFSYi5RqBFAvQ2pnRfvqCNFh9UNt.";
    let setup = Setup::new(
        &[],
        &format!(
            r#"users = {{ gone = {{}}, kept = {{ hashed_password = "{OLD}" }} }}, files = {{ ["/etc/archix-plan-old"] = {{ text = "a" }}, ["/etc/systemd/system/archix-plan-old.service"] = {{ text = "[Unit]\n[Service]\nExecStart=/bin/true\n[Install]\nWantedBy=multi-user.target\n" }} }}, services = {{ ["archix-plan-old.service"] = true }}"#
        ),
    );
    setup.first_switch();
    setup.ok(&["switch", "--noconfirm"]);
    setup.set_body(
        &["tree"],
        &format!(
            r#"users = {{ fresh = {{ groups = {{ "wheel" }} }}, kept = {{ hashed_password = "{NEW}" }} }}, files = {{ ["/etc/archix-plan-new"] = {{ text = "b" }}, ["/etc/systemd/system/archix-plan-new.service"] = {{ text = "[Unit]\n[Service]\nExecStart=/bin/true\n[Install]\nWantedBy=multi-user.target\n" }} }}, services = {{ ["archix-plan-new.service"] = true }}"#
        ),
    );
    let output = setup.answering_raw("n\n", &["switch"]);
    let text = said(&output);
    for line in [
        "package + tree ",
        "file + /etc/archix-plan-new (0644 root:root)",
        "file - /etc/archix-plan-old",
        "unit + archix-plan-new.service enabled",
        "unit - archix-plan-old.service",
        "user + fresh (groups wheel",
        "user - gone",
        "user ~ kept: password",
    ] {
        assert!(text.contains(line), "the plan is missing {line:?}:\n{text}");
    }
    assert!(text.contains(":: the config since the last generation"), "{text}");
    assert!(
        !text.contains(OLD) && !text.contains(NEW) && !text.contains("oldsalt"),
        "{text}"
    );
}

#[test]
#[ignore]
fn a_package_that_cant_be_fetched_refuses_the_switch_before_anything_changes_and_names_it() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    setup.first_switch();
    setup.ok(&["switch", "--noconfirm"]);
    setup.set_body(&["tree"], r#"files = { ["/etc/archix-unfetched"] = { text = "x" } }"#);
    setup.ok(&["lock"]);
    for dir in [setup.paths.pacman_cache.clone(), setup.paths.packages()] {
        for cached in fs::read_dir(&dir).into_iter().flatten().flatten() {
            if cached.file_name().to_string_lossy().starts_with("tree-") {
                fs::remove_file(cached.path()).unwrap();
            }
        }
    }
    let generation = setup.machine.current();
    let output = setup.run_env(
        &[("ARCHIX_TEST_ARCHIVE", "http://127.0.0.1:9")],
        &["switch", "--noconfirm"],
    );
    let text = said(&output);
    assert!(!output.status.success(), "{text}");
    assert!(text.contains("tree") && text.contains("nothing was changed"), "{text}");
    assert_eq!(installed("tree"), None);
    assert_eq!(setup.machine.current(), generation);
    assert!(!setup.machine.system_file("etc/archix-unfetched").exists());
}

#[test]
#[ignore]
fn a_revert_to_the_last_generation_works_offline_from_archixs_own_cache_shown_in_status() {
    let _system = lock_system();
    let setup = Setup::new(&["tree"], "");
    setup.first_switch();
    setup.ok(&["switch", "--noconfirm"]);
    let file = setup.lock().packages["tree"].file.clone();
    assert!(setup.paths.packages().join(&file).is_file());
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(installed("tree"), None);
    assert!(
        setup.paths.packages().join(&file).is_file(),
        "the last 2 generations keep every package"
    );
    assert!(setup.ok(&["status"]).contains("package cache:"));
    for cached in fs::read_dir(&setup.paths.pacman_cache).unwrap().flatten() {
        if cached.file_name().to_string_lossy().starts_with("tree-") {
            fs::remove_file(cached.path()).unwrap();
        }
    }
    setup.set_body(&["tree"], "");
    setup.ok_env(
        &[("ARCHIX_TEST_ARCHIVE", "http://127.0.0.1:9")],
        &["switch", "--noconfirm"],
    );
    assert!(installed("tree").is_some());
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    setup.set_body(&[], r#"files = { ["/etc/archix-newer"] = { text = "x" } }"#);
    setup.ok(&["switch", "--noconfirm"]);
    assert!(
        !setup.paths.packages().join(&file).exists(),
        "an official package two generations back is the archive's to serve"
    );
}

/// A package pacman's database says is installed, with no package file behind it, as an AUR helper's leftover looks to archix.
fn fake_installed(machine: &Machine, name: &str, version: &str, depends: &[&str], files: &[&str]) {
    let dir = machine.local_db().join(format!("{name}-{version}"));
    fs::create_dir_all(&dir).unwrap();
    let mut desc = format!("%NAME%\n{name}\n\n%VERSION%\n{version}\n\n%ARCH%\nx86_64\n\n%REASON%\n0\n\n");
    if !depends.is_empty() {
        desc += &format!("%DEPENDS%\n{}\n\n", depends.join("\n"));
    }
    fs::write(dir.join("desc"), desc).unwrap();
    fs::write(dir.join("files"), format!("%FILES%\n{}\n", files.join("\n"))).unwrap();
}

const WARN_UNDECLARED: &str = "undeclared_packages = \"warn\"";

#[test]
#[ignore]
fn a_revert_that_breaks_a_dependency_of_an_untracked_package_refuses_unless_allow_partial() {
    let _system = lock_system();
    let setup = Setup::new(&["lemon"], WARN_UNDECLARED);
    setup.first_switch();
    upgrade_lemon_behind_archix(&setup.machine);
    fake_installed(&setup.machine, "archix-test-app", "1-1", &["lemon>=3.54"], &[]);
    let current = setup.machine.current();
    let output = setup.answering_raw("n\ny\n", &["switch"]);
    let text = said(&output);
    assert!(!output.status.success(), "{text}");
    assert!(
        text.contains("archix-test-app needs lemon>=3.54, but lemon would be 3.53.4-1")
            && text.contains("--allow-partial")
            && text.contains("nothing was changed"),
        "{text}"
    );
    assert_eq!(installed("lemon").as_deref(), Some("3.54.0-1"), "nothing moved");
    assert_eq!(setup.machine.current(), current);
    setup.answering("n\ny\n", &["switch", "--allow-partial"]);
    assert_eq!(installed("lemon").as_deref(), Some("3.53.4-1"));
    assert!(
        setup.machine.local_db().join("archix-test-app-1-1").is_dir(),
        "left as it was"
    );
}

#[test]
#[ignore]
fn a_revert_that_takes_a_library_an_untracked_package_links_refuses_unless_allow_partial() {
    let _system = lock_system();
    let setup = Setup::new(&["lemon"], WARN_UNDECLARED);
    setup.first_switch();
    upgrade_lemon_behind_archix(&setup.machine);
    let db = setup.machine.local_db();
    let lemon = db.join("lemon-3.54.0-1/files");
    let files = fs::read_to_string(&lemon).unwrap();
    fs::write(
        &lemon,
        files.replacen("%FILES%\n", "%FILES%\nusr/lib/libarchixgone.so.1\n", 1),
    )
    .unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let dir = scratch.path().display();
    let built = Command::new("bash")
        .arg("-ec")
        .arg(format!(
            "cd {dir}; echo 'int gone(void) {{ return 0; }}' > lib.c; gcc -shared -Wl,-soname,libarchixgone.so.1 -o libarchixgone.so.1 lib.c; \
             echo 'int gone(void); int main(void) {{ return gone(); }}' > app.c; gcc -o app app.c ./libarchixgone.so.1"
        ))
        .status()
        .unwrap();
    assert!(built.success());
    let app = setup.machine.system_file("usr/bin/archix-test-linked");
    fs::copy(scratch.path().join("app"), &app).unwrap();
    fake_installed(
        &setup.machine,
        "archix-test-linked",
        "1-1",
        &[],
        &["usr/bin/archix-test-linked"],
    );
    let output = setup.answering_raw("n\ny\n", &["switch"]);
    let text = said(&output);
    assert!(!output.status.success(), "{text}");
    assert!(
        text.contains("archix-test-linked (links libarchixgone.so.1, which lemon stops shipping)"),
        "{text}"
    );
    assert_eq!(installed("lemon").as_deref(), Some("3.54.0-1"), "nothing moved");
    setup.answering("n\ny\n", &["switch", "--allow-partial"]);
    assert_eq!(installed("lemon").as_deref(), Some("3.53.4-1"));
}

const LIVE_FILES: &str = r#"files = { ["/etc/archix-live.conf"] = { text = "live" }, ["/usr/local/share/archix-live"] = { text = "usr" } }, users = { archixlive = { groups = {} } }"#;

#[test]
#[ignore]
fn a_switch_applies_packages_files_and_users_on_the_root_with_no_reboot_and_records_plain_generations() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    setup.first_switch();
    assert_eq!(setup.machine.current(), Some(1));
    setup.set_body(&["tree"], LIVE_FILES);
    let out = setup.ok(&["switch", "--noconfirm"]);
    let checks = Checks::default();
    checks.check(
        installed("tree").is_some(),
        format_args!("tree is installed on the root: {out}"),
    );
    checks.eq(
        fs::read_to_string(setup.machine.system_file("etc/archix-live.conf")).ok(),
        Some("live\n".to_string()),
        "the /etc file",
    );
    checks.eq(
        fs::read_to_string(setup.machine.system_file("usr/local/share/archix-live")).ok(),
        Some("usr\n".to_string()),
        "the /usr file",
    );
    let passwd = fs::read_to_string(setup.machine.system_file("etc/passwd")).unwrap();
    checks.check(passwd.contains("\narchixlive:"), "the user is in the root's passwd");
    checks.check(
        setup.machine.system_file("home/archixlive").is_dir(),
        "the user's home is made",
    );
    checks.eq(setup.machine.current(), Some(2), "generation 2 is current");
    checks.check(
        !out.contains("reboot") && !out.contains("deployment") && !out.contains("holds the system"),
        format_args!("no reboot, no deployment and, off btrfs, no snapshot: {out}"),
    );
    setup.set_body(&[], "");
    let out = setup.ok(&["switch", "--noconfirm"]);
    checks.eq(installed("tree"), None, "removed again");
    let passwd = fs::read_to_string(setup.machine.system_file("etc/passwd")).unwrap();
    checks.check(!passwd.contains("\narchixlive:"), format_args!("the user goes: {out}"));
    let listed = setup.ok(&["generations"]);
    checks.check(
        listed.lines().any(|line| line.starts_with("*    3")) && listed.contains("   1  ") && listed.contains("   2  "),
        format_args!("three plain records, 3 current: {listed}"),
    );
    checks.check(
        !listed.contains("snapshots"),
        format_args!("no snapshot is listed on a root that isn't btrfs: {listed}"),
    );
    checks.finish();
}

fn snapshots(machine: &Machine) -> Vec<archix::snapshot::Record> {
    archix::snapshot::Store::open(&machine.root).list().unwrap()
}

fn applied_tree(number: u32) -> String {
    let paths = archix::paths::Paths {
        state: PathBuf::from("/var/lib/archix"),
        ..archix::paths::Paths::under(Path::new("/"))
    };
    archix::generations::Store::new(&paths)
        .get(number)
        .unwrap()
        .applied
        .expect("generation recorded in root's repository")
        .tree
}

const ROLLBACKS: &str = "rollback = { enable = true, keep_switch = 0, entries = 1 }";

#[test]
#[ignore]
fn on_btrfs_each_switch_snapshots_the_state_before_it_prunes_by_the_policy_and_generations_list_snapshots_by_state() {
    let _system = lock_system();
    let machine = Machine::on_btrfs();
    machine.btrfs.as_ref().unwrap().mount_archix();
    let setup = Setup::on(machine, &[], "", &[]);
    let out = setup.first_switch();
    let checks = Checks::default();
    checks.check(
        snapshots(&setup.machine).is_empty() && !out.contains("holds the system"),
        format_args!("rollbacks off: no snapshot and no word of one: {out}"),
    );
    let file = |text: &str| format!(r#"{ROLLBACKS}, files = {{ ["/etc/archix-snap"] = {{ text = "{text}" }} }}"#);
    setup.set_body(&[], &file("one"));
    setup.ok(&["switch", "--noconfirm"]);
    let taken = snapshots(&setup.machine);
    checks.eq(taken.len(), 1, "one snapshot");
    let store = archix::snapshot::Store::open(&setup.machine.root);
    let first = &taken[0];
    checks.eq(first.generation, Some(1), "it holds generation 1, the state before");
    checks.eq(first.tree.clone(), Some(applied_tree(1)), "and generation 1's tree");
    checks.eq(
        first.label.clone(),
        format!("before switch to tree {}", &applied_tree(2)[..12]),
        "labelled with the tree it came before",
    );
    checks.check(
        !store.path(first.number).join("etc/archix-snap").exists(),
        "the file this switch wrote isn't in the snapshot taken before it",
    );
    checks.check(
        setup.machine.system_file("etc/archix-snap").is_file(),
        "but is on the root",
    );
    setup.set_body(&[], &file("two"));
    setup.ok(&["switch", "--noconfirm"]);
    setup.set_body(&[], &file("three"));
    setup.ok(&["switch", "--noconfirm"]);
    let taken = snapshots(&setup.machine);
    checks.eq(
        taken.iter().map(|r| r.generation).collect::<Vec<_>>(),
        vec![Some(1), Some(3)],
        "keep_switch = 0 pruned all but the fallback slot (the first snapshot, as no boot counted as good since) and the newest, which has the one entry",
    );
    checks.eq(
        fs::read_to_string(store.path(taken[1].number).join("etc/archix-snap")).ok(),
        Some("two\n".to_string()),
        "the newest holds what generation 3 wrote",
    );
    setup.ok(&["revert", "3", "--noconfirm"]);
    checks.eq(
        fs::read_to_string(setup.machine.system_file("etc/archix-snap")).ok(),
        Some("two\n".to_string()),
        "the rollback applied generation 3's file",
    );
    let taken = snapshots(&setup.machine);
    checks.eq(
        taken.last().map(|r| r.label.clone()),
        Some("before revert to generation 3".to_string()),
        "a rollback snapshots first too",
    );
    let listed = setup.ok(&["generations"]);
    checks.check(
        listed.contains("snapshots, by the state each holds")
            && listed.lines().any(|line| {
                line.contains("(generation 4)")
                    && line.contains("system as of tree")
                    && line.contains("before revert to generation 3")
            }),
        format_args!("generations lists snapshots by the state each holds: {listed}"),
    );
    checks.finish();
}

#[test]
#[ignore]
fn on_btrfs_a_refused_switch_changes_nothing_and_takes_no_snapshot() {
    let _system = lock_system();
    let machine = Machine::on_btrfs();
    machine.btrfs.as_ref().unwrap().mount_archix();
    let setup = Setup::on(machine, &[], ROLLBACKS, &[]);
    setup.first_switch();
    let before = (setup.machine.current(), snapshots(&setup.machine).len());
    setup.set_body(
        &[],
        &format!(
            r#"{ROLLBACKS}, tmpfiles = {{ "d /var/lib/archix-refused 0750 archix-nosuchuser - -" }}, files = {{ ["/etc/archix-refused"] = {{ text = "x" }} }}"#
        ),
    );
    let refused = setup.fails(&["switch", "--noconfirm"]);
    let checks = Checks::default();
    checks.check(
        refused.contains("systemd-tmpfiles refuses") && refused.contains("nothing was changed"),
        format_args!("{refused}"),
    );
    checks.eq(
        (setup.machine.current(), snapshots(&setup.machine).len()),
        before,
        "a bad tmpfiles line: same generation, no snapshot",
    );
    checks.check(!setup.machine.system_file("etc/archix-refused").exists(), "no file");
    setup.set_body(
        &["tree"],
        &format!(r#"{ROLLBACKS}, files = {{ ["/etc/archix-refused"] = {{ text = "x" }} }}"#),
    );
    for dir in [setup.paths.pacman_cache.clone(), setup.paths.packages()] {
        for cached in fs::read_dir(&dir).into_iter().flatten().flatten() {
            if cached.file_name().to_string_lossy().starts_with("tree-") {
                fs::remove_file(cached.path()).unwrap();
            }
        }
    }
    let output = setup.run_env(
        &[("ARCHIX_TEST_ARCHIVE", "http://127.0.0.1:9")],
        &["switch", "--noconfirm"],
    );
    let text = said(&output);
    checks.check(
        !output.status.success() && text.contains("tree"),
        format_args!("a package that can't be fetched refuses: {text}"),
    );
    checks.eq(
        (setup.machine.current(), snapshots(&setup.machine).len()),
        before,
        "an unfetchable package: same generation, no snapshot",
    );
    checks.check(
        !setup.machine.system_file("etc/archix-refused").exists(),
        "still no file",
    );
    checks.finish();
}

#[test]
#[ignore]
fn revert_applies_an_earlier_generation_in_place_and_refuses_one_root_s_repository_doesnt_hold_as_recorded() {
    let _system = lock_system();
    let setup = Setup::new(&[], r#"files = { ["/etc/archix-rolled"] = { text = "one" } }"#);
    setup.first_switch();
    setup.set_body(&["tree"], r#"files = { ["/etc/archix-rolled"] = { text = "two" } }"#);
    setup.ok(&["switch", "--noconfirm"]);
    let lock = fs::read_to_string(setup.config.join("archix.lock")).unwrap();
    let checks = Checks::default();
    let out = setup.ok(&["revert", "1", "--noconfirm"]);
    checks.eq(installed("tree"), None, "tree goes");
    checks.eq(
        fs::read_to_string(setup.machine.system_file("etc/archix-rolled")).ok(),
        Some("one\n".to_string()),
        "generation 1's file",
    );
    checks.eq(setup.machine.current(), Some(1), "1 is current");
    checks.check(
        !out.contains("reboot") && !out.contains("deployment"),
        format_args!("in place: {out}"),
    );
    checks.eq(
        fs::read_to_string(setup.config.join("archix.lock")).unwrap(),
        lock.clone(),
        "the lock isn't touched",
    );
    setup.ok(&["revert", "2", "--noconfirm"]);
    checks.check(installed("tree").is_some(), "rolling forward brings tree back");
    checks.eq(
        setup
            .ok(&["generations"])
            .lines()
            .filter(|l| l.contains(" UTC "))
            .count(),
        2,
        "no new record",
    );

    let git = |args: &[&str]| -> String {
        let output = Command::new("git")
            .env("GIT_DIR", "/var/lib/archix/config.git")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    let one = git(&["rev-parse", "refs/generations/1"]);
    let two = git(&["rev-parse", "refs/generations/2"]);
    git(&["update-ref", "refs/generations/1", &two]);
    let refused = setup.fails(&["revert", "1", "--noconfirm"]);
    checks.check(
        refused.contains("can't be read back") && refused.contains("nothing was changed"),
        format_args!("a moved ref: {refused}"),
    );
    checks.check(installed("tree").is_some(), "nothing changed after the moved ref");
    git(&["update-ref", "refs/generations/1", &one]);
    let plan = git(&["rev-parse", &format!("{one}:plan.json")]);
    let object = PathBuf::from("/var/lib/archix/config.git/objects")
        .join(&plan[..2])
        .join(&plan[2..]);
    let mut mode = fs::metadata(&object).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o644);
    fs::set_permissions(&object, mode).unwrap();
    fs::write(&object, b"not a git object").unwrap();
    let refused = setup.fails(&["revert", "1", "--noconfirm"]);
    checks.check(
        refused.contains("can't be read back") && refused.contains("nothing was changed"),
        format_args!("a changed object: {refused}"),
    );
    checks.check(installed("tree").is_some(), "nothing changed after the changed object");
    checks.eq(setup.machine.current(), Some(2), "2 stays current");
    checks.finish();
}

#[test]
#[ignore]
fn a_transaction_is_refused_before_it_starts_when_the_root_has_too_few_inodes_left() {
    let dir = tempfile::tempdir().unwrap();
    let (image, mnt) = (dir.path().join("fs.img"), dir.path().join("mnt"));
    fs::create_dir_all(&mnt).unwrap();
    let sh = |script: &str| {
        let status = Command::new("bash").args(["-ec", script]).status().unwrap();
        assert!(status.success(), "{script}");
    };
    sh(&format!(
        "truncate -s 64M {0} && mkfs.ext4 -q -N 8192 {0} && mount -o loop {0} {1}",
        image.display(),
        mnt.display()
    ));
    let db = mnt.join("var/lib/pacman/local");
    fs::create_dir_all(&db).unwrap();
    let checked = archix::conflicts::check(&[], &[], &db, &mnt);
    let filler = mnt.join("filler");
    fs::create_dir(&filler).unwrap();
    let mut made = 0;
    while fs::File::create(filler.join(made.to_string())).is_ok() {
        made += 1;
    }
    for n in 0..64 {
        fs::remove_file(filler.join(n.to_string())).unwrap();
    }
    let refused = archix::conflicts::check(&[], &[], &db, &mnt);
    let _ = Command::new("umount").arg(&mnt).status();
    checked.expect("a root with inodes to spare passes");
    let text = format!(
        "{:#}",
        refused.expect_err("a root without inodes for the transaction passed")
    );
    assert!(
        text.contains("free inodes") && text.contains("nothing was changed"),
        "{text}"
    );
}

#[test]
#[ignore]
fn on_btrfs_the_first_switch_with_rollbacks_lays_the_root_out_itself_and_snapshots_start_after_the_reboot() {
    let _system = lock_system();
    let setup = Setup::on(Machine::on_btrfs(), &[], ROLLBACKS, &[]);
    let out = setup.first_switch();
    let btrfs = setup.machine.btrfs.as_ref().unwrap();
    let subvolumes = Command::new("btrfs")
        .args(["subvolume", "list", "-o"])
        .arg(&btrfs.top)
        .output()
        .unwrap();
    let subvolumes = String::from_utf8_lossy(&subvolumes.stdout).into_owned();
    let checks = Checks::default();
    checks.check(
        out.contains("set up     rollbacks") && out.contains("snapshots start after the next reboot"),
        format_args!("the switch says it sets rollbacks up: {out}"),
    );
    checks.check(
        subvolumes.contains("path @archix") && subvolumes.contains("path @varlib"),
        format_args!("it made @archix and @varlib at the top level: {subvolumes}"),
    );
    let fstab = fs::read_to_string(setup.machine.system_file("etc/fstab")).unwrap_or_default();
    checks.check(
        fstab.contains("subvol=/@archix") && fstab.contains("/var/rollback/pacman /var/lib/pacman none bind"),
        format_args!("and their fstab lines: {fstab}"),
    );
    checks.check(
        snapshots(&setup.machine).is_empty(),
        "no snapshot yet: @archix isn't mounted until the reboot",
    );
    checks.finish();
}

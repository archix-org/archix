//! Switches test machines through deployments; run only inside the throwaway Arch container via `tests/arch/run.sh`.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use archix::paths::Paths;
use common::*;

/// Every entry under `dir` with what an in-place change would alter: inode, mode, size and modification time.
fn identity(dir: &Path) -> BTreeMap<PathBuf, (u64, u32, u64, i64, i64)> {
    fn walk(root: &Path, rel: &Path, out: &mut BTreeMap<PathBuf, (u64, u32, u64, i64, i64)>) {
        let path = root.join(rel);
        let meta = fs::symlink_metadata(&path).unwrap();
        out.insert(
            rel.to_path_buf(),
            (meta.ino(), meta.mode(), meta.size(), meta.mtime(), meta.mtime_nsec()),
        );
        if meta.is_dir() {
            for entry in fs::read_dir(&path).unwrap() {
                walk(root, &rel.join(entry.unwrap().file_name()), out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, Path::new(""), &mut out);
    out
}

/// What must hold after any crash: `current` names a complete deployment, every entry on the ESP names one and starts a UKI that's there, and loader.conf's default is one of the entries.
fn assert_bootable(machine: &Machine, what: &str) {
    let complete = |n: u32| {
        archix::deploy::TREES
            .iter()
            .all(|tree| machine.deployment(n).join(tree).is_dir())
    };
    let current = machine
        .current()
        .unwrap_or_else(|| panic!("{what}: no current deployment"));
    assert!(
        complete(current),
        "{what}: current names deployment {current}, which isn't complete"
    );
    let mut ids = Vec::new();
    for name in machine.entries() {
        let number: u32 = name
            .trim_start_matches("archix-")
            .split(['+', '-', '.'])
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            complete(number),
            "{what}: {name} names deployment {number}, which isn't complete"
        );
        let text = fs::read_to_string(machine.esp().join("loader/entries").join(&name)).unwrap();
        let uki = text
            .lines()
            .find_map(|line| line.strip_prefix("efi ").or_else(|| line.strip_prefix("uki ")))
            .unwrap();
        assert!(
            machine.esp().join(uki.trim_start_matches('/')).is_file(),
            "{what}: {name} starts {uki}, which isn't there"
        );
        ids.push(match name.split_once('+') {
            Some((stem, _)) => format!("{stem}.conf"),
            None => name,
        });
    }
    let fallbacks = archix::system::boot::fallbacks(&machine.esp());
    assert!(!fallbacks.is_empty(), "{what}: no fallback UKI");
    for number in fallbacks {
        assert!(
            complete(number),
            "{what}: the fallback UKI names deployment {number}, which isn't complete"
        );
    }
    let loader = machine.loader_conf();
    let default = loader
        .lines()
        .find_map(|line| line.strip_prefix("default "))
        .unwrap_or_else(|| panic!("{what}: loader.conf has no default:\n{loader}"));
    assert!(
        ids.iter().any(|id| id == default.trim()),
        "{what}: the default {default} has no entry"
    );
}

#[test]
#[ignore]
fn the_first_switch_lays_out_deployment_1_and_each_switch_builds_the_next_leaving_it_untouched() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    let machine = &setup.machine;
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(out.contains("laid the system out as deployment 1"), "{out}");
    assert_eq!(
        machine.deployments(),
        [1],
        "a first switch that changes nothing builds no second deployment:\n{out}"
    );
    assert_eq!(machine.current(), Some(1));
    assert_eq!(machine.entries(), ["archix-1.conf"]);
    assert!(machine.loader_conf().contains("default archix-1.conf"));
    assert!(
        machine.root.join("usr/bin/pacman").exists(),
        "the live root runs on deployment 1"
    );
    machine.reboot(Some("archix-1.conf"));
    assert!(machine.root.join("usr/bin/pacman").exists(), "deployment 1 boots");
    for tree in ["usr", "opt", "boot"] {
        let err = fs::write(machine.root.join(tree).join("written"), "x").unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EROFS), "/{tree} is writable: {err}");
    }
    let one = identity(&machine.deployment(1));
    let db_before = fs::read_dir(machine.deployment(1).join("pacman")).unwrap().count();

    setup.set_body(&["tree"], "");
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(out.contains("boots at the next restart"), "{out}");
    assert_eq!(machine.deployments(), [1, 2]);
    assert_eq!(machine.current(), Some(2));
    assert!(machine.installed_in(2, "tree").is_some());
    assert!(machine.deployment(2).join("usr/bin/tree").is_file());
    assert_eq!(machine.installed_in(1, "tree"), None);
    assert!(
        !machine.root.join("usr/bin/tree").exists(),
        "the running system changes only at the next boot"
    );
    assert_eq!(identity(&machine.deployment(1)), one, "deployment 1 was changed");
    assert_eq!(
        fs::read_dir(machine.deployment(1).join("pacman")).unwrap().count(),
        db_before
    );
    assert!(machine.leftovers().is_empty(), "{:?}", machine.leftovers());
    assert_bootable(machine, "after the switch");
    machine.reboot(Some("archix-2.conf"));
    assert!(
        machine.root.join("usr/bin/tree").is_file(),
        "deployment 2 boots with tree"
    );

    assert!(setup.ok(&["switch", "--noconfirm"]).contains("nothing to do"));
    assert_eq!(machine.deployments(), [1, 2]);
    assert!(setup.ok(&["status"]).contains("the system matches the lock"));
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(installed("tree"), None, "undeclared packages are removed by default");
    assert_eq!(machine.current(), Some(3));
}

#[test]
#[ignore]
fn rollback_to_a_kept_deployment_repoints_current_and_the_boot_default_without_reinstalling() {
    let _system = lock_system();
    let setup = Setup::new(&["tree"], "");
    setup.lay_out();
    let machine = &setup.machine;
    let with_tree = machine.current().unwrap();
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    let without_tree = machine.current().unwrap();
    assert!(without_tree > with_tree);
    assert_eq!(installed("tree"), None);
    let listing = setup.ok(&["generations"]);
    assert!(listing.contains(&format!("* {without_tree:>4}")), "{listing}");
    assert!(
        setup
            .ok(&["status"])
            .contains(&format!("current generation: {without_tree}")),
        "status names the current generation"
    );
    let log = fs::read_to_string(machine.root.join("var/log/pacman.log")).unwrap();
    let deployments = machine.deployments();
    let one = identity(&machine.deployment(with_tree));

    let out = setup.ok(&["rollback", "--noconfirm"]);
    assert!(out.contains("nothing is reinstalled"), "{out}");
    assert_eq!(
        machine.current(),
        Some(with_tree),
        "rollback goes to the previous generation"
    );
    assert!(
        machine
            .loader_conf()
            .contains(&format!("default archix-{with_tree}.conf")),
        "{}",
        machine.loader_conf()
    );
    assert_eq!(machine.deployments(), deployments, "no deployment is built");
    assert_eq!(
        fs::read_to_string(machine.root.join("var/log/pacman.log")).unwrap(),
        log,
        "pacman doesn't run"
    );
    assert_eq!(identity(&machine.deployment(with_tree)), one);
    assert!(installed("tree").is_some());
    assert_bootable(machine, "after the rollback");
    machine.reboot(None);
    assert!(
        machine.root.join("usr/bin/tree").is_file(),
        "the rolled-back deployment boots"
    );

    setup.ok(&["rollback", &without_tree.to_string(), "--noconfirm"]);
    assert_eq!(machine.current(), Some(without_tree));
    assert_eq!(installed("tree"), None);
    assert!(
        setup.ok(&["switch", "--noconfirm"]).contains("nothing to do"),
        "the system already matches the config"
    );
}

#[test]
#[ignore]
fn rollback_to_a_generation_whose_deployment_was_dropped_rebuilds_it_from_the_package_cache_unless_the_esp_cant_hold_its_entry()
 {
    let _system = lock_system();
    let checks = Checks::default();
    let keep = "deployments = { keep = 1 }";
    let setup = Setup::new(&["tree"], keep);
    setup.lay_out();
    let machine = &setup.machine;
    let with_tree = machine.current().unwrap();
    setup.set_body(&["lemon"], keep);
    setup.ok(&["switch", "--noconfirm"]);
    let with_lemon = machine.current().unwrap();
    machine.reboot(Some(&format!("archix-{with_lemon}.conf")));
    setup.set_body(&[], keep);
    let out = setup.ok(&["switch", "--noconfirm"]);
    let newest = machine.current().unwrap();
    assert!(
        !machine.deployments().contains(&with_tree),
        "keep = 1 drops what isn't booted, current or the default:\n{out}"
    );
    checks.check(
        machine.deployments().contains(&with_lemon),
        "the booted deployment stays",
    );
    checks.check(
        setup.ok(&["generations"]).contains(&format!("{with_tree:>4}")),
        "the dropped generation stays as a record",
    );
    checks.check(
        machine
            .entries()
            .iter()
            .all(|e| !e.starts_with(&format!("archix-{with_tree}"))),
        format_args!("the dropped deployment's entry goes: {:?}", machine.entries()),
    );

    {
        let esp = FatEsp::new(machine, "160M");
        esp.fill(1 << 20);
        let before = (
            machine.deployments(),
            machine.current(),
            machine.entries(),
            machine.loader_conf(),
        );
        let output = setup.run(&["rollback", &with_tree.to_string(), "--noconfirm"]);
        let err = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "a rollback onto a full ESP wasn't refused");
        checks.check(
            err.contains("the ESP at") && err.contains("nothing was changed"),
            format_args!("a rollback onto a full ESP says why and that nothing changed: {err}"),
        );
        checks.eq(
            (
                machine.deployments(),
                machine.current(),
                machine.entries(),
                machine.loader_conf(),
            ),
            before,
            "a rollback refused for a full ESP changes nothing",
        );
        checks.check(
            machine.leftovers().is_empty(),
            format_args!(
                "a rollback refused for a full ESP leaves nothing behind: {:?}",
                machine.leftovers()
            ),
        );
    }

    let record = setup
        .paths
        .state
        .join(format!("generations/{with_tree}/generation.json"));
    let text = fs::read_to_string(&record).unwrap();
    let field = format!("\"format\": {},", archix::generations::FORMAT);
    assert!(text.contains(&field), "records carry their format:\n{text}");
    fs::write(&record, text.replace(&field, "\"format\": 99,")).unwrap();
    let refused = setup.fails(&["rollback", &with_tree.to_string(), "--noconfirm"]);
    checks.check(
        refused.contains("written by a newer archix (record format 99") && refused.contains("nothing was changed"),
        format_args!("a newer archix's record is refused: {refused}"),
    );
    checks.eq(machine.current(), Some(newest), "a refused newer record leaves current");
    checks.check(
        setup.ok(&["generations"]).contains(&format!("{with_tree:>4}")),
        "a refused newer record stays listed",
    );
    fs::write(&record, &text).unwrap();

    let out = setup.ok(&["rollback", &with_tree.to_string(), "--noconfirm"]);
    checks.check(
        out.contains("rebuilding it from deployment"),
        format_args!("the rollback rebuilds: {out}"),
    );
    checks.eq(
        machine.current(),
        Some(with_tree),
        "the rebuilt deployment keeps its generation's number",
    );
    checks.check(
        machine.installed_in(with_tree, "tree").is_some(),
        "the rebuilt deployment has tree",
    );
    checks.eq(
        machine.installed_in(with_tree, "lemon"),
        None,
        "the rebuilt deployment lacks lemon",
    );
    checks.check(
        machine.entries().contains(&format!("archix-{with_tree}.conf")),
        format_args!("the rebuilt deployment gets its entry: {:?}", machine.entries()),
    );
    checks.check(
        machine.leftovers().is_empty(),
        format_args!("the rebuild leaves nothing behind: {:?}", machine.leftovers()),
    );
    checks.check(
        machine.deployments().contains(&with_lemon),
        "the booted deployment stays after the rebuild",
    );
    checks.check(
        !machine.deployments().contains(&newest),
        format_args!(
            "a rebuilding rollback runs gc like a switch: {:?}",
            machine.deployments()
        ),
    );
    assert_bootable(machine, "after the rebuild");
    machine.reboot(None);
    checks.check(
        machine.root.join("usr/bin/tree").is_file(),
        "the rebuilt deployment boots with tree",
    );
    checks.finish();
}

#[test]
#[ignore]
fn gc_drops_deployments_past_keep_but_never_the_booted_current_or_default_one() {
    let _system = lock_system();
    let body = |n: u32| {
        format!(
            r#"deployments = {{ keep = 2 }}, generations = {{ keep = 1, keep_days = 0 }}, files = {{ ["/etc/archix-test-gc"] = {{ text = "{n}" }} }}"#
        )
    };
    let setup = Setup::new(&["tree"], &body(0));
    setup.lay_out();
    let machine = &setup.machine;
    let booted = machine.current().unwrap();
    for n in 1..=4 {
        setup.set_body(&[], &body(n));
        setup.ok(&["switch", "--noconfirm"]);
    }
    let current = machine.current().unwrap();
    let kept = machine.deployments();
    assert_eq!(kept, [booted, current], "keep = 2 counts the booted and current ones");
    let records: Vec<u32> = archix::generations::Store::new(&Paths::system())
        .list()
        .unwrap()
        .iter()
        .map(|g| g.number)
        .collect();
    for n in &kept {
        assert!(
            records.contains(n),
            "deployment {n} lost its generation record: {records:?}"
        );
    }
    assert_eq!(machine.entries().len(), kept.len(), "{:?}", machine.entries());
    assert!(
        machine.installed_in(booted, "tree").is_some()
            && fs::read_dir(setup.paths.packages())
                .unwrap()
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().starts_with("tree-")),
        "the booted generation's packages stay cached"
    );
    assert!(setup.ok(&["gc"]).contains("freed"));

    machine.reboot(Some(&format!("archix-{current}.conf")));
    setup.ok(&["gc"]);
    assert_eq!(
        machine.deployments().len(),
        2,
        "once the newest has booted, keep = 2 keeps two: {:?}",
        machine.deployments()
    );
    let one = "deployments = { keep = 1 }";
    setup.set_body(&[], one);
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(
        machine.deployments().len(),
        2,
        "keep = 1 keeps the booted one until the new one has booted: {:?}",
        machine.deployments()
    );
    machine.reboot(Some(&format!("archix-{}.conf", machine.current().unwrap())));
    setup.ok(&["gc"]);
    assert_eq!(machine.deployments(), [machine.current().unwrap()]);
    assert_bootable(machine, "after gc");
}

#[test]
#[ignore]
fn deployments_keep_below_one_is_refused() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    let init = setup.read("init.lua").unwrap();
    setup.write(
        "init.lua",
        &init.replacen("return { ", "return { deployments = { keep = 0 }, ", 1),
    );
    let err = setup.fails(&["switch", "--noconfirm"]);
    assert!(err.contains("deployments.keep"), "{err}");
    assert!(setup.machine.current().is_none());
}

#[test]
#[ignore]
fn a_kill_at_any_step_of_a_switch_leaves_a_bootable_machine_and_the_next_switch_needs_no_repair() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    setup.lay_out();
    let machine = &setup.machine;
    let installed_lua = setup.dir.path().join("lua/hosts/box/installed.lua");
    let steps = [
        "deploy-stage",
        "deploy-transact",
        "deploy-flatten",
        "deploy-commit-sync",
        "deploy-commit-rename",
        "deploy-commit-esp",
        "deploy-esp-uki",
        "deploy-esp-entry",
        "deploy-esp-loader",
        "deploy-esp-fallback",
        "deploy-commit-current",
        "deploy-done",
    ];
    for (i, step) in steps.iter().enumerate() {
        let what = format!("killed at {step}");
        let (verb, package) = if i % 2 == 0 {
            ("install", "tree")
        } else {
            ("remove", "tree")
        };
        let before = machine.current().unwrap();
        let killed = setup.run_env(&[("ARCHIX_TEST_CRASH", step)], &[verb, package, "--noconfirm"]);
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&killed.status),
            Some(libc::SIGKILL),
            "{what}: {}",
            String::from_utf8_lossy(&killed.stderr)
        );
        assert_bootable(machine, &what);
        let after = machine.current().unwrap();
        assert!(after >= before, "{what}: current went back to {after}");
        if after == before {
            let declared = archix::installed::read(&installed_lua).unwrap_or_default();
            assert_eq!(
                declared.contains(&"tree".to_string()),
                verb == "remove",
                "{what}: installed.lua changed though nothing was committed"
            );
        }
        machine.reboot(None);
        assert!(
            machine.root.join("usr/bin/pacman").is_file(),
            "{what}: the machine doesn't boot"
        );
        let out = setup.ok(&[verb, package, "--noconfirm"]);
        assert!(
            machine.leftovers().is_empty(),
            "{what}: {:?}\n{out}",
            machine.leftovers()
        );
        assert_eq!(installed("tree").is_some(), verb == "install", "{what}");
        assert_bootable(machine, &format!("{what}, then a switch"));
    }
}

#[test]
#[ignore]
fn a_deployment_that_fails_its_tries_falls_back_and_the_one_that_booted_becomes_current() {
    let _system = lock_system();
    let counting = ("ARCHIX_TEST_LOADER_FEATURES", "4194303");
    let setup = Setup::new(&[], "");
    setup.ok_env(&[counting], &["switch", "--noconfirm"]);
    let machine = &setup.machine;
    let entries_dir = machine.esp().join("loader/entries");
    // What systemd-bless-boot does once a boot reaches boot-complete.target.
    let bless = |n: u32| {
        fs::rename(
            entries_dir.join(format!("archix-{n}+3.conf")),
            entries_dir.join(format!("archix-{n}.conf")),
        )
        .unwrap()
    };
    bless(1);
    machine.reboot(Some("archix-1.conf"));
    setup.ok_env(&[counting], &["boot-complete"]);
    setup.set_body(&["tree"], "");
    setup.ok_env(&[counting], &["switch", "--noconfirm"]);
    let new = machine.current().unwrap();
    let entries = machine.entries();
    assert!(entries.contains(&format!("archix-{new}+3.conf")), "{entries:?}");
    let loader = machine.loader_conf();
    assert!(
        loader.contains("default archix-1.conf") && loader.contains(&format!("preferred archix-{new}.conf")),
        "the new deployment is preferred, the proven one the default:\n{loader}"
    );

    fs::rename(
        entries_dir.join(format!("archix-{new}+3.conf")),
        entries_dir.join(format!("archix-{new}+0-3.conf")),
    )
    .unwrap();
    machine.reboot(Some("archix-1.conf"));
    let out = setup.ok_env(&[counting], &["boot-check"]);
    assert!(out.contains("didn't finish booting"), "{out}");
    assert_eq!(machine.current(), Some(1), "the deployment that booted is current");
    assert!(machine.loader_conf().contains("default archix-1.conf"));
    assert!(installed("tree").is_none());
    let notes = fs::read_to_string(setup.paths.notes()).unwrap_or_default();
    assert!(notes.contains("didn't finish booting"), "{notes}");

    setup.ok_env(&[counting], &["switch", "--noconfirm"]);
    let again = machine.current().unwrap();
    machine.reboot(Some(&format!("archix-{again}.conf")));
    bless(again);
    setup.ok_env(&[counting], &["boot-complete"]);
    assert!(
        machine.loader_conf().contains(&format!("default archix-{again}.conf")),
        "a deployment that booted fine becomes the default:\n{}",
        machine.loader_conf()
    );
}

#[test]
#[ignore]
fn tries_that_stopped_for_a_missing_required_disk_say_so_after_the_fallback_and_a_rollback_retries_them() {
    let _system = lock_system();
    let counting = ("ARCHIX_TEST_LOADER_FEATURES", "4194303");
    let setup = Setup::new(&[], "");
    setup.ok_env(&[counting], &["switch", "--noconfirm"]);
    let machine = &setup.machine;
    let entries_dir = machine.esp().join("loader/entries");
    let rename = |from: String, to: String| fs::rename(entries_dir.join(from), entries_dir.join(to)).unwrap();
    rename("archix-1+3.conf".into(), "archix-1.conf".into());
    machine.reboot(Some("archix-1.conf"));
    setup.ok_env(&[counting], &["boot-complete"]);
    setup.set_body(
        &[],
        r#"file_systems = { ["/home"] = { device = "UUID=archix-test-missing-home", fs_type = "ext4" } }"#,
    );
    setup.ok_env(&[counting], &["switch", "--noconfirm"]);
    let new = machine.current().unwrap();
    let drop_in = fs::read_to_string(
        machine
            .deployment(new)
            .join("etc/systemd/system/emergency.service.d/archix-required-mounts.conf"),
    )
    .unwrap_or_default();
    assert_eq!(
        drop_in, "[Service]\nExecStartPre=-/usr/bin/archix emergency-hint --no-rescue-entry\n",
        "a deployment with a required mount gets emergency mode's hint"
    );

    // A counted try of the new deployment, stopped in emergency mode without the /home disk.
    rename(format!("archix-{new}+3.conf"), format!("archix-{new}+2-1.conf"));
    machine.reboot(Some(&format!("archix-{new}.conf")));
    let hint = setup.ok_env(
        &[counting, ("ARCHIX_TEST_COUNTED_BOOT", "1")],
        &["emergency-hint", "--no-rescue-entry"],
    );
    for line in [
        "archix:   /home (UUID=archix-test-missing-home): its disk isn't attached\n",
        "archix: reattach the disk and reboot,\n",
        &format!("archix: this is one of deployment {new}'s counted boot tries"),
    ] {
        assert!(hint.contains(line), "the hint should say {line:?}:\n{hint}");
    }
    assert!(!hint.contains("rescue entry"), "{hint}");

    rename(format!("archix-{new}+2-1.conf"), format!("archix-{new}+0-3.conf"));
    machine.reboot(Some("archix-1.conf"));
    let out = setup.ok_env(&[counting], &["boot-check"]);
    let why = format!(
        "archix: deployment {new} failed its boot tries because /home (UUID=archix-test-missing-home) was missing, not because of the deployment, so systemd-boot started 1; switch again (or `archix rollback {new}`) to retry it"
    );
    assert!(out.contains(&why), "{out}");
    assert_eq!(machine.current(), Some(1));
    let notes = fs::read_to_string(setup.paths.notes()).unwrap_or_default();
    assert!(notes.contains(&why), "{notes}");
    let status = setup.ok(&["status"]);
    assert!(status.contains(&why), "{status}");

    let out = setup.ok_env(&[counting], &["rollback", &new.to_string(), "--noconfirm"]);
    assert!(
        out.contains(&format!(
            "deployment {new}'s entry had used up its boot tries; it gets them again"
        )),
        "{out}"
    );
    assert!(
        machine.entries().contains(&format!("archix-{new}+3.conf")),
        "{:?}",
        machine.entries()
    );
    let loader = machine.loader_conf();
    assert!(
        loader.contains("default archix-1.conf") && loader.contains(&format!("preferred archix-{new}.conf")),
        "the retried deployment is preferred, the proven one still the default:\n{loader}"
    );
    assert_eq!(machine.current(), Some(new));
    assert!(
        !setup.ok(&["status"]).contains(&why),
        "the note goes once the deployment is current again"
    );
}

#[test]
#[ignore]
fn a_failed_health_check_falls_back_says_why_and_names_unsafe_downgrades_and_hibernation_waits_for_the_reboot() {
    let _system = lock_system();
    let counting = ("ARCHIX_TEST_LOADER_FEATURES", "4194303");
    let setup = Setup::new(&["lemon"], "");
    let machine = &setup.machine;
    let body = format!(
        r#"boot = {{ loader = "systemd-boot", esp = "{}", kernel = "linux", rescue_entry = false, health_check = {{ listening = {{ 1 }}, timeout = 1 }} }}, kernel_params = {{ "root=/dev/vda2 rw" }}, unsafe_downgrades = {{ lemon = {{ "3.54" }} }}"#,
        machine.esp().display()
    );
    setup.set_body(&["lemon"], &body);
    setup.ok_env(&[counting], &["switch", "--noconfirm"]);
    let good = machine.current().unwrap();
    let entries_dir = machine.esp().join("loader/entries");
    fs::rename(
        entries_dir.join(format!("archix-{good}+3.conf")),
        entries_dir.join(format!("archix-{good}.conf")),
    )
    .unwrap();
    machine.reboot(Some(&format!("archix-{good}.conf")));
    setup.ok_env(&[counting], &["boot-complete"]);
    setup.ok_env(&[counting], &["boot-check"]);
    let guard = machine
        .root
        .join("run/systemd/sleep.conf.d/zz-archix-pending-deployment.conf");
    assert!(!guard.exists(), "nothing is pending on the deployment that booted");

    setup.set_body(&["lemon", "tree"], &body);
    let out = setup.ok_env(&[counting], &["switch", "--noconfirm"]);
    let new = machine.current().unwrap();
    assert!(
        out.contains(&format!("hibernation is off until you reboot into deployment {new}")),
        "{out}"
    );
    let text = fs::read_to_string(&guard).unwrap_or_default();
    assert!(
        text.contains("AllowHibernation=no") && !text.contains("AllowSuspend="),
        "{text}"
    );
    assert!(setup.ok(&["status"]).contains("hibernation is off"));
    let desc = machine.deployment(new).join("pacman/lemon-3.53.4-1/desc");
    let migrated = fs::read_to_string(&desc).unwrap().replace("3.53.4-1", "3.54.0-1");
    machine.store_writable(true);
    fs::write(&desc, migrated).unwrap();
    machine.store_writable(false);

    machine.reboot(Some(&format!("archix-{new}.conf")));
    setup.ok_env(&[counting], &["boot-check"]);
    assert!(!guard.exists(), "the next boot allows hibernation again");
    let checked = setup.run_env(&[counting], &["health-check"]);
    let out = format!(
        "{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );
    assert!(!checked.status.success(), "{out}");
    assert!(
        out.contains(&format!(
            "deployment {new} failed its health check \"TCP port 1 listening\""
        )),
        "{out}"
    );
    assert!(
        machine.entries().contains(&format!("archix-{new}+0-3.conf")),
        "the failed deployment's entry has no tries left: {:?}",
        machine.entries()
    );

    machine.reboot(Some(&format!("archix-{good}.conf")));
    let out = setup.ok_env(&[counting], &["boot-check"]);
    let why =
        format!("deployment {new} fell back to {good} because the health check \"TCP port 1 listening\" failed at ");
    let risky = "lemon 3.54.0-1 -> 3.53.4-1 crosses 3.54";
    assert!(out.contains(&why) && out.contains(risky), "{out}");
    assert_eq!(machine.current(), Some(good));
    let notes = fs::read_to_string(setup.paths.notes()).unwrap_or_default();
    assert!(notes.contains(&why) && notes.contains(risky), "{notes}");
    let status = setup.ok(&["status"]);
    assert!(status.contains(&why) && status.contains(risky), "{status}");
}

#[test]
#[ignore]
fn a_read_only_esp_at_boot_keeps_the_deployment_current_says_so_and_is_retried_at_the_next_boot() {
    let _system = lock_system();
    let counting = ("ARCHIX_TEST_LOADER_FEATURES", "4194303");
    let setup = Setup::new(&[], "");
    setup.ok_env(&[counting], &["switch", "--noconfirm"]);
    let machine = &setup.machine;
    let entries_dir = machine.esp().join("loader/entries");
    let rename = |from: String, to: String| fs::rename(entries_dir.join(from), entries_dir.join(to)).unwrap();
    rename("archix-1+3.conf".into(), "archix-1.conf".into());
    machine.reboot(Some("archix-1.conf"));
    setup.ok_env(&[counting], &["boot-complete"]);
    setup.set_body(&["tree"], "");
    setup.ok_env(&[counting], &["switch", "--noconfirm"]);
    let new = machine.current().unwrap();
    let esp = FatEsp::new(machine, "256M");
    let read_only = |ro: bool| {
        run(Command::new("mount")
            .args(["-o", if ro { "remount,ro" } else { "remount,rw" }])
            .arg(&esp.at))
    };
    let notes = || fs::read_to_string(setup.paths.notes()).unwrap_or_default();
    let said = "is read-only";

    // The first try of the new deployment, whose blessing the kernel's read-only FAT stops.
    rename(format!("archix-{new}+3.conf"), format!("archix-{new}+2-1.conf"));
    machine.reboot(Some(&format!("archix-{new}.conf")));
    read_only(true);
    let loader = machine.loader_conf();
    setup.ok_env(&[counting], &["boot-check"]);
    // systemd-bless-boot's drop-in ignores archix bless failing (ExecStartPre=-), and its own rename then fails the same way.
    let blessed = setup.run_env(&[counting], &["bless"]);
    let why = String::from_utf8_lossy(&blessed.stderr);
    assert!(
        !blessed.status.success() && why.contains(&format!("renaming archix-{new}+2-1.conf")),
        "{why}"
    );
    assert!(
        machine.entries().contains(&format!("archix-{new}+2-1.conf")),
        "its counters stay as they were: {:?}",
        machine.entries()
    );
    let out = setup.ok_env(&[counting], &["boot-complete"]);
    assert!(out.contains(said) && out.contains("couldn't count this boot"), "{out}");
    assert_eq!(machine.current(), Some(new), "a healthy deployment isn't rolled back");
    assert_eq!(machine.loader_conf(), loader);
    assert!(notes().contains(said), "{}", notes());
    assert!(setup.ok(&["status"]).contains(said));
    let again = setup.ok_env(&[counting], &["boot-complete"]);
    assert_eq!(notes().matches(said).count(), 1, "said once: {}\n{again}", notes());

    // The next try, after fsck at boot repaired the ESP: systemd-bless-boot counts it, and archix makes it the default.
    read_only(false);
    rename(format!("archix-{new}+2-1.conf"), format!("archix-{new}.conf"));
    machine.reboot(Some(&format!("archix-{new}.conf")));
    setup.ok_env(&[counting], &["boot-check"]);
    setup.ok_env(&[counting], &["boot-complete"]);
    assert!(
        machine.loader_conf().contains(&format!("default archix-{new}.conf")),
        "{}",
        machine.loader_conf()
    );
    assert!(!notes().contains(said), "{}", notes());
    assert!(!setup.ok(&["status"]).contains(said));

    // A blessed deployment whose ESP turned read-only before archix made it the default.
    setup.set_body(&[], "");
    setup.ok_env(&[counting], &["switch", "--noconfirm"]);
    let newest = machine.current().unwrap();
    rename(format!("archix-{newest}+3.conf"), format!("archix-{newest}.conf"));
    machine.reboot(Some(&format!("archix-{newest}.conf")));
    read_only(true);
    let out = setup.ok_env(&[counting], &["boot-complete"]);
    assert!(out.contains(said) && out.contains("the boot default"), "{out}");
    assert_eq!(machine.current(), Some(newest));
    assert!(notes().contains(said), "{}", notes());
    read_only(false);
    setup.ok_env(&[counting], &["boot-complete"]);
    assert!(machine.loader_conf().contains(&format!("default archix-{newest}.conf")));
    assert!(!notes().contains(said), "{}", notes());
}

#[test]
#[ignore]
fn archix_blesses_a_counted_boot_itself_before_systemd_bless_boot_and_syncs_the_entry_under_its_new_name() {
    let _system = lock_system();
    let counting = ("ARCHIX_TEST_LOADER_FEATURES", "4194303");
    let setup = Setup::new(&[], "");
    setup.ok_env(&[counting], &["switch", "--noconfirm"]);
    let machine = &setup.machine;
    let new = machine.current().unwrap();
    let dropin = machine
        .deployment(new)
        .join("etc/systemd/system/systemd-bless-boot.service.d/archix.conf");
    let text = fs::read_to_string(&dropin).unwrap_or_default();
    assert!(text.contains("ExecStartPre=-") && text.contains(" bless"), "{text}");
    let entries_dir = machine.esp().join("loader/entries");
    let counted = entries_dir.join(format!("archix-{new}+3.conf"));
    let content = fs::read_to_string(&counted).unwrap();
    fs::rename(&counted, entries_dir.join(format!("archix-{new}+2-1.conf"))).unwrap();
    machine.reboot(Some(&format!("archix-{new}.conf")));
    let out = setup.ok_env(&[counting], &["bless"]);
    assert!(
        out.contains(&format!("renamed archix-{new}+2-1.conf to archix-{new}.conf")),
        "{out}"
    );
    assert_eq!(machine.entries(), [format!("archix-{new}.conf")]);
    assert_eq!(
        fs::read_to_string(entries_dir.join(format!("archix-{new}.conf"))).unwrap(),
        content
    );
    assert_eq!(
        setup.ok_env(&[counting], &["bless"]),
        "",
        "systemd-bless-boot's own rename then finds it done"
    );
    setup.ok_env(&[counting], &["boot-complete"]);
    assert!(machine.loader_conf().contains(&format!("default archix-{new}.conf")));
}

fn efivar(entry: &str) -> Vec<u8> {
    let mut bytes = vec![6, 0, 0, 0];
    bytes.extend(entry.encode_utf16().flat_map(u16::to_le_bytes));
    bytes.extend([0, 0]);
    bytes
}

#[test]
#[ignore]
fn a_default_picked_in_the_boot_menu_is_cleared_at_the_commit_and_status_warns_until_then() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    setup.lay_out();
    let machine = &setup.machine;
    let vars = tempfile::tempdir().unwrap();
    let var = |name: &str| vars.path().join(format!("{name}-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f"));
    fs::write(var("LoaderEntryDefault"), efivar("archix-1.conf")).unwrap();
    fs::write(var("LoaderEntryOneShot"), efivar("archix-1.conf")).unwrap();
    let env = [("ARCHIX_TEST_EFIVARS", vars.path().to_str().unwrap())];
    let status = setup.ok_env(&env, &["status"]);
    assert!(
        status.contains(
            "LoaderEntryDefault variable (the boot menu's default, or `bootctl set-default`) names archix-1.conf"
        ),
        "{status}"
    );
    setup.set_body(&["tree"], "");
    let out = setup.ok_env(&env, &["switch", "--noconfirm"]);
    let new = machine.current().unwrap();
    assert!(
        out.contains(&format!(
            "cleared LoaderEntryDefault (archix-1.conf), set in the boot menu or with bootctl, which overrode loader.conf and would have kept archix-{new}.conf from booting"
        )),
        "{out}"
    );
    assert!(!var("LoaderEntryDefault").exists());
    assert!(out.contains("the next boot starts archix-1.conf once"), "{out}");
    assert!(
        var("LoaderEntryOneShot").exists(),
        "a one-shot boot is the user's to make"
    );
    assert!(!setup.ok_env(&env, &["status"]).contains("LoaderEntryDefault"));
}

#[test]
#[ignore]
fn a_boot_where_nothing_changed_writes_no_efi_variable_and_leaves_loader_conf_alone() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    setup.lay_out();
    let machine = &setup.machine;
    let vars = tempfile::tempdir().unwrap();
    let var = |name: &str| vars.path().join(format!("{name}-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f"));
    let current = machine.current().unwrap();
    for name in ["LoaderEntrySelected", "LoaderEntryOneShot", "LoaderConfigTimeout"] {
        fs::write(var(name), efivar(&format!("archix-{current}.conf"))).unwrap();
    }
    let env = [("ARCHIX_TEST_EFIVARS", vars.path().to_str().unwrap())];
    let snapshot = || {
        let mut files: Vec<(String, u64, i64, i64)> = fs::read_dir(vars.path())
            .unwrap()
            .chain(fs::read_dir(machine.esp().join("loader")).unwrap())
            .map(|file| {
                let file = file.unwrap();
                let meta = file.metadata().unwrap();
                (file.path().display().to_string(), meta.ino(), meta.mtime(), meta.mtime_nsec())
            })
            .collect();
        files.sort();
        files
    };
    setup.ok_env(&env, &["boot-check"]);
    setup.ok_env(&env, &["boot-complete"]);
    for boot in 1..=2 {
        let before = snapshot();
        machine.reboot(Some(&format!("archix-{current}.conf")));
        setup.ok_env(&env, &["boot-check"]);
        setup.ok_env(&env, &["boot-complete"]);
        assert_eq!(
            snapshot(),
            before,
            "steady-state boot {boot} wrote an EFI variable or loader.conf:\n{}",
            machine.loader_conf()
        );
    }
    assert!(machine.loader_conf().contains(&format!("default archix-{current}.conf")));
}

fn run(command: &mut Command) -> String {
    let out = command.output().unwrap();
    assert!(
        out.status.success(),
        "{command:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The machine's ESP moved onto a FAT image, mounted at the same place until dropped.
struct FatEsp {
    _dir: tempfile::TempDir,
    dev: String,
    at: PathBuf,
}

impl FatEsp {
    fn new(machine: &Machine, size: &str) -> FatEsp {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("esp.img");
        run(Command::new("truncate").args(["-s", size]).arg(&img));
        let dev = run(Command::new("losetup").args(["--show", "-f"]).arg(&img))
            .trim()
            .to_string();
        let esp = FatEsp {
            _dir: dir,
            dev,
            at: machine.esp(),
        };
        run(Command::new("mkfs.fat").args(["-F", "32"]).arg(&esp.dev));
        let staged = esp._dir.path().join("mnt");
        fs::create_dir(&staged).unwrap();
        run(Command::new("mount").arg(&esp.dev).arg(&staged));
        run(Command::new("cp").arg("-rT").arg(&esp.at).arg(&staged));
        run(Command::new("umount").arg(&staged));
        run(Command::new("mount").arg(&esp.dev).arg(&esp.at));
        esp
    }

    /// Fills the FAT until `leave` bytes are free.
    fn fill(&self, leave: u64) {
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        let path = std::ffi::CString::new(self.at.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::statvfs(path.as_ptr(), &mut stat) }, 0);
        let free = stat.f_bavail * stat.f_frsize;
        // FAT can't fallocate, so the filler is written out.
        let blocks = free.saturating_sub(leave) / 65536;
        run(Command::new("dd")
            .args(["if=/dev/zero", "bs=64K", "status=none", &format!("count={blocks}")])
            .arg(format!("of={}", self.at.join("filler").display())));
    }
}

impl Drop for FatEsp {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg("-l").arg(&self.at).status();
        let _ = Command::new("losetup").arg("-d").arg(&self.dev).status();
    }
}

#[test]
#[ignore]
fn a_switch_rollback_or_gc_is_refused_while_no_fat_filesystem_is_mounted_at_the_esp() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    setup.lay_out();
    let machine = &setup.machine;
    setup.set_body(&["tree"], "");
    setup.ok(&["switch", "--noconfirm"]);
    setup.set_body(&[], "");
    let check = [("ARCHIX_TEST_CHECK_ESP", "1")];
    let state = || {
        (
            machine.deployments(),
            machine.current(),
            machine.entries(),
            machine.loader_conf(),
        )
    };
    let before = state();
    for args in [
        &["switch", "--noconfirm"][..],
        &["rollback", "1", "--noconfirm"],
        &["gc"],
    ] {
        let output = setup.run_env(&check, args);
        let err = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{args:?} ran with no ESP mounted");
        assert!(
            err.contains("no FAT filesystem is mounted there") && err.contains("nothing was changed"),
            "{args:?}: {err}"
        );
        assert_eq!(state(), before, "{args:?} changed something");
    }
    let _esp = FatEsp::new(machine, "256M");
    setup.ok_env(&check, &["switch", "--noconfirm"]);
    assert_ne!(
        machine.current(),
        before.1,
        "with the ESP mounted the switch goes ahead"
    );
}

#[test]
#[ignore]
fn a_package_pacman_would_reject_stops_the_switch_before_anything_is_staged() {
    let _system = lock_system();
    let setup = Setup::new(&["tree"], "");
    setup.lay_out();
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
    let deployments = machine.deployments();
    let err = setup.fails(&["switch", "--noconfirm"]);
    drop((restore, restore_shared));
    assert!(err.contains("nothing was changed"), "{err}");
    assert_eq!(
        machine.deployments(),
        deployments,
        "no deployment was staged or committed"
    );
    assert!(machine.leftovers().is_empty(), "{:?}", machine.leftovers());
    assert!(installed("lemon").is_some());
    setup.ok(&["switch", "--noconfirm"]);
    assert!(installed("tree").is_some() && installed("lemon").is_none());
}

#[test]
#[ignore]
fn an_unowned_file_in_the_way_stops_the_switch_before_anything_changes() {
    let _system = lock_system();
    let setup = Setup::new(&["lemon"], "");
    setup.lay_out();
    let stray = setup
        .machine
        .deployment(setup.machine.current().unwrap())
        .join("usr/bin/tree");
    setup.machine.store_writable(true);
    fs::write(&stray, "mine").unwrap();
    setup.machine.store_writable(false);
    setup.set_body(&["tree"], "");
    let deployments = setup.machine.deployments();
    let err = setup.fails(&["switch", "--noconfirm"]);
    assert!(err.contains("usr/bin/tree from tree exists on disk"), "{err}");
    assert_eq!(setup.machine.deployments(), deployments);
    assert!(installed("lemon").is_some(), "nothing was removed");
    assert_eq!(
        fs::read_to_string(&stray).unwrap(),
        "mine",
        "the user's file is untouched"
    );
    setup.machine.store_writable(true);
    fs::remove_file(&stray).unwrap();
    setup.machine.store_writable(false);
    setup.ok(&["switch", "--noconfirm"]);
    assert!(installed("tree").is_some());
}

#[test]
#[ignore]
fn a_switch_without_systemd_boot_or_with_the_esp_in_boot_is_refused_before_anything_changes() {
    let _system = lock_system();
    let setup = Setup::new(&[], r#"boot = { esp = "/efi" }"#);
    let err = setup.fails(&["switch", "--noconfirm"]);
    assert!(err.contains("boot.loader = \"systemd-boot\""), "{err}");
    let esp_in_boot = format!(
        r#"boot = {{ loader = "systemd-boot", esp = "{}", rescue_entry = false }}, kernel_params = {{ "root=/dev/vda2 rw" }}"#,
        setup.machine.root.join("boot/efi").display()
    );
    fs::create_dir_all(setup.machine.root.join("boot/efi")).unwrap();
    setup.set_body(&[], &esp_in_boot);
    let diff = setup.ok(&["diff"]);
    assert!(
        diff.contains("refuses to lay this system out") && diff.contains("in /boot"),
        "diff names what the switch would refuse:\n{diff}"
    );
    let err = setup.fails(&["switch", "--noconfirm"]);
    assert!(err.contains("in /boot") && err.contains("nothing was changed"), "{err}");
    assert!(!setup.machine.root.join("archix").exists());
    for removed in ["boot.snapshots", "boot.entries"] {
        let (group, key) = removed.split_once('.').unwrap();
        setup.write(
            "init.lua",
            &format!("return {{ packages = {{ \"base\" }}, {group} = {{ {key} = false }} }}"),
        );
        let err = setup.fails(&["eval"]);
        assert!(
            err.contains(&format!("`{removed}`: option was removed")) && err.contains("deployments.keep"),
            "{err}"
        );
    }
}

#[test]
#[ignore]
fn archive_mirrors_stand_in_when_the_archive_is_down() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    setup.lay_out();
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
    setup.lay_out();
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
    setup.lay_out();
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
/// Under /var/cache, which the staging root shares with the machine, unlike /run.
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
    setup.lay_out();
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

/// Installs lemon 3.54.0 from a later snapshot with plain pacman into the running deployment, the way `pacman -U` or yay would.
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

/// Plain pacman on the machine's running deployment as it is, read-only /usr and all, through `sudo` as yay runs it; returns whether it succeeded and what it printed.
fn plain_pacman(machine: &Machine, args: &[&str]) -> (bool, String) {
    let root = machine.root.display().to_string();
    let output = Command::new("sudo")
        .args(["pacman", "--root", &root, "--dbpath", &format!("{root}/var/lib/pacman")])
        .args(["--config", &format!("{root}/etc/pacman.conf")])
        .args(["--hookdir", &format!("{root}/etc/pacman.d/hooks")])
        .args(args)
        .arg("--noconfirm")
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), text)
}

#[test]
#[ignore]
fn plain_pacman_on_a_deployment_points_to_archix_and_reading_still_works() {
    let _system = lock_system();
    let setup = Setup::new(&["lemon"], "");
    let conf = setup.machine.root.join("etc/pacman.conf");
    let arch_default = fs::read_to_string(&conf).unwrap().replace("#CheckSpace", "CheckSpace");
    fs::write(&conf, arch_default).unwrap();
    setup.lay_out();
    let laid_out = fs::read_to_string(system_path("/etc/pacman.conf")).unwrap();
    assert!(
        !laid_out.lines().any(|line| line.trim() == "CheckSpace"),
        "alpm's space check would stop at the read-only /usr before the hook: {laid_out}"
    );
    let file = std::env::temp_dir().join("lemon-3.54.0-1-x86_64.pkg.tar.zst");
    if !file.exists() {
        assert!(
            archix::archive::Archive::new(archix::archive::ARCHIVE_URL)
                .fetch(LEMON_URL, &file)
                .unwrap()
        );
    }
    let status = Command::new("cp")
        .args(["-a", "/var/lib/pacman/sync"])
        .arg(setup.machine.root.join("var/lib/pacman/"))
        .status()
        .unwrap();
    assert!(status.success());
    for args in [&["-U", file.to_str().unwrap()][..], &["-R", "lemon"], &["-S", "tree"]] {
        let (ok, out) = plain_pacman(&setup.machine, args);
        assert!(!ok, "pacman {args:?} changed a deployment: {out}");
        assert!(
            out.contains("archix install <package>")
                && out.contains("archix upgrade")
                && out.contains("archix switch")
                && !out.contains("mounted read only"),
            "pacman {args:?}: {out}"
        );
    }
    assert_eq!(installed("lemon").as_deref(), Some("3.53.4-1"));
    assert!(installed("tree").is_none());

    let downloads = tempfile::tempdir().unwrap();
    fs::set_permissions(downloads.path(), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let cachedir = downloads.path().to_str().unwrap();
    for args in [
        &["-Q", "lemon"][..],
        &["-Qi", "lemon"],
        &["-Ss", "^lemon$"],
        &["-Si", "lemon"],
        &["-Sw", "--cachedir", cachedir, "lemon"],
    ] {
        let (ok, out) = plain_pacman(&setup.machine, args);
        assert!(ok && !out.contains("archix install"), "pacman {args:?}: {out}");
    }
    assert!(fs::read_dir(downloads.path()).unwrap().count() > 0, "-Sw downloaded");

    setup.set_body(&["lemon", "tree"], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert!(
        installed("tree").is_some(),
        "archix's own pacman runs in the staging root, which the hook lets through"
    );
}

#[test]
#[ignore]
fn manual_upgrades_are_recorded_and_adopted_as_pins() {
    let _system = lock_system();
    let setup = Setup::new(&["lemon"], "");
    setup.lay_out();
    assert_eq!(installed("lemon").as_deref(), Some("3.53.4-1"));
    assert!(
        system_path("/etc/pacman.d/hooks/archix.hook").is_file(),
        "the deployment has the pacman hook"
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
    setup.lay_out();
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
    setup.lay_out();
    upgrade_lemon_behind_archix(&setup.machine);
    let out = setup.answering("n\ny\n", &["switch"]);
    assert!(out.contains("downgrade  lemon 3.54.0-1 -> 3.53.4-1"), "{out}");
    assert_eq!(installed("lemon").as_deref(), Some("3.53.4-1"));
    assert!(setup.lock().pinned.is_empty());
}

#[test]
#[ignore]
fn packages_installed_with_pacman_are_offered_for_declaring() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    setup.lay_out();
    let installed_lua = setup.dir.path().join("lua/hosts/box/installed.lua");
    upgrade_lemon_behind_archix(&setup.machine);
    let out = setup.answering("n\ny\n", &["switch"]);
    assert!(
        out.contains("installed with pacman or an AUR helper but not declared: lemon"),
        "{out}"
    );
    assert!(installed("lemon").is_none(), "declining removes it");
    assert!(!installed_lua.exists());

    setup.machine.reboot(None);
    upgrade_lemon_behind_archix(&setup.machine);
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(out.contains("not declared: lemon"), "{out}");
    assert_eq!(archix::installed::read(&installed_lua).unwrap(), ["lemon"]);
    assert_eq!(
        installed("lemon").as_deref(),
        Some("3.54.0-1"),
        "declaring keeps the installed version"
    );
    assert_eq!(setup.lock().pinned, ["lemon"]);
}

#[test]
#[ignore]
fn unsafe_downgrades_ask_and_noconfirm_refuses_them() {
    let _system = lock_system();
    let body = r#"unsafe_downgrades = { lemon = { "3.54" } }, deployments = { keep = 1 }"#;
    let setup = Setup::new(&["lemon"], body);
    setup.lay_out();
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

    setup.machine.reboot(None);
    upgrade_lemon_behind_archix(&setup.machine);
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(installed("lemon").as_deref(), Some("3.54.0-1"), "--noconfirm adopts");
    let newest = setup.machine.current().unwrap();
    setup.machine.reboot(Some(&format!("archix-{newest}.conf")));
    setup.ok(&["gc"]);
    assert_eq!(
        setup.machine.deployments(),
        [newest],
        "the older deployments are dropped"
    );
    let err = setup.fails(&["rollback", "--noconfirm"]);
    assert!(err.contains("--allow-unsafe-downgrade"), "{err}");
    assert_eq!(installed("lemon").as_deref(), Some("3.54.0-1"));
    setup.ok(&["rollback", "--noconfirm", "--allow-unsafe-downgrade"]);
    assert_eq!(
        installed("lemon").as_deref(),
        Some("3.53.4-1"),
        "the rebuilt generation has its own lemon"
    );
}

#[test]
#[ignore]
fn bare_terms_search_and_install_the_picks_like_yay() {
    let _system = lock_system();
    let mut aur = LocalAur::new();
    aur.publish(TestPkg::new("tree-git", "2.3.2"));
    let setup = Setup::new(&[], &format!("aur = {{ url = {:?} }}", aur.url()));
    setup.lay_out();
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
    assert!(setup.machine.current().is_none(), "nothing was laid out");
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
    setup.lay_out();
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
    let deployments = setup.machine.deployments();
    let err = setup.fails(&["switch", "--noconfirm"]);
    std::process::Command::new("umount").arg(&store).status().unwrap();
    assert!(
        err.contains(&*store.to_string_lossy())
            && err.contains("nothing was changed")
            && !err.contains("downloading tree"),
        "{err}"
    );
    assert_eq!(setup.machine.deployments(), deployments);
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
        err.contains("archix recover` is gone") && err.contains("nothing to recover"),
        "{err}"
    );
    let err = setup.fails(&["rescue", "list"]);
    assert!(
        err.contains("runs in the rescue boot") && err.contains("archix-rescue.conf"),
        "{err}"
    );
    let err = setup.fails(&["rescue", "--root-dir", "/", "list"]);
    assert!(err.contains("the running system"), "{err}");
}

#[test]
#[ignore]
fn a_config_kept_in_etc_comes_along_into_every_deployment_a_switch_or_rollback_makes_current() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    let machine = &setup.machine;
    // Where archix keeps the config by default, in /etc, which each deployment has its own copy of.
    let config = machine.root.join("etc/archix");
    let copied = Command::new("cp")
        .args(["-a", "--no-target-directory"])
        .arg(setup.dir.path())
        .arg(&config)
        .status()
        .unwrap();
    assert!(copied.success());
    let archix = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_archix"))
            .arg("--config")
            .arg(&config)
            .args(["--host", "box"])
            .args(args)
            .envs(machine.env())
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "archix {args:?}:\n{text}");
    };
    archix(&["switch", "--noconfirm"]);
    machine.reboot(Some("archix-1.conf"));
    let lock_in = |n: u32| fs::read_to_string(machine.deployment(n).join("etc/archix/archix.lock")).unwrap();
    assert!(!lock_in(1).contains("\ntree = {"));

    archix(&["install", "tree", "--noconfirm"]);
    let new = machine.current().unwrap();
    assert!(
        lock_in(new).contains("\ntree = {"),
        "the new deployment boots with the lock its switch wrote"
    );
    assert!(
        fs::read_to_string(machine.deployment(new).join("etc/archix/lua/hosts/box/installed.lua"))
            .unwrap()
            .contains("\"tree\"")
    );
    machine.reboot(None);
    assert!(
        fs::read_to_string(config.join("archix.lock"))
            .unwrap()
            .contains("\ntree = {")
    );

    archix(&["rollback", "1", "--noconfirm"]);
    assert_eq!(machine.current(), Some(1));
    assert!(
        lock_in(1).contains("\ntree = {"),
        "a rollback leaves archix.lock as it is, also in the deployment it boots"
    );
    machine.reboot(None);
    assert!(installed("tree").is_none());
    archix(&["switch", "--noconfirm"]);
    assert!(installed("tree").is_some(), "the next switch returns to the config");
}

#[test]
#[ignore]
fn a_package_signed_by_a_disabled_key_stops_the_staged_transaction() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    setup.lay_out();
    let machine = &setup.machine;
    // As pacman-key --populate does to revoked packager keys, in the running deployment's keyring only.
    let gnupg = machine.root.join("etc/pacman.d/gnupg");
    let script = format!(
        "h={}; for k in $(gpg --homedir \"$h\" --batch --with-colons --list-keys | awk -F: '$1 == \"pub\" {{ getline; print $10 }}'); do \
         printf 'disable\\nsave\\n' | gpg --homedir \"$h\" --batch --command-fd 0 --edit-key \"$k\" > /dev/null 2>&1 || true; done; \
         gpgconf --homedir \"$h\" --kill all",
        gnupg.display()
    );
    let disabled = Command::new("bash").args(["-c", &script]).status().unwrap();
    assert!(disabled.success());
    let (deployments, current) = (machine.deployments(), machine.current());
    setup.set_body(&["tree"], "");
    let err = setup.fails(&["switch", "--noconfirm"]);
    assert!(
        err.contains("in the staging root"),
        "the staged pacman took tree: {err}"
    );
    assert_eq!(machine.deployments(), deployments, "{err}");
    assert_eq!(machine.current(), current);
    assert!(machine.leftovers().is_empty(), "{:?}", machine.leftovers());
}

/// How many entries the root's own `at` directory holds under whatever is mounted there, through a non-recursive bind of the root.
fn hidden_entries(root: &Path, at: &str) -> usize {
    let script = format!(
        "d=$(mktemp -d) && mount --bind {root} \"$d\" && ls -A \"$d/{at}\" | wc -l; umount \"$d\"; rmdir \"$d\"",
        root = root.display()
    );
    let out = Command::new("unshare")
        .args(["-m", "--propagation", "private", "sh", "-c", &script])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

#[test]
#[ignore]
fn the_first_layout_leaves_what_its_binds_hide_for_the_next_boot_to_prune() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    let machine = &setup.machine;
    setup.ok(&["switch", "--noconfirm"]);
    for at in ["usr", "etc"] {
        assert!(
            hidden_entries(&machine.root, at) > 0,
            "the first switch pruned the live /{at}, which running services may still use"
        );
    }
    machine.reboot(Some("archix-1.conf"));
    setup.ok(&["boot-check"]);
    for at in ["usr", "etc", "opt", "boot", "var/lib/pacman/local"] {
        assert_eq!(hidden_entries(&machine.root, at), 0, "the boot didn't prune /{at}");
    }
    assert!(
        machine.root.join("usr/bin/pacman").is_file(),
        "deployment 1 is untouched"
    );
}

#[test]
#[ignore]
fn keep_one_never_drops_the_deployment_the_boot_mounted_when_its_entry_is_unknown() {
    let _system = lock_system();
    let keep = "deployments = { keep = 1 }";
    let setup = Setup::new(&["tree"], keep);
    setup.lay_out();
    let machine = &setup.machine;
    let running = machine.current().unwrap();
    let out = machine.reboot(None);
    assert!(
        out.contains(&format!("booting deployment {running} (current)")),
        "{out}"
    );
    setup.set_body(&[], keep);
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(
        machine.deployments().contains(&running),
        "keep = 1 dropped deployment {running}, which this boot runs:\n{out}"
    );
    setup.ok(&["gc"]);
    assert!(machine.deployments().contains(&running));
    assert_bootable(machine, "after gc");
}

#[test]
#[ignore]
fn a_commit_that_fails_after_the_rename_keeps_neither_the_deployment_nor_its_record() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    setup.lay_out();
    let machine = &setup.machine;
    let records = || -> Vec<u32> {
        archix::generations::Store::new(&Paths::system())
            .list()
            .unwrap()
            .iter()
            .map(|g| g.number)
            .collect()
    };
    let (deployments, generations, current) = (machine.deployments(), records(), machine.current());
    setup.set_body(&["tree"], "");
    let failed = setup.run_env(&[("ARCHIX_TEST_FAIL", "deploy-esp-entry")], &["switch", "--noconfirm"]);
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(
        !failed.status.success() && stderr.contains("failing at deploy-esp-entry"),
        "{stderr}"
    );
    assert_eq!(
        machine.deployments(),
        deployments,
        "a deployment without an entry was kept"
    );
    assert_eq!(records(), generations, "the failed switch saved a record");
    assert_eq!(machine.current(), current);
    assert_bootable(machine, "after the failed commit");
    setup.ok(&["switch", "--noconfirm"]);
    assert!(installed("tree").is_some());
    assert!(machine.leftovers().is_empty(), "{:?}", machine.leftovers());
    assert_bootable(machine, "after the next switch");
}

#[test]
#[ignore]
fn archix_writes_its_config_into_the_deployment_waiting_for_the_next_boot_too() {
    let _system = lock_system();
    let mut setup = Setup::new(&[], "");
    setup.lay_out();
    setup.move_config_to_etc();
    let init = setup.read("init.lua").unwrap();
    setup.write(
        "init.lua",
        &init.replacen("packages = { ", "packages = { \"tree\", ", 1),
    );
    let out = setup.ok(&["switch", "--noconfirm"]);
    let machine = &setup.machine;
    let pending = machine.current().unwrap();
    assert!(
        out.contains(&format!(
            "deployment {pending} is committed and boots at the next restart; no /etc merge is running, so changes to /etc made before then don't reach it"
        )) && !out.contains("carried over"),
        "a test root gets no /etc merge, so nothing may claim one carries changes: {out}"
    );
    let status = setup.ok(&["status"]);
    assert!(
        status.contains(&format!(
            "deployment {pending} is committed and boots at the next restart; no /etc merge is running"
        )) && !status.contains("carried over"),
        "{status}"
    );

    let init = setup.read("init.lua").unwrap();
    setup.write(
        "init.lua",
        &init.replacen("packages = { ", "packages = { \"lemon\", ", 1),
    );
    setup.ok(&["update", "--to", IMAGE_SNAPSHOT, "--noconfirm"]);
    assert!(setup.read("archix.lock").unwrap().contains("\nlemon = {"));
    let staged = fs::read_to_string(machine.deployment(pending).join("etc/archix/archix.lock")).unwrap();
    assert!(
        staged.contains("\nlemon = {"),
        "deployment {pending} boots without the update made after its switch"
    );

    machine.reboot(Some(&format!("archix-{pending}.conf")));
    assert!(
        !setup
            .ok(&["status"])
            .contains("is committed and boots at the next restart")
    );
}

#[test]
#[ignore]
fn a_crash_right_after_the_commit_still_boots_the_config_the_switch_wrote() {
    let _system = lock_system();
    let mut setup = Setup::new(&[], "");
    setup.lay_out();
    setup.move_config_to_etc();
    let killed = setup.run_env(
        &[("ARCHIX_TEST_CRASH", "deploy-done")],
        &["install", "tree", "--noconfirm"],
    );
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&killed.status),
        Some(libc::SIGKILL),
        "{}",
        String::from_utf8_lossy(&killed.stderr)
    );
    let new = setup.machine.current().unwrap();
    let etc = setup.machine.deployment(new).join("etc/archix");
    assert!(
        fs::read_to_string(etc.join("archix.lock"))
            .unwrap()
            .contains("\ntree = {"),
        "deployment {new} boots with a lock that lacks what its switch installed"
    );
    assert!(
        fs::read_to_string(etc.join("lua/hosts/box/installed.lua"))
            .unwrap()
            .contains("\"tree\"")
    );
}

#[test]
#[ignore]
fn a_kept_deployment_cant_be_changed_through_archix_and_a_rollback_repairs_one_that_was() {
    let _system = lock_system();
    let setup = Setup::new(&["tree"], "");
    setup.lay_out();
    let machine = &setup.machine;
    let with_tree = machine.current().unwrap();
    let usr = machine.deployment(with_tree).join("usr");
    let tree = usr.join("bin/tree");
    let before = identity(&usr.join("bin"));
    for args in [
        vec!["chmod", "-R", "700"],
        vec!["chown", "-R", "nobody"],
        vec!["setfacl", "-R", "-m", "u:nobody:rwx"],
    ] {
        let out = Command::new(args[0])
            .args(&args[1..])
            .arg(usr.join("bin"))
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success() && err.contains("Read-only file system"),
            "{args:?}: {err}"
        );
    }
    let out = Command::new("setcap")
        .arg("cap_net_raw+ep")
        .arg(&tree)
        .output()
        .unwrap();
    assert!(!out.status.success(), "setcap through /archix");
    assert_eq!(identity(&usr.join("bin")), before, "a change got through /archix");
    let out = setup.ok(&["verify"]);
    assert!(
        out.contains(&format!("deployment {with_tree}: matches its packages")),
        "{out}"
    );

    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    machine.reboot(None);
    machine.store_writable(true);
    fs::set_permissions(&tree, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    machine.store_writable(false);
    let out = setup.run(&["verify"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("/usr/bin/tree (tree): mode 700, not 755"), "{text}");
    assert!(
        setup
            .ok(&["status"])
            .contains(&format!("deployment {with_tree} differs from its packages in 1 files")),
        "status shows the last check"
    );
    let out = setup.ok(&["rollback", &with_tree.to_string(), "--noconfirm"]);
    assert!(out.contains("repaired 1 files"), "{out}");
    assert_eq!(fs::metadata(&tree).unwrap().mode() & 0o7777, 0o755);
    assert!(
        setup
            .ok(&["verify"])
            .contains(&format!("deployment {with_tree}: matches its packages"))
    );
    assert_eq!(machine.current(), Some(with_tree));
}

#[test]
#[ignore]
fn a_boot_of_an_older_entry_keeps_the_newer_deployment_and_its_updates_wait_in_a_login_note() {
    let _system = lock_system();
    let keep = "deployments = { keep = 1 }";
    let setup = Setup::new(&[], keep);
    setup.lay_out();
    let machine = &setup.machine;
    let old = machine.current().unwrap();
    setup.set_body(&["tree"], keep);
    setup.ok(&["switch", "--noconfirm"]);
    let new = machine.current().unwrap();
    let waiting = format!("1 update is waiting for a reboot (deployment {new}): tree ");
    assert!(setup.ok(&["status"]).contains(&waiting), "{}", setup.ok(&["status"]));
    let notes = fs::read_to_string(setup.paths.notes()).unwrap_or_default();
    assert!(notes.contains(&waiting), "{notes}");

    let out = setup.ok(&["rollback", "--noconfirm"]);
    assert!(out.contains("nothing is reinstalled"), "{out}");
    assert_eq!(
        machine.current(),
        Some(old),
        "a rollback while {new} is pending goes back to the running one"
    );
    let notes = fs::read_to_string(setup.paths.notes()).unwrap_or_default();
    assert!(!notes.contains("waiting for a reboot"), "{notes}");
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(out.contains(&format!("from deployment {old}")), "{out}");
    let new = machine.current().unwrap();
    assert!(machine.installed_in(new, "tree").is_some());

    machine.reboot(Some(&format!("archix-{old}.conf")));
    setup.ok(&["boot-check"]);
    assert_eq!(
        machine.current(),
        Some(old),
        "the boot of the older entry made it current"
    );
    setup.ok(&["gc"]);
    assert!(
        machine.deployments().contains(&new),
        "keep = 1 dropped deployment {new} after one boot of an older entry"
    );

    let state = Paths::system().state;
    fs::create_dir_all(state.join("pending-hooks")).unwrap();
    let flag = state.join("hook-may-pass");
    let _ = fs::remove_file(&flag);
    fs::write(
        state.join(format!("pending-hooks/{old}.json")),
        format!(r#"{{"hooks": {{"h": {{"run": "test -e {}"}}}}}}"#, flag.display()),
    )
    .unwrap();
    fs::create_dir_all(state.join("boot-tries")).unwrap();
    fs::write(state.join(format!("boot-tries/hooks-{old}")), "3\n").unwrap();
    let status = setup.ok(&["status"]);
    assert!(
        status.contains(&format!("deployment {old}'s hooks failed at 3 boots")),
        "{status}"
    );
    setup.fails(&["hooks", "retry"]);
    fs::write(&flag, "").unwrap();
    assert!(setup.ok(&["hooks", "retry"]).contains("hooks ran"));
    assert!(!state.join(format!("pending-hooks/{old}.json")).exists());
    fs::write(
        state.join(format!("pending-hooks/{old}.json")),
        r#"{"hooks": {"h": {"run": "false"}}}"#,
    )
    .unwrap();
    assert!(setup.ok(&["hooks", "skip"]).contains("dropped"));
    assert!(!state.join(format!("pending-hooks/{old}.json")).exists());
    let _ = fs::remove_file(&flag);

    setup.ok(&["rollback", &old.to_string(), "--noconfirm"]);
    setup.ok(&["gc"]);
    assert!(
        !machine.deployments().contains(&new),
        "an explicit rollback lets keep = 1 drop the newer deployment"
    );
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
fn what_the_machine_carries_but_the_config_doesnt_declare_is_named_before_and_after_switches_and_a_rollback() {
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

    setup.lay_out();
    setup.set_body(&["tree"], declared);
    setup.ok(&["switch", "--noconfirm"]);
    let second = machine.current().unwrap();
    machine.reboot(None);
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

    setup.ok(&["rollback", "--noconfirm"]);
    checks.check(machine.current() < Some(second), "the rollback went back");
    machine.reboot(None);
    checks.check(
        machine.root.join("opt/foo/bin/foo").is_file(),
        "/opt/foo was dropped by the rollback",
    );
    check(&setup.ok(&["diff", "--all"]), "after the rollback");
    checks.finish();
}

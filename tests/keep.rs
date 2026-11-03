//! `archix rollback N`, the keep: a snapshot made `@` from the next boot, with accounts carried and /var/lib put in step, and `--cancel`; on test machines on the whole rollback layout, rebooted by remounting; run only inside the Arch container via `tests/arch/run.sh`.

mod common;

use std::fs;
use std::path::Path;
use std::process::Command;

use archix::snapshot::{Kind, Store};
use common::*;

const ROLLBACKS: &str = "rollback = { enable = true }";

fn sh(script: &str) -> String {
    let output = Command::new("bash").args(["-ec", script]).output().unwrap();
    assert!(
        output.status.success(),
        "{script}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn on_layout(body: &str) -> Setup {
    Setup::on(Machine::on_layout("4G"), &[], body, &[])
}

fn top(machine: &Machine) -> &Path {
    &machine.btrfs.as_ref().unwrap().top
}

fn numbers(machine: &Machine) -> Vec<u32> {
    Store::open(&machine.root)
        .list()
        .unwrap()
        .iter()
        .map(|record| record.number)
        .collect()
}

fn in_root(machine: &Machine, command: &str) -> String {
    sh(&format!("chroot {} sh -c '{command}'", machine.root.display()))
}

fn entry(text: &str, name: &str) -> Option<String> {
    text.lines()
        .find(|line| line.split(':').next() == Some(name))
        .map(str::to_string)
}

#[test]
#[ignore]
fn keep_makes_the_switch_snapshot_at_by_name_from_the_next_boot_cancel_swaps_back_and_the_kept_tree_is_current() {
    let _system = lock_system();
    let checks = Checks::default();
    let setup = on_layout(&format!(
        r#"{ROLLBACKS}, files = {{ ["/etc/archix-keep-test"] = {{ text = "Y\n" }} }}"#
    ));
    let machine = &setup.machine;
    fs::write(machine.system_file("etc/fstab"), "UUID=abcd / btrfs rw,subvol=@ 0 0\n").unwrap();
    fs::write(
        machine.system_file("etc/kernel/cmdline"),
        "root=UUID=abcd rootflags=subvol=@ rw\n",
    )
    .unwrap();
    setup.first_switch();
    let tree_y = setup.ok(&["status"]);
    setup.set_body(
        &[],
        &format!(r#"{ROLLBACKS}, files = {{ ["/etc/archix-keep-test"] = {{ text = "X\n" }} }}"#),
    );
    let out = setup.ok(&["switch", "--noconfirm"]);
    let switch_snapshot = *numbers(machine).last().unwrap();
    checks.check(
        out.contains(&format!("snapshot {switch_snapshot} holds the system")),
        format_args!("the switch to X took snapshot {switch_snapshot}: {out}"),
    );
    let fstab = fs::read_to_string(machine.system_file("etc/fstab")).unwrap();
    let default = sh(&format!("btrfs subvolume get-default {}", top(machine).display()));

    let kept = setup.ok(&["rollback", &switch_snapshot.to_string(), "--noconfirm"]);
    let replaced = switch_snapshot + 1;
    checks.check(
        kept.contains(&format!("the running system is snapshot {replaced}")) && kept.contains("are lost"),
        format_args!("keep names the replaced root and says what's lost: {kept}"),
    );
    checks.eq(
        fs::read_to_string(top(machine).join("@/etc/archix-keep-test")).ok(),
        Some("Y\n".to_string()),
        "@ is the snapshot's clone",
    );
    checks.eq(
        fs::read_to_string(machine.system_file("etc/archix-keep-test")).ok(),
        Some("X\n".to_string()),
        "the running root stays as it was until the reboot",
    );
    checks.eq(
        Store::open(&machine.root).get(replaced).unwrap().map(|r| r.kind),
        Some(Kind::Replaced),
        "the running root is filed as replaced",
    );
    checks.eq(
        sh(&format!("btrfs subvolume get-default {}", top(machine).display())),
        default.clone(),
        "no set-default",
    );
    checks.eq(
        fs::read_to_string(top(machine).join("@/etc/fstab")).ok(),
        Some(fstab.clone()),
        "fstab still mounts subvol=@ by name",
    );
    let refused = setup.fails(&["switch", "--noconfirm"]);
    checks.check(
        refused.contains(&format!("rollback to snapshot {switch_snapshot} is pending"))
            && refused.contains("archix rollback --cancel"),
        format_args!("a switch refuses while the keep is pending: {refused}"),
    );
    let again = setup.fails(&["rollback", "1", "--noconfirm"]);
    checks.check(
        again.contains("pending already"),
        format_args!("a second keep refuses: {again}"),
    );
    let status = setup.ok(&["status"]);
    checks.check(
        status.contains(&format!("rollback to snapshot {switch_snapshot} is pending"))
            && status.contains("/etc edits made before that reboot are lost")
            && !status.contains("isn't the one archix last produced"),
        format_args!("status shows the pending keep, not a foreign root: {status}"),
    );

    let cancelled = setup.ok(&["rollback", "--cancel"]);
    checks.check(cancelled.contains("cancelled"), format_args!("--cancel: {cancelled}"));
    checks.eq(
        fs::read_to_string(top(machine).join("@/etc/archix-keep-test")).ok(),
        Some("X\n".to_string()),
        "--cancel swaps the running root back into @",
    );
    checks.check(
        !numbers(machine).contains(&replaced),
        format_args!("and the replaced snapshot is gone: {:?}", numbers(machine)),
    );
    checks.check(
        !machine.system_file("run/archix/keep-pending").exists(),
        "nothing is pending",
    );
    checks.check(
        !setup.ok(&["status"]).contains("isn't the one archix last produced"),
        "the root is archix's own again",
    );
    let refused = setup.run(&["rollback", "--cancel"]);
    checks.check(!refused.status.success(), "a second --cancel has nothing to do");

    setup.ok(&["rollback", &switch_snapshot.to_string(), "--noconfirm"]);
    machine.reboot();
    checks.eq(
        fs::read_to_string(machine.system_file("etc/archix-keep-test")).ok(),
        Some("Y\n".to_string()),
        "after the reboot the root, mounted by the name @, is the kept snapshot",
    );
    let status = setup.ok(&["status"]);
    let current_y = tree_y
        .lines()
        .find(|line| line.starts_with("current generation:"))
        .unwrap()
        .to_string();
    checks.check(
        status.lines().any(|line| line == current_y) && current_y.contains("tree "),
        format_args!("status reports the kept tree Y as current ({current_y}): {status}"),
    );
    checks.check(
        !status.contains("pending") && !status.contains("isn't the one archix last produced"),
        format_args!("and nothing pending or foreign: {status}"),
    );
    checks.check(
        setup
            .ok(&["generations"])
            .lines()
            .any(|line| line.trim_start().starts_with("2 ")),
        "the generation the replaced root was at stays listed",
    );
    setup.ok(&["switch", "--noconfirm"]);
    checks.eq(
        machine.current(),
        Some(3),
        "the next switch takes a new number, not the replaced root's",
    );
    checks.finish();
}

#[test]
#[ignore]
fn keep_carries_accounts_made_since_keeps_revoked_passwords_and_brings_deleted_people_back_locked() {
    let _system = lock_system();
    let checks = Checks::default();
    let setup = on_layout(&format!(r#"{ROLLBACKS}, users = {{ bob = {{}}, erin = {{}} }}"#));
    let machine = &setup.machine;
    setup.first_switch();
    in_root(
        machine,
        "useradd -m -s /bin/bash dave && useradd -m -s /bin/bash alice && echo alice:pw1 | chpasswd",
    );
    let out = setup.ok(&["snapshot", "create"]);
    let number = *numbers(machine).last().unwrap();
    checks.check(out.contains(&format!("snapshot {number} taken")), out);
    in_root(
        machine,
        "useradd -m -s /bin/bash carol && echo carol:pw2 | chpasswd && useradd -r -s /usr/bin/nologin svc && userdel dave && userdel erin && usermod -s /bin/sh bob && usermod -s /bin/sh alice && passwd -l alice",
    );
    let shadow_alice = entry(&fs::read_to_string(machine.system_file("etc/shadow")).unwrap(), "alice").unwrap();
    let kept = setup.ok(&["rollback", &number.to_string(), "--noconfirm"]);
    checks.check(
        kept.contains("dave was deleted since") && kept.contains("archix user unlock erin"),
        format_args!("keep names the placeholders: {kept}"),
    );
    machine.reboot();
    let passwd = fs::read_to_string(machine.system_file("etc/passwd")).unwrap();
    let shadow = fs::read_to_string(machine.system_file("etc/shadow")).unwrap();
    checks.check(
        entry(&passwd, "carol").is_some_and(|line| line.ends_with(":/bin/bash"))
            && entry(&shadow, "carol").is_some_and(|line| line.starts_with("carol:$")),
        format_args!("a user created since comes with its password: {passwd}"),
    );
    checks.check(
        entry(&passwd, "svc").is_some(),
        "a service user created since comes too",
    );
    checks.eq(
        entry(&passwd, "bob").map(|line| line.ends_with(":/bin/bash")),
        Some(true),
        "a declared user follows the snapshot's config",
    );
    checks.eq(
        entry(&passwd, "alice").map(|line| line.ends_with(":/bin/sh")),
        Some(true),
        "an undeclared one's change since is carried",
    );
    checks.eq(
        entry(&shadow, "alice"),
        Some(shadow_alice),
        "a revoked password stays revoked",
    );
    for name in ["dave", "erin"] {
        checks.check(
            entry(&passwd, name).is_some_and(|line| line.ends_with(":/usr/bin/nologin"))
                && entry(&shadow, name)
                    .is_some_and(|line| line.split(':').nth(1) == Some("!") && line.split(':').nth(7) == Some("1")),
            format_args!(
                "{name}, deleted since, is a locked placeholder: {:?} {:?}",
                entry(&passwd, name),
                entry(&shadow, name)
            ),
        );
    }
    let refused = setup.fails(&["user", "unlock", "dave"]);
    checks.check(
        refused.contains("isn't declared"),
        format_args!("an undeclared placeholder stays locked: {refused}"),
    );
    setup.ok(&["user", "unlock", "erin"]);
    let passwd = fs::read_to_string(machine.system_file("etc/passwd")).unwrap();
    let shadow = fs::read_to_string(machine.system_file("etc/shadow")).unwrap();
    checks.check(
        entry(&passwd, "erin").is_some_and(|line| !line.ends_with("nologin"))
            && entry(&shadow, "erin").is_some_and(|line| line.split(':').nth(7) == Some("")),
        format_args!(
            "unlock re-enables a declared one: {:?} {:?}",
            entry(&passwd, "erin"),
            entry(&shadow, "erin")
        ),
    );
    checks.finish();
}

#[test]
#[ignore]
fn keep_puts_package_owned_var_lib_files_back_moves_newer_packages_files_aside_and_their_reinstall_works() {
    let _system = lock_system();
    let checks = Checks::default();
    let packages = Packages::new();
    let state = |v: &str| {
        format!(
            "install -d \"$pkgdir/var/lib/archix-test-state\" && echo {v} > \"$pkgdir/var/lib/archix-test-state/data\" && echo {v} > \"$pkgdir/var/lib/archix-test-state/edited\""
        )
    };
    let v1 = packages.build_version("archix-test-state", "1", &state("1"));
    let v2 = packages.build_version("archix-test-state", "2", &state("2"));
    let late = packages.build(
        "archix-test-late",
        "install -d \"$pkgdir/var/lib/archix-test-late\" && echo late > \"$pkgdir/var/lib/archix-test-late/x\"",
    );
    let cached = Path::new("/var/cache/pacman/pkg").join(v1.file_name().unwrap());
    let _restore = RestoreFile::new(&cached);
    fs::copy(&v1, &cached).unwrap();
    let setup = on_layout(&format!(r#"{ROLLBACKS}, drift = "keep""#));
    let machine = &setup.machine;
    setup.first_switch();
    machine.pacman(&["-U", v1.to_str().unwrap()]);
    setup.ok(&["snapshot", "create"]);
    let number = *numbers(machine).last().unwrap();
    machine.pacman(&["-U", v2.to_str().unwrap(), late.to_str().unwrap()]);
    fs::write(machine.system_file("var/lib/archix-test-state/edited"), "mine\n").unwrap();
    let kept = setup.ok(&["rollback", &number.to_string(), "--noconfirm"]);
    let lib = machine.system_file("var/lib");
    checks.eq(
        fs::read_to_string(lib.join("archix-test-state/data")).ok(),
        Some("1\n".to_string()),
        "an unchanged file is put back as the snapshot's package has it",
    );
    checks.eq(
        fs::read_to_string(lib.join("archix-test-state/edited")).ok(),
        Some("mine\n".to_string()),
        "a changed one is left",
    );
    checks.check(
        kept.contains("/var/lib/archix-test-state/edited changed since"),
        format_args!("with a warning: {kept}"),
    );
    let aside = lib.join(format!(".archix-moved/{number}/archix-test-late/x"));
    checks.check(
        !lib.join("archix-test-late/x").exists() && fs::read_to_string(&aside).ok().as_deref() == Some("late\n"),
        format_args!("a newer package's file is moved to {}", aside.display()),
    );
    let status = setup.ok(&["status"]);
    checks.check(
        status.contains("files rollbacks moved aside"),
        format_args!("status shows the moved files' size: {status}"),
    );
    machine.reboot();
    checks.eq(
        machine.installed("archix-test-state"),
        Some("1-1".to_string()),
        "the kept root has the snapshot's packages",
    );
    checks.eq(machine.installed("archix-test-late"), None, "and not the newer one");
    machine.pacman(&["-U", late.to_str().unwrap()]);
    checks.eq(
        fs::read_to_string(lib.join("archix-test-late/x")).ok(),
        Some("late\n".to_string()),
        "reinstalling the newer package finds nothing in its way",
    );
    checks.finish();
}

#[test]
#[ignore]
fn cancel_puts_var_lib_back_as_it_was() {
    let _system = lock_system();
    let checks = Checks::default();
    let packages = Packages::new();
    let state = |v: &str| {
        format!(
            "install -d \"$pkgdir/var/lib/archix-test-state\" && echo {v} > \"$pkgdir/var/lib/archix-test-state/data\""
        )
    };
    let v1 = packages.build_version("archix-test-state", "1", &state("1"));
    let v2 = packages.build_version("archix-test-state", "2", &state("2"));
    let late = packages.build(
        "archix-test-late",
        "install -d \"$pkgdir/var/lib/archix-test-late\" && echo late > \"$pkgdir/var/lib/archix-test-late/x\"",
    );
    let cached = Path::new("/var/cache/pacman/pkg").join(v1.file_name().unwrap());
    let _restore = RestoreFile::new(&cached);
    fs::copy(&v1, &cached).unwrap();
    let setup = on_layout(&format!(r#"{ROLLBACKS}, drift = "keep""#));
    let machine = &setup.machine;
    setup.first_switch();
    machine.pacman(&["-U", v1.to_str().unwrap()]);
    setup.ok(&["snapshot", "create"]);
    let number = *numbers(machine).last().unwrap();
    machine.pacman(&["-U", v2.to_str().unwrap(), late.to_str().unwrap()]);
    setup.ok(&["rollback", &number.to_string(), "--noconfirm"]);
    setup.ok(&["rollback", "--cancel"]);
    let lib = machine.system_file("var/lib");
    checks.eq(
        fs::read_to_string(lib.join("archix-test-state/data")).ok(),
        Some("2\n".to_string()),
        "the restored file is as before",
    );
    checks.eq(
        fs::read_to_string(lib.join("archix-test-late/x")).ok(),
        Some("late\n".to_string()),
        "the moved one is back",
    );
    checks.check(
        !lib.join(".archix-moved").join(number.to_string()).exists(),
        "and the keep's directory is gone",
    );
    checks.finish();
}

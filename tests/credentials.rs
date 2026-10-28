//! Passwords, SSH keys, sudo rules and tmpfiles lines from the config; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

mod common;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};

use archix::system::password;
use common::*;

/// hunter2, yescrypt.
const ALICE: &str = "$y$j9T$u2f4nll1JxfH3hjxKg.8P1$iQmwHBe.iDFgnhAJVdpekC.aTQ8uIWtwoFNGILtAHV9";
/// correct-horse, yescrypt.
const ALICE2: &str = "$y$j9T$9BzPOKqucY3iBxC49Aj8f/$1BeiZ74./.58vLhZGjTfoNfQKFtc62kYIzXNggKHQiD";
/// hunter2, SHA-512.
const BOB: &str =
    "$6$7NaGYGLemRAuL7Ox$CWh388jGyxO3sNyFQ0DALQEpj8Fc8AiKYTtWx./.uAa2zzfv6LqWJApTfEeTpr6zlVEJdRTBs/aAjCAONSPsP0";
/// other-pass, yescrypt: what passwd sets on the running system.
const OTHER: &str = "$y$j9T$ILP7Vc892RE1zeViK5cKc1$oXtlyB41EDdR32sGMb9qHfOLNIR4JWqpHxCvirK.YMB";

fn users(alice: &str, extra: &str) -> String {
    format!(
        r#"users = {{ alice = {{ groups = {{ "wheel" }}, hashed_password = "{alice}" }}, bob = {{ initial_hashed_password = "{BOB}" }} }}{extra}"#
    )
}

/// `user`'s hash in deployment `number`'s /etc/shadow.
fn hash(machine: &Machine, number: u32, user: &str) -> String {
    let shadow = fs::read_to_string(machine.deployment(number).join("etc/shadow")).unwrap();
    shadow
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{user}:")))
        .and_then(|rest| rest.split(':').next())
        .unwrap_or_default()
        .to_string()
}

/// Boots the current deployment, as a plain restart does.
fn boot(setup: &Setup) {
    let current = setup.machine.current().unwrap();
    setup.machine.reboot(Some(&format!("archix-{current}.conf")));
}

fn no_hash_in(c: &Checks, out: &str, what: &str) {
    c.check(
        !out.contains("$y$") && !out.contains("$6$"),
        format!("{what} prints no hash:\n{out}"),
    );
}

#[test]
#[ignore]
fn declared_passwords_are_enforced_but_never_undo_a_lock_and_a_rollback_keeps_the_running_ones() {
    let _system = lock_system();
    let c = Checks::default();
    let setup = Setup::new(&[], &users(ALICE, ""));
    let machine = &setup.machine;
    let out = setup.lay_out();
    c.check(
        out.contains("password   alice gets the declared password")
            && out.contains("password   bob gets its initial password"),
        format!("the first switch says what it sets:\n{out}"),
    );
    no_hash_in(&c, &out, "the switch");
    let first = machine.current().unwrap();
    c.eq(
        hash(machine, first, "alice"),
        ALICE.to_string(),
        "alice's declared hash",
    );
    c.eq(hash(machine, first, "bob"), BOB.to_string(), "bob's initial hash");
    c.eq(
        password::verify(b"hunter2", &hash(machine, first, "alice")),
        Some(true),
        "alice logs in with the declared password",
    );
    c.check(
        archix::system::rescue_auth::accounts(&machine.deployment(first).join("etc"))
            .unwrap()
            .iter()
            .any(|account| account.user == "alice"),
        "alice, in wheel, opens the rescue entry",
    );

    let live = machine.root.join("etc");
    password::set(&live, "alice", OTHER).unwrap();
    password::set(&live, "bob", OTHER).unwrap();
    let diff = setup.ok(&["diff"]);
    c.check(
        diff.contains("password   resetting alice's password to the declared one") && !diff.contains("bob"),
        format!("diff announces the reset of the enforced password only:\n{diff}"),
    );
    no_hash_in(&c, &diff, "diff");
    let out = setup.ok(&["switch", "--noconfirm"]);
    c.check(out.contains("resetting alice's password"), out.to_string());
    let second = machine.current().unwrap();
    c.eq(
        hash(machine, second, "alice"),
        ALICE.to_string(),
        "passwd's change goes back to the declared hash",
    );
    c.eq(
        hash(machine, second, "bob"),
        OTHER.to_string(),
        "an initial password is bob's own once set",
    );
    boot(&setup);

    password::set(&live, "alice", &format!("!{ALICE}")).unwrap();
    setup.set_body(&[], &users(ALICE2, ""));
    let out = setup.ok(&["switch", "--noconfirm"]);
    c.check(
        out.contains("alice's account is locked or expired, so archix keeps it that way"),
        format!("the switch says it keeps the lock:\n{out}"),
    );
    let third = machine.current().unwrap();
    c.eq(
        hash(machine, third, "alice"),
        format!("!{ALICE}"),
        "a lock made with passwd -l stays",
    );
    boot(&setup);
    let expiry = fs::read_to_string(live.join("shadow"))
        .unwrap()
        .replace(&format!("alice:!{ALICE}"), &format!("alice:{ALICE}"));
    let expired: String = expiry
        .lines()
        .map(|line| match line.starts_with("alice:") {
            true => {
                let mut fields: Vec<&str> = line.split(':').collect();
                fields[7] = "1";
                fields.join(":")
            }
            false => line.to_string(),
        })
        .map(|line| line + "\n")
        .collect();
    fs::write(live.join("shadow"), expired).unwrap();
    setup.ok(&["switch", "--noconfirm"]);
    c.eq(
        hash(machine, machine.current().unwrap(), "alice"),
        ALICE.to_string(),
        "an expired account isn't given the declared password either",
    );
    boot(&setup);

    password::set(&live, "alice", ALICE).unwrap();
    let unexpired = fs::read_to_string(live.join("shadow"))
        .unwrap()
        .lines()
        .map(|line| match line.starts_with("alice:") {
            true => {
                let mut fields: Vec<&str> = line.split(':').collect();
                fields[7] = "";
                fields.join(":")
            }
            false => line.to_string(),
        })
        .map(|line| line + "\n")
        .collect::<String>();
    fs::write(live.join("shadow"), unexpired).unwrap();
    setup.ok(&["switch", "--noconfirm"]);
    let unlocked = machine.current().unwrap();
    c.eq(
        hash(machine, unlocked, "alice"),
        ALICE2.to_string(),
        "once unlocked, the next switch applies it",
    );
    boot(&setup);

    let rolled = setup.ok(&["rollback", &second.to_string(), "--noconfirm"]);
    c.eq(
        hash(machine, second, "alice"),
        ALICE2.to_string(),
        format!("a rollback carries the running password, not the older deployment's declared one:\n{rolled}"),
    );
    no_hash_in(&c, &rolled, "the rollback");

    let before = machine.deployments();
    setup.set_body(
        &[],
        &format!(r#"users = {{ alice = {{ groups = {{}}, hashed_password = "{ALICE2}" }}, bob = {{ initial_hashed_password = "{BOB}" }} }}"#),
    );
    let refused = setup.fails(&["switch", "--noconfirm"]);
    c.check(
        refused.contains("leaves no account that can log in with a password"),
        format!("taking the last password holder out of wheel, with root locked, is refused:\n{refused}"),
    );
    c.eq(
        machine.current(),
        Some(second),
        "nothing changed after the wheel removal: current",
    );
    c.eq(
        machine.deployments(),
        before.clone(),
        "nothing changed after the wheel removal: deployments",
    );

    setup.set_body(&[], &users("!", ""));
    let refused = setup.fails(&["switch", "--noconfirm"]);
    c.check(
        refused.contains("leaves no account that can log in with a password")
            && refused.contains("allow_no_password_login"),
        format!("locking the last login is refused:\n{refused}"),
    );
    c.eq(machine.current(), Some(second), "nothing changed: current");
    c.eq(machine.deployments(), before, "nothing changed: deployments");
    c.eq(machine.leftovers(), Vec::<String>::new(), "nothing changed: staging");
    setup.set_body(&[], &users("!", ", allow_no_password_login = true"));
    setup.ok(&["switch", "--noconfirm"]);
    c.eq(
        hash(machine, machine.current().unwrap(), "alice"),
        "!".to_string(),
        "acknowledged, the lock is declared",
    );
    let quiet = setup.ok(&["status"]);
    c.check(
        !quiet.contains("no account can log in to the rescue entry"),
        format!("acknowledged, status doesn't warn:\n{quiet}"),
    );
    setup.set_body(&[], &users("!", ""));
    let warned = setup.ok(&["status"]);
    c.check(
        warned.contains("no account can log in to the rescue entry") && warned.contains("live USB"),
        format!("with no account the rescue entry can use, status keeps warning:\n{warned}"),
    );
    c.finish();
}

fn rescue_password(setup: &Setup, user: &str, password: &str) -> String {
    let root = setup.machine.root.display().to_string();
    let esp = setup.machine.esp().display().to_string();
    setup.answering(
        &format!("{password}\n{password}\n"),
        &["rescue", "--root-dir", &root, "--esp-dir", &esp, "password", user],
    )
}

#[test]
#[ignore]
fn a_password_set_in_the_rescue_holds_until_the_declared_hash_changes() {
    let _system = lock_system();
    let c = Checks::default();
    let setup = Setup::new(&[], &users(ALICE, ""));
    let machine = &setup.machine;
    setup.lay_out();
    machine.power_off();
    let out = rescue_password(&setup, "alice", "rescued pass");
    c.check(
        out.contains("switches keep it over users.alice.hashed_password"),
        out.to_string(),
    );
    let notes = fs::read_to_string(machine.root.join("var/lib/archix/notes")).unwrap_or_default();
    c.check(
        notes.contains("alice's password was set in the rescue entry"),
        format!("a login note:\n{notes}"),
    );
    boot(&setup);

    setup.set_body(
        &[],
        &users(ALICE, r#", files = { ["/etc/archix-test.conf"] = { text = "x" } }"#),
    );
    let out = setup.ok(&["switch", "--noconfirm"]);
    c.check(
        out.contains("keeping alice's password set in the rescue entry") && !out.contains("resetting alice's"),
        out.to_string(),
    );
    let kept = machine.current().unwrap();
    c.eq(
        password::verify(b"rescued pass", &hash(machine, kept, "alice")),
        Some(true),
        "the rescue password survives a switch",
    );
    boot(&setup);
    c.eq(
        password::verify(b"rescued pass", &hash(machine, kept, "alice")),
        Some(true),
        "and the boot after it",
    );

    setup.set_body(&[], &users(ALICE2, ""));
    let out = setup.ok(&["switch", "--noconfirm"]);
    c.check(
        out.contains("changed since the rescue entry set alice's password"),
        out.to_string(),
    );
    c.eq(
        hash(machine, machine.current().unwrap(), "alice"),
        ALICE2.to_string(),
        "a changed declared hash replaces it",
    );
    c.check(
        !machine
            .root
            .join("var/lib/archix/accounts/password-override/alice")
            .exists(),
        "the override is forgotten",
    );
    c.finish();
}

fn sh(script: &str) -> String {
    let output = Command::new("bash").args(["-ec", script]).output().unwrap();
    let text = String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{script}\n{text}");
    text
}

/// sshd from the machine's running deployment, in a chroot of its root on `port`.
fn sshd(root: &Path, port: u16) -> Child {
    let r = root.display();
    sh(&format!(
        "mountpoint -q {r}/dev || mount --bind /dev {r}/dev; mountpoint -q {r}/proc || mount -t proc proc {r}/proc; mkdir -p {r}/run/sshd; ssh-keygen -A -f {r}"
    ));
    let child = Command::new("chroot")
        .arg(root)
        .args([
            "/usr/bin/sshd",
            "-D",
            "-e",
            "-p",
            &port.to_string(),
            "-o",
            "ListenAddress=127.0.0.1",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    child
}

fn ssh_logs_in(key: &Path, user: &str, port: u16) -> bool {
    Command::new("ssh")
        .args(["-i", &key.display().to_string(), "-p", &port.to_string()])
        .args([
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "BatchMode=yes",
        ])
        .args(["-o", "IdentitiesOnly=yes", "-o", "ConnectTimeout=10"])
        .arg(format!("{user}@127.0.0.1"))
        .arg("true")
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn new_key(dir: &Path, name: &str) -> String {
    sh(&format!(
        "ssh-keygen -q -t ed25519 -N '' -C {name}@test -f {}/{name}",
        dir.display()
    ));
    fs::read_to_string(dir.join(format!("{name}.pub")))
        .unwrap()
        .trim()
        .to_string()
}

#[test]
#[ignore]
fn keys_sudo_rules_and_tmpfiles_go_into_the_deployment_and_bad_ones_change_nothing() {
    let _system = lock_system();
    let c = Checks::default();
    let keys = tempfile::tempdir().unwrap();
    let alice_key = new_key(keys.path(), "alice");
    new_key(keys.path(), "bob");
    let body = |rule: &str, tmpfiles: &str| {
        format!(
            r#"users = {{ alice = {{ groups = {{ "wheel" }}, hashed_password = "{ALICE}", ssh_authorized_keys = {{ "{alice_key}" }} }}, bob = {{ hashed_password = "{BOB}" }} }},
            sudo = {{ enable = true, extra_rules = {{ "{rule}" }} }},
            tmpfiles = {{ "{tmpfiles}" }}"#
        )
    };
    let good = body(
        "bob ALL=(ALL) NOPASSWD: /usr/bin/systemctl",
        "d /var/lib/archix-tmpfiles-test 0750 alice alice -",
    );
    let setup = Setup::new(&["sudo", "openssh"], &good);
    let machine = &setup.machine;
    setup.lay_out();
    let deployment = machine.deployment(machine.current().unwrap());
    let mode = |path: &str| fs::symlink_metadata(deployment.join(path)).map(|meta| (meta.mode() & 0o7777, meta.uid()));
    c.eq(
        mode("etc/ssh/authorized_keys.d/alice").ok(),
        Some((0o644, 0)),
        "the keys file is root's, 0644",
    );
    c.eq(
        mode("etc/sudoers.d/archix").ok(),
        Some((0o440, 0)),
        "the sudoers drop-in is root's, 0440",
    );
    c.check(
        fs::read_to_string(deployment.join("etc/sudoers.d/archix"))
            .unwrap_or_default()
            .contains("bob ALL=(ALL) NOPASSWD: /usr/bin/systemctl"),
        "the extra rule",
    );
    let root = machine.root.clone();
    let port = 2200 + (std::process::id() % 700) as u16;
    let mut daemon = sshd(&root, port);
    let checked = Command::new("chroot")
        .arg(&root)
        .args(["/usr/bin/sshd", "-t"])
        .output()
        .unwrap();
    let effective = Command::new("chroot")
        .arg(&root)
        .args(["/usr/bin/sshd", "-T", "-C", "user=alice,host=x,addr=127.0.0.1"])
        .output()
        .unwrap();
    c.check(
        checked.status.success(),
        format!("sshd -t passes: {}", String::from_utf8_lossy(&checked.stderr)),
    );
    c.check(
        String::from_utf8_lossy(&effective.stdout)
            .to_lowercase()
            .contains("authorizedkeysfile .ssh/authorized_keys /etc/ssh/authorized_keys.d/%u"),
        format!("{}", String::from_utf8_lossy(&effective.stdout)),
    );
    let bob_home = root.join("home/bob");
    let (uid, gid) = fs::metadata(&bob_home).map(|meta| (meta.uid(), meta.gid())).unwrap();
    fs::create_dir_all(bob_home.join(".ssh")).unwrap();
    fs::copy(keys.path().join("bob.pub"), bob_home.join(".ssh/authorized_keys")).unwrap();
    for path in [bob_home.join(".ssh"), bob_home.join(".ssh/authorized_keys")] {
        std::os::unix::fs::chown(&path, Some(uid), Some(gid)).unwrap();
    }
    fs::set_permissions(bob_home.join(".ssh"), fs::Permissions::from_mode(0o700)).unwrap();
    c.check(
        ssh_logs_in(&keys.path().join("alice"), "alice", port),
        "alice logs in with her declared key",
    );
    c.check(
        ssh_logs_in(&keys.path().join("bob"), "bob", port),
        "bob, with keys only in ~/.ssh/authorized_keys, still logs in",
    );
    c.check(
        !ssh_logs_in(&keys.path().join("bob"), "alice", port),
        "bob's key isn't alice's",
    );
    let _ = daemon.kill();
    let _ = daemon.wait();

    let made = root.join("var/lib/archix-tmpfiles-test");
    let _ = fs::remove_dir(&made);
    sh(&format!(
        "systemd-tmpfiles --root={} --create archix.conf",
        root.display()
    ));
    let alice_uid: u32 = fs::read_to_string(deployment.join("etc/passwd"))
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("alice:x:")?.split(':').next()?.parse().ok())
        .unwrap();
    c.eq(
        fs::metadata(&made).map(|meta| (meta.mode() & 0o7777, meta.uid())).ok(),
        Some((0o750, alice_uid)),
        "the boot's tmpfiles run makes the directory, alice's and 0750",
    );

    let before = (machine.current(), machine.deployments());
    for (rule, tmpfiles, says) in [
        (
            "bob ALL=(ALL) NOPASSWD /usr/bin/systemctl",
            "d /var/lib/archix-tmpfiles-test 0750 alice alice -",
            "visudo refuses",
        ),
        (
            "bob ALL=(ALL) NOPASSWD: /usr/bin/systemctl",
            "d /var/lib/archix-tmpfiles-test 0750 nosuchuser - -",
            "systemd-tmpfiles refuses",
        ),
    ] {
        setup.set_body(&["sudo", "openssh"], &body(rule, tmpfiles));
        let refused = setup.fails(&["switch", "--noconfirm"]);
        c.check(
            refused.contains(says) && refused.contains("nothing was changed"),
            refused.to_string(),
        );
        c.eq(
            (machine.current(), machine.deployments()),
            before.clone(),
            format!("{says}: nothing changed"),
        );
        c.eq(
            machine.leftovers(),
            Vec::<String>::new(),
            format!("{says}: no staging left"),
        );
    }
    c.finish();
}

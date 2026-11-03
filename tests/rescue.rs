//! The rescue boot entry and `archix rescue`; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

mod common;

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use common::*;

fn text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr)
}

/// `archix rescue` on a machine whose root is mounted at its root dir and nothing runs from it.
fn rescue(setup: &Setup, args: &[&str]) -> Output {
    rescue_answering(setup, "", args)
}

fn rescue_answering(setup: &Setup, input: &str, args: &[&str]) -> Output {
    let root = setup.machine.root.display().to_string();
    let esp = setup.machine.esp().display().to_string();
    let mut all = vec!["rescue", "--root-dir", &root, "--esp-dir", &esp];
    all.extend(args);
    setup.answering_raw(input, &all)
}

fn ok(output: Output) -> String {
    let text = text(&output);
    assert!(output.status.success(), "archix rescue failed:\n{text}");
    text
}

fn root_hash(etc: &Path) -> String {
    let shadow = fs::read_to_string(etc.join("shadow")).unwrap();
    let line = shadow
        .lines()
        .find(|line| line.starts_with("root:"))
        .unwrap()
        .to_string();
    line.split(':').nth(1).unwrap().to_string()
}

#[test]
#[ignore]
fn the_rescue_checks_and_resets_a_password_on_a_root_nothing_runs_from_and_has_no_deployment_commands() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    setup.first_switch();
    let machine = &setup.machine;

    let refused = text(&rescue(&setup, &["check"]));
    assert!(refused.contains("needs the root and the ESP unmounted"), "{refused}");
    for gone in [
        &["list"][..],
        &["use", "1"],
        &["verify"],
        &["restore-etc", "/etc/nanorc"],
        &["eject", "--noconfirm"],
    ] {
        let output = rescue(&setup, gone);
        assert!(!output.status.success(), "{gone:?}: {}", text(&output));
    }

    let differ = rescue_answering(&setup, "one\ntwo\n", &["password", "root"]);
    assert!(
        !differ.status.success() && text(&differ).contains("differ"),
        "{}",
        text(&differ)
    );
    ok(rescue_answering(
        &setup,
        "s3cret pass\ns3cret pass\n",
        &["password", "root"],
    ));
    let hash = root_hash(&machine.root.join("etc"));
    let salt = hash.split('$').nth(2).unwrap();
    let openssl = Command::new("openssl")
        .args(["passwd", "-6", "-salt", salt, "s3cret pass"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&openssl.stdout).trim(),
        hash,
        "the root's own /etc/shadow has it"
    );
}

/// A loop device for an image file, detached when dropped.
struct Loop(String);

impl Loop {
    fn new(image: &Path) -> Loop {
        let out = Command::new("losetup")
            .args(["--show", "-f"])
            .arg(image)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", text(&out));
        Loop(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

impl Drop for Loop {
    fn drop(&mut self) {
        let _ = Command::new("losetup").args(["-d", &self.0]).status();
    }
}

fn u16_at(bytes: &[u8], at: usize) -> u64 {
    u64::from(u16::from_le_bytes([bytes[at], bytes[at + 1]]))
}

#[test]
#[ignore]
fn rescue_check_repairs_the_esp_and_refuses_a_mounted_root() {
    let dir = tempfile::tempdir().unwrap();
    let (root_img, esp_img) = (dir.path().join("root.img"), dir.path().join("esp.img"));
    for (image, mkfs) in [(&root_img, ["mkfs.ext4", "-q"]), (&esp_img, ["mkfs.fat", "-F32"])] {
        fs::File::create(image).unwrap().set_len(64 << 20).unwrap();
        let made = Command::new(mkfs[0]).arg(mkfs[1]).arg(image).output().unwrap();
        assert!(made.status.success(), "{}", text(&made));
    }
    // FAT32: the copies differ, as a write cut off between them leaves them.
    let mut fat = fs::read(&esp_img).unwrap();
    let reserved = u16_at(&fat, 0x0e);
    let per_fat = u64::from(u32::from_le_bytes(fat[0x24..0x28].try_into().unwrap()));
    let second = ((reserved + per_fat) * 512 + 4 * 100) as usize;
    fat[second..second + 4].copy_from_slice(&0x0fff_fff7u32.to_le_bytes());
    fs::write(&esp_img, &fat).unwrap();
    let (root, esp) = (Loop::new(&root_img), Loop::new(&esp_img));
    let check = || {
        Command::new(env!("CARGO_BIN_EXE_archix"))
            .args(["rescue", "--root", &root.0, "--esp", &esp.0, "check"])
            .output()
            .unwrap()
    };
    let first = check();
    let out = text(&first);
    assert!(first.status.success(), "{out}");
    assert!(
        out.contains(&format!(":: e2fsck -f -y {}", root.0))
            && out.contains(&format!("{} (ext4) is clean\n", root.0))
            && out.contains(&format!("{} (vfat) is clean now", esp.0)),
        "{out}"
    );
    let again = text(&check());
    assert!(
        again.contains(&format!("{} (vfat) is clean\n", esp.0)),
        "fsck.fat repaired it: {again}"
    );

    let at = dir.path().join("mnt");
    fs::create_dir(&at).unwrap();
    assert!(Command::new("mount").arg(&root.0).arg(&at).status().unwrap().success());
    let mounted = check();
    let _ = Command::new("umount").arg(&at).status();
    assert!(!mounted.status.success(), "{}", text(&mounted));
    assert!(
        text(&mounted).contains(&format!("is mounted at {}", at.display())),
        "{}",
        text(&mounted)
    );
}

/// `archix rescue` behind its login, as the rescue boot runs it, on a root nothing runs from; how long it took.
fn rescue_login(setup: &Setup, input: &str, args: &[&str]) -> (Output, std::time::Duration) {
    let root = setup.machine.root.display().to_string();
    let esp = setup.machine.esp().display().to_string();
    let started = std::time::Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_archix"))
        .args(["--config", setup.config.to_str().unwrap(), "--host", "box"])
        .args(["rescue", "--root-dir", &root, "--esp-dir", &esp])
        .args(args)
        .envs(setup.machine.env())
        .env("ARCHIX_TEST_RESCUE_LOGIN", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), input.as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    (output, started.elapsed())
}

/// `etc`'s file `name` with the lines starting `prefix` replaced by `line`.
fn set_line(etc: &Path, name: &str, prefix: &str, line: &str) {
    let path = etc.join(name);
    let text = fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = text
        .lines()
        .filter(|l| !l.starts_with(prefix))
        .map(str::to_string)
        .collect();
    lines.push(line.to_string());
    fs::write(&path, lines.join("\n") + "\n").unwrap();
}

/// chpasswd's yescrypt hash of "correct horse battery" on Arch, as libxcrypt writes it.
const WHEEL_HASH: &str = "$y$j9T$kMqcsJASkGTKWEBuc18hd1$2I2nh.vdW7uIPYraHA5FxOpLQZ5yNdM39PxvIPOsMJ7";

#[test]
#[ignore]
fn the_rescue_opens_only_for_root_or_a_wheel_users_password_from_the_roots_shadow() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    setup.first_switch();
    let etc = setup.machine.root.join("etc");
    let openssl = Command::new("openssl")
        .args(["passwd", "-6", "root pass"])
        .output()
        .unwrap();
    let root_hash = String::from_utf8_lossy(&openssl.stdout).trim().to_string();
    set_line(&etc, "shadow", "root:", &format!("root:{root_hash}:20000::::::"));
    set_line(&etc, "passwd", "alice:", "alice:x:1000:1000::/home/alice:/bin/bash");
    set_line(&etc, "passwd", "bob:", "bob:x:1001:1001::/home/bob:/bin/bash");
    set_line(&etc, "shadow", "alice:", &format!("alice:{WHEEL_HASH}:20000::::::"));
    set_line(&etc, "shadow", "bob:", &format!("bob:{WHEEL_HASH}:20000::::::"));
    set_line(&etc, "group", "wheel:", "wheel:x:998:alice");

    let out = ok(rescue_login(&setup, "root\nroot pass\n", &["shell"]).0);
    assert!(out.contains("logged in as root"), "root's password opens it:\n{out}");
    let out = ok(rescue_login(&setup, "alice\ncorrect horse battery\n", &["shell"]).0);
    assert!(
        out.contains("logged in as alice"),
        "a wheel user's password opens it:\n{out}"
    );

    let (refused, took) = rescue_login(
        &setup,
        "bob\ncorrect horse battery\nalice\nwrong\nroot\n\nalice\ncorrect horse battery\necho SHELL-RAN\n",
        &["shell"],
    );
    let out = text(&refused);
    assert!(!refused.status.success(), "{out}");
    assert!(
        out.matches("wrong user or password").count() == 3 && out.contains("3 wrong logins"),
        "bob isn't in wheel, a wrong password and none fail, and the fourth try never comes:\n{out}"
    );
    assert!(
        !out.contains("logged in") && !out.contains("SHELL-RAN") && !out.contains("a shell in"),
        "{out}"
    );
    assert!(
        took >= std::time::Duration::from_secs(6),
        "2 s and 4 s between the tries: {took:?}"
    );
    let (ended, _) = rescue_login(&setup, "", &["shell"]);
    assert!(
        !ended.status.success() && text(&ended).contains("no login"),
        "{}",
        text(&ended)
    );

    set_line(&etc, "shadow", "root:", "root:*:14871::::::");
    set_line(&etc, "shadow", "alice:", &format!("alice:!{WHEEL_HASH}:20000::::::"));
    let (locked, _) = rescue_login(&setup, "root\n\nalice\ncorrect horse battery\n", &["shell"]);
    let out = text(&locked);
    assert!(!locked.status.success(), "{out}");
    assert!(
        out.contains("root is locked and no wheel user has a password")
            && out.contains(archix::system::rescue_auth::GUIDE)
            && !out.contains("rescue login"),
        "a locked root and no wheel password: refused before any prompt:\n{out}"
    );
    let out = ok(rescue(&setup, &["shell"]));
    assert!(
        !out.contains("rescue login"),
        "outside the rescue boot, as from a live USB, there's no login:\n{out}"
    );
}

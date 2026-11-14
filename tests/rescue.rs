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
    // A rescue that stays closed exits before it reads everything typed ahead.
    match std::io::Write::write_all(&mut child.stdin.take().unwrap(), input.as_bytes()) {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
        written => written.unwrap(),
    }
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
        !out.contains("logged in") && !out.contains("SHELL-RAN") && !out.contains("shell in"),
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

/// The static build, which is what the rescue entry's image and a live USB run.
const STATIC: &str = "/target/x86_64-unknown-linux-musl/release/archix";

fn sh(script: &str) -> String {
    let output = Command::new("bash").args(["-ec", script]).output().unwrap();
    assert!(output.status.success(), "{script}\n{}", text(&output));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A btrfs on a loop device laid out as CachyOS's installer and then archix's rollbacks leave one (`@` and its siblings, @archix with snapshot 1 of a healthy root, @varlib), whose root then lost libz and every program with it; nothing of it is mounted.
struct BrokenRoot {
    dir: tempfile::TempDir,
    device: Loop,
    esp: Loop,
}

impl BrokenRoot {
    fn new() -> BrokenRoot {
        assert!(
            Path::new(STATIC).is_file(),
            "{STATIC} is missing; tests/arch/run.sh builds it"
        );
        let dir = tempfile::tempdir().unwrap();
        let (image, esp_image, top) = (
            dir.path().join("disk.img"),
            dir.path().join("esp.img"),
            dir.path().join("top"),
        );
        fs::create_dir(&top).unwrap();
        sh(&format!(
            "truncate -s 1G {image} && mkfs.btrfs -q {image} && truncate -s 64M {esp} && mkfs.fat -F32 {esp} > /dev/null",
            image = image.display(),
            esp = esp_image.display()
        ));
        let (device, esp) = (Loop::new(&image), Loop::new(&esp_image));
        sh(&format!(
            r#"t={top}; mount {device} $t
            for s in @ @home @root @srv @cache @tmp @log @archix @varlib; do btrfs -q subvolume create $t/$s; done
            r=$t/@
            mkdir -p $r/etc $r/usr/bin $r/usr/lib $r/var/lib $r/var/rollback/pacman/local $r/var/rollback/dkms \
              $r/var/rollback/archix $r/.archix $r/proc $r/sys $r/dev $r/run $r/efi $r/home $t/@archix/snapshots/1 \
              $t/@varlib/pacman $t/@varlib/dkms $t/@varlib/archix
            printf 'root:x:0:0::/root:/usr/bin/bash\n' > $r/etc/passwd
            printf 'root:!:20000::::::\n' > $r/etc/shadow; chmod 600 $r/etc/shadow
            printf 'root:x:0:\nwheel:x:998:\n' > $r/etc/group
            printf '{device} / btrfs rw,subvol=/@ 0 0\n{esp} /efi vfat umask=0077 0 2\n' > $r/etc/fstab
            echo libz > $r/usr/lib/libz.so.1
            for p in btrfs mount umount findmnt cp git sh bash chroot archix pacman; do printf '#!/bin/sh\necho ran\n' > $r/usr/bin/$p; chmod 755 $r/usr/bin/$p; done
            echo healthy > $r/etc/archix-test
            btrfs -q subvolume snapshot -r $r $t/@archix/snapshots/1/root
            printf '{{"format": 1, "number": 1, "kind": "switch", "created": 1, "generation": 7, "commit": "0123456789abcdef0123456789abcdef01234567", "label": "before the swap that removed libz"}}\n' > $t/@archix/snapshots/1/record.json"#,
            top = top.display(),
            device = device.0,
            esp = esp.0,
        ));
        archix::snapshot::record_root(&top.join("@archix"), &top).unwrap();
        // The swap: libz is gone, so nothing on the root starts, and the pacman that died left its lock.
        sh(&format!(
            r#"r={top}/@; rm $r/usr/lib/libz.so.1
            for p in $r/usr/bin/*; do echo 'error while loading shared libraries: libz.so.1' > $p; chmod 644 $p; done
            echo broken > $r/etc/archix-test; : > $r/var/rollback/pacman/db.lck
            umount {top}"#,
            top = top.display()
        ));
        BrokenRoot { dir, device, esp }
    }

    /// What `look` sees at the top level, mounted read-only for it.
    fn top<T>(&self, look: impl FnOnce(&Path) -> T) -> T {
        let top = self.dir.path().join("top");
        sh(&format!("mount -o ro,subvolid=5 {} {}", self.device.0, top.display()));
        let seen = look(&top);
        sh(&format!("umount {}", top.display()));
        seen
    }

    /// Whether `@` is the healthy snapshot again and the broken root is filed as snapshot 2 of kind replaced.
    fn assert_rolled_back(&self) {
        let (root, libz, replaced, record) = self.top(|top| {
            (
                fs::read_to_string(top.join("@/etc/archix-test")).unwrap_or_default(),
                top.join("@/usr/lib/libz.so.1").is_file(),
                fs::read_to_string(top.join("@archix/snapshots/2/root/etc/archix-test")).unwrap_or_default(),
                fs::read_to_string(top.join("@archix/snapshots/2/record.json")).unwrap_or_default(),
            )
        });
        assert_eq!(root, "healthy\n", "@ is snapshot 1's content");
        assert!(libz, "with its libz");
        assert_eq!(replaced, "broken\n", "the broken root is snapshot 2");
        assert!(record.contains("\"replaced\""), "of kind replaced: {record}");
        let left: Vec<_> = archix::conflicts::mount_points()
            .into_iter()
            .filter(|point| point.starts_with("/run/archix-rescue"))
            .collect();
        assert!(left.is_empty(), "the rescue unmounts what it mounted: {left:?}");
    }
}

/// The static archix with a PATH holding no program at all and the kernel command line of some other system, as a live USB's is, in a mount namespace of its own.
fn rescue_static(args: &[&str]) -> Output {
    use std::process::Stdio;
    let cmdline = tempfile::NamedTempFile::new().unwrap();
    fs::write(
        cmdline.path(),
        "root=/dev/archix-test-live rootflags=subvol=live rw quiet\n",
    )
    .unwrap();
    let empty = tempfile::tempdir().unwrap();
    let env = Command::new("which").arg("env").output().unwrap();
    let mut child = Command::new("unshare")
        .args(["-m", "sh", "-c"])
        .arg(r#"mount --bind "$0" /proc/cmdline && exec "$@""#)
        .arg(cmdline.path())
        .args([String::from_utf8_lossy(&env.stdout).trim(), "-i"])
        .arg(format!("PATH={}", empty.path().display()))
        .args([STATIC, "rescue"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The keep asks before it changes anything.
    let _ = std::io::Write::write_all(&mut child.stdin.take().unwrap(), b"y\n");
    child.wait_with_output().unwrap()
}

#[test]
#[ignore]
fn rescue_rolls_back_a_root_whose_every_program_is_broken_given_only_its_device() {
    let _system = lock_system();
    let broken = BrokenRoot::new();
    let out = ok(rescue_static(&["--root", &broken.device.0, "rollback", "1"]));
    assert!(out.contains("snapshot 1 is now @"), "{out}");
    assert!(
        out.contains("git doesn't start on this root") && out.contains("generation 7's record"),
        "the root's git is garbage too, so the keep goes by the generation's record: {out}"
    );
    broken.assert_rolled_back();
}

#[test]
#[ignore]
fn rescue_rolls_back_a_root_mounted_by_hand_from_that_mounts_device_not_the_kernel_command_line() {
    let _system = lock_system();
    let broken = BrokenRoot::new();
    let at = broken.dir.path().join("mnt");
    fs::create_dir(&at).unwrap();
    sh(&format!("mount -o subvol=/@ {} {}", broken.device.0, at.display()));
    let rolled = rescue_static(&["--root-dir", at.to_str().unwrap(), "rollback", "1"]);
    sh(&format!("umount {}", at.display()));
    let out = ok(rolled);
    assert!(
        out.contains("snapshot 1 is now @") && out.contains("still shows the root that was replaced"),
        "{out}"
    );
    assert!(
        !out.contains("archix-test-live"),
        "the live system's root= is none of its business: {out}"
    );
    broken.assert_rolled_back();
}

#[test]
#[ignore]
fn rescue_with_no_flags_finds_the_only_archix_root_and_names_them_when_there_are_several() {
    let _system = lock_system();
    let none = rescue_static(&["rollback", "1"]);
    assert!(
        !none.status.success() && text(&none).contains("no archix root was found") && text(&none).contains("--root"),
        "{}",
        text(&none)
    );
    let broken = BrokenRoot::new();
    {
        let other = BrokenRoot::new();
        let several = rescue_static(&["rollback", "1"]);
        let out = text(&several);
        assert!(!several.status.success(), "{out}");
        assert!(
            out.contains("2 archix roots were found")
                && out.contains(&broken.device.0)
                && out.contains(&other.device.0)
                && out.contains("--root"),
            "{out}"
        );
    }
    let out = ok(rescue_static(&["rollback", "1"]));
    assert!(
        out.contains(&format!("the archix root is on {}", broken.device.0)) && out.contains("snapshot 1 is now @"),
        "{out}"
    );
    broken.assert_rolled_back();
}

#[test]
#[ignore]
fn rescue_password_shell_and_check_find_and_mount_the_root_themselves() {
    let _system = lock_system();
    let broken = BrokenRoot::new();
    let run = |input: &str, args: &[&str]| {
        let mut child = Command::new(STATIC)
            .arg("rescue")
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(&mut child.stdin.take().unwrap(), input.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    };
    ok(run("new pass\nnew pass\n", &["password", "root"]));
    let shadow = broken.top(|top| fs::read_to_string(top.join("@/etc/shadow")).unwrap());
    assert!(shadow.starts_with("root:$6$"), "the root's own shadow, in @: {shadow}");
    let out = ok(run("cat etc/archix-test; exit\n", &["shell"]));
    assert!(
        out.contains("this rescue environment's shell in /run/archix-rescue/root") && out.contains("broken"),
        "the environment's sh, in the root it mounted: {out}"
    );
    let out = ok(run("", &["check"]));
    assert!(
        out.contains(&format!("{} (btrfs) is clean", broken.device.0))
            && out.contains(&format!("{} (vfat) is clean", broken.esp.0))
            && out.contains("the root and the ESP are clean"),
        "the root by looking at the disks, the ESP by the root's fstab: {out}"
    );
}

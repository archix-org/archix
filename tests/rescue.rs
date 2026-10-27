//! The rescue boot entry and `archix rescue`; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

mod common;

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::*;

fn body(esp: &Path, extra: &str) -> String {
    format!(
        r#"boot = {{ loader = "systemd-boot", esp = "{}", secure_boot = true{extra} }}, kernel_params = {{ "root=/dev/vda2", "rw" }}, deployments = {{ keep = 3 }}"#,
        esp.display()
    )
}

fn sbctl_keys(esp: &Path) {
    if !Path::new("/var/lib/sbctl/keys/db/db.key").exists() {
        let made = Command::new("sbctl")
            .env("ESP_PATH", esp)
            .arg("create-keys")
            .status()
            .unwrap();
        assert!(made.success());
    }
}

fn rescue_uki(esp: &Path) -> PathBuf {
    archix::system::rescue::uki(esp).expect("the rescue entry names a UKI")
}

fn rescue_ukis(esp: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(esp.join(archix::system::rescue::DIR))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn record() -> archix::system::rescue::Record {
    serde_json::from_str(&fs::read_to_string("/var/lib/archix/boot/rescue.json").unwrap()).unwrap()
}

/// The initramfs packed into a UKI, unpacked into a directory.
fn unpacked(uki: &Path) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let initrd = dir.path().join("initrd");
    let copied = Command::new("objcopy")
        .args(["-O", "binary", "--only-section=.initrd"])
        .arg(uki)
        .arg(&initrd)
        .status()
        .unwrap();
    assert!(copied.success());
    let out = dir.path().join("out");
    fs::create_dir(&out).unwrap();
    let extracted = Command::new("lsinitcpio")
        .arg("-x")
        .arg(&initrd)
        .current_dir(&out)
        .status()
        .unwrap();
    assert!(extracted.success());
    dir
}

fn text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr)
}

#[test]
#[ignore]
fn every_switch_keeps_a_signed_rescue_entry_from_the_newest_deployment_that_booted_fine() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    let machine = &setup.machine;
    let esp = machine.esp();
    sbctl_keys(&esp);
    // The test template leaves kernel modules out, and the rescue image takes every storage, filesystem and keyboard module.
    for modules in fs::read_dir("/usr/lib/modules").unwrap().flatten() {
        let from = modules.path().join("kernel");
        let to = machine.root.join("usr/lib/modules").join(modules.file_name());
        if from.is_dir() && to.is_dir() {
            assert!(
                Command::new("cp")
                    .arg("-a")
                    .arg(&from)
                    .arg(&to)
                    .status()
                    .unwrap()
                    .success()
            );
        }
    }
    setup.set_body(&[], &body(&esp, ""));
    let out = setup.lay_out();
    let first = machine.current().unwrap();
    assert!(
        out.contains(&format!("the rescue entry boots deployment {first}'s kernel")),
        "the first layout writes it from current, as nothing booted yet:\n{out}"
    );
    let entry = fs::read_to_string(esp.join("loader/entries/archix-rescue.conf")).unwrap();
    assert!(
        entry.contains("sort-key archix-rescue\n") && entry.contains("efi /EFI/archix-rescue/rescue-"),
        "{entry}"
    );
    let uki = rescue_uki(&esp);
    let verified = Command::new("sbctl")
        .env("ESP_PATH", &esp)
        .arg("verify")
        .arg(&uki)
        .output()
        .unwrap();
    assert!(text(&verified).contains("is signed"), "{}", text(&verified));
    let inspected = text(&Command::new("ukify").arg("inspect").arg(&uki).output().unwrap());
    assert!(
        inspected.contains("root=/dev/vda2 rw archix.rescue") && !inspected.contains(".pcrsig"),
        "{inspected}"
    );
    let image = unpacked(&uki);
    let root = image.path().join("out");
    for path in [
        "hooks/archix-rescue",
        "usr/bin/archix",
        "usr/bin/e2fsck",
        "usr/bin/fsck.fat",
        "usr/bin/btrfs",
        "usr/bin/bsdtar",
        "usr/lib/archix-rescue/cp",
        "usr/bin/cttyhack",
        "usr/bin/cryptsetup",
        "usr/lib/udev/rules.d/95-dm-notify.rules",
        "usr/lib/libgcc_s.so.1",
    ] {
        assert!(
            root.join(path).symlink_metadata().is_ok(),
            "the rescue initramfs lacks {path}"
        );
    }
    assert!(
        !root.join("hooks/archix-deploy").exists(),
        "the rescue mounts no deployment"
    );
    let hook = fs::read_to_string(root.join("hooks/archix-rescue")).unwrap();
    assert!(
        !hook.contains("launch_interactive_shell") && hook.contains("reboot -f"),
        "the rescue hook never falls back to a shell:\n{hook}"
    );
    let found = Command::new("find")
        .arg(&root)
        .args(["-name", "libcryptsetup-token-*"])
        .output()
        .unwrap();
    assert!(
        found.stdout.is_empty(),
        "no LUKS2 token plugin, so a TPM can't open a volume in the rescue: {}",
        text(&found)
    );
    assert_eq!(
        fs::read(root.join("usr/bin/archix")).unwrap(),
        fs::read(machine.deployment(first).join("usr/lib/archix/archix-static")).unwrap(),
        "the rescue runs its deployment's static archix"
    );
    assert!(
        fs::read_to_string(root.join("config"))
            .unwrap()
            .contains("archix-rescue")
    );
    assert!(!machine.loader_conf().contains("rescue"), "{}", machine.loader_conf());
    assert_eq!(record().source, first);

    // The new deployment hasn't booted, so the rescue stays on the one that runs.
    setup.set_body(&["tree"], &body(&esp, ""));
    setup.ok(&["switch", "--noconfirm"]);
    let second = machine.current().unwrap();
    assert_eq!(rescue_uki(&esp), uki, "nothing it's built from changed");
    assert_eq!(
        fs::metadata(&uki).unwrap().ino(),
        fs::metadata(rescue_uki(&esp)).unwrap().ino()
    );
    machine.reboot(Some(&format!("archix-{second}.conf")));
    fs::create_dir_all("/var/lib/archix/activated").unwrap();
    fs::write(format!("/var/lib/archix/activated/{second}-test"), "").unwrap();
    setup.set_body(&[], &body(&esp, ""));
    let out = setup.ok(&["switch", "--noconfirm"]);
    let third = machine.current().unwrap();
    assert!(
        out.contains(&format!("the rescue entry boots deployment {second}'s kernel")),
        "{second} booted fine, so it gives the kernel now, not the new {third}:\n{out}"
    );
    assert_eq!(record().source, second);
    let refreshed = rescue_uki(&esp);
    assert_ne!(refreshed, uki);
    assert_eq!(
        rescue_ukis(&esp),
        [refreshed.file_name().unwrap().to_string_lossy()],
        "the old rescue UKI went once the entry named the new one"
    );
    let signed = fs::read_to_string("/var/lib/archix/boot/signed").unwrap();
    let hash: String = archix::system::authenticode::hash(&fs::read(&refreshed).unwrap())
        .unwrap()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert!(
        signed.lines().any(|line| line.starts_with(&hash)),
        "the refreshed rescue UKI is recorded as signed, so it can be revoked once replaced:\n{signed}"
    );

    setup.ok(&["gc"]);
    assert_eq!(rescue_uki(&esp), refreshed, "gc keeps it");
    assert!(refreshed.is_file());
    assert!(!machine.loader_conf().contains("rescue"));

    setup.set_body(&["tree"], &body(&esp, ", rescue_entry = false"));
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(out.contains("took the rescue entry off the ESP"), "{out}");
    assert!(!esp.join("loader/entries/archix-rescue.conf").exists() && rescue_ukis(&esp).is_empty());
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

/// A file of deployment `number` replaced by a new one with other content, as damage that reaches no other deployment.
fn damage(machine: &Machine, number: u32, path: &str) {
    let file = machine.deployment(number).join(path);
    let new = file.with_extension("damaged");
    fs::write(&new, "damaged\n").unwrap();
    fs::rename(&new, &file).unwrap();
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
fn the_rescue_commands_repair_repoint_reset_a_password_and_eject_a_root_nothing_runs_from() {
    let _system = lock_system();
    let keep = "deployments = { keep = 3 }";
    let setup = Setup::new(&["tree", "nano"], keep);
    setup.lay_out();
    let machine = &setup.machine;
    let older = machine.current().unwrap();
    setup.set_body(&["nano"], keep);
    setup.ok(&["switch", "--noconfirm"]);
    let newer = machine.current().unwrap();
    machine.power_off();

    let listed = ok(rescue(&setup, &["list"]));
    assert!(
        listed.contains(&format!("* deployment {newer:<4} current, boot default"))
            && listed.contains(&format!("  deployment {older:<4}")),
        "{listed}"
    );
    assert!(listed.contains("never booted"), "{listed}");
    let refused = text(&rescue(&setup, &["check"]));
    assert!(refused.contains("needs the root and the ESP unmounted"), "{refused}");

    damage(machine, older, "usr/bin/tree");
    let broken = rescue(&setup, &["verify", &older.to_string()]);
    assert!(
        !broken.status.success() && text(&broken).contains("/usr/bin/tree (tree)"),
        "{}",
        text(&broken)
    );
    let used = ok(rescue(&setup, &["use", &older.to_string()]));
    assert!(
        used.contains("repaired 1 files") && used.contains("is current and the boot default"),
        "{used}"
    );
    assert_eq!(machine.current(), Some(older));
    assert!(
        machine.loader_conf().contains(&format!("default archix-{older}.conf")),
        "{}",
        machine.loader_conf()
    );
    assert_eq!(
        fs::read_link(machine.root.join("archix/current")).unwrap(),
        Path::new(&format!("deploy/{older}"))
    );
    assert!(ok(rescue(&setup, &["verify"])).contains(&format!("deployment {older}: matches its packages")));
    assert!(!text(&rescue(&setup, &["use", "99"])).contains("is current"));

    let differ = rescue_answering(&setup, "one\ntwo\n", &["password", "root", "--all"]);
    assert!(
        !differ.status.success() && text(&differ).contains("differ"),
        "{}",
        text(&differ)
    );
    ok(rescue_answering(
        &setup,
        "s3cret pass\ns3cret pass\n",
        &["password", "root", "--all"],
    ));
    for number in [older, newer] {
        let hash = root_hash(&machine.deployment(number).join("etc"));
        let salt = hash.split('$').nth(2).unwrap();
        let openssl = Command::new("openssl")
            .args(["passwd", "-6", "-salt", salt, "s3cret pass"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&openssl.stdout).trim(),
            hash,
            "deployment {number}"
        );
    }

    let nanorc = machine.deployment(older).join("etc/nanorc");
    let shipped = fs::read(machine.deployment(newer).join("etc/nanorc")).unwrap();
    fs::write(&nanorc, "include broken\n").unwrap();
    let restored = ok(rescue(&setup, &["restore-etc", "/etc/nanorc"]));
    assert!(
        restored.contains(&format!("restored /etc/nanorc in deployment {older} as nano ships it")),
        "{restored}"
    );
    assert_eq!(fs::read(&nanorc).unwrap(), shipped);
    let refused = text(&rescue(&setup, &["restore-etc", "/etc/shadow", "--all"]));
    assert!(refused.contains("holds this machine's users"), "{refused}");

    let esp = machine.esp();
    let left = esp.join(archix::system::rescue::DIR).join("rescue-x.efi");
    fs::create_dir_all(left.parent().unwrap()).unwrap();
    fs::write(&left, "uki").unwrap();
    fs::write(
        archix::system::rescue::entry_path(&esp),
        archix::system::rescue::entry_text("rescue-x.efi", older, "7"),
    )
    .unwrap();
    let ejected = ok(rescue(&setup, &["eject", "--noconfirm"]));
    assert!(ejected.contains("plain Arch now"), "{ejected}");
    assert!(!machine.root.join("archix").exists(), "/archix is still there");
    assert!(
        machine.root.join("usr/bin/tree").is_file(),
        "deployment {older}, with tree, is the root's own now"
    );
    assert!(
        machine.loader_conf().contains("default arch-linux.efi\n"),
        "{}",
        machine.loader_conf()
    );
    assert!(esp.join("EFI/Linux/arch-linux.efi").is_file());
    assert!(machine.entries().is_empty(), "{:?}", machine.entries());
    assert!(
        !esp.join(archix::system::rescue::DIR).exists(),
        "eject takes the rescue entry off"
    );
    assert!(archix::deploy::eject::read_state(&setup.paths.state).unwrap().is_none());
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
fn the_rescue_opens_only_for_root_or_a_wheel_users_password_from_the_current_deployment() {
    let _system = lock_system();
    let setup = Setup::new(&[], "deployments = { keep = 3 }");
    setup.lay_out();
    let machine = &setup.machine;
    let current = machine.current().unwrap();
    machine.power_off();
    let etc = machine.deployment(current).join("etc");
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

    let out = ok(rescue_login(&setup, "root\nroot pass\n", &["list"]).0);
    assert!(
        out.contains("logged in as root") && out.contains(&format!("* deployment {current:<4}")),
        "root's password opens it:\n{out}"
    );
    let out = ok(rescue_login(&setup, "alice\ncorrect horse battery\n", &["list"]).0);
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
    let out = ok(rescue(&setup, &["list"]));
    assert!(
        !out.contains("rescue login"),
        "outside the rescue boot, as from a live USB, there's no login:\n{out}"
    );
}

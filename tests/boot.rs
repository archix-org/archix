//! Builds deployments' boot entries on a test machine's ESP; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::*;

fn boot_body(setup_esp: &Path, extra: &str) -> String {
    format!(
        r#"boot = {{ loader = "systemd-boot", esp = "{}" {extra} }}, kernel_params = {{ "root=/dev/vda2", "rw" }}"#,
        setup_esp.display()
    )
}

/// Entry ids, as systemd-boot reports them: the file names without their boot counters.
fn entries(machine: &Machine) -> Vec<String> {
    let mut names: Vec<String> = machine.entries().iter().map(|name| entry_id(name)).collect();
    names.sort();
    names
}

fn entry_id(file: &str) -> String {
    match file.split_once('+') {
        Some((stem, _)) => format!("{stem}.conf"),
        None => file.to_string(),
    }
}

/// The entry file behind an id or a file name, whatever its boot counters say.
fn entry_file(machine: &Machine, id: &str) -> PathBuf {
    let file = machine
        .entries()
        .into_iter()
        .find(|file| entry_id(file) == entry_id(id))
        .unwrap_or_else(|| panic!("no entry {id}"));
    machine.esp().join("loader/entries").join(file)
}

/// The UKI a boot menu entry starts.
fn uki(machine: &Machine, entry: &str) -> PathBuf {
    let text = fs::read_to_string(entry_file(machine, entry)).unwrap();
    let path = text
        .lines()
        .find_map(|line| line.strip_prefix("efi ").or_else(|| line.strip_prefix("uki ")))
        .unwrap()
        .trim();
    machine.esp().join(path.trim_start_matches('/'))
}

fn inspect(machine: &Machine, entry: &str) -> String {
    let output = Command::new("ukify")
        .arg("inspect")
        .arg(uki(machine, entry))
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned()
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

#[test]
#[ignore]
fn signed_images_are_recorded_by_hash_and_listed_for_revoking_once_off_the_esp() {
    let _system = lock_system();
    let machine_esp = |setup: &Setup| setup.machine.esp();
    let pacman = Command::new("pacman")
        .args(["-S", "--needed", "--noconfirm", "efitools"])
        .status()
        .unwrap();
    assert!(pacman.success());
    let setup = Setup::new(&[], "");
    let esp = machine_esp(&setup);
    sbctl_keys(&esp);
    let body = |n: u32| {
        format!(
            r#"deployments = {{ keep = 1 }}, boot = {{ loader = "systemd-boot", esp = "{}", secure_boot = true }}, kernel_params = {{ "root=/dev/vda2", "rw", "archix.test={n}" }}"#,
            esp.display()
        )
    };
    setup.set_body(&[], &body(1));
    setup.ok(&["switch", "--noconfirm"]);
    let machine = &setup.machine;
    let first = uki(machine, &format!("archix-{}.conf", machine.current().unwrap()));
    let esl = std::env::temp_dir().join("archix-first.esl");
    assert!(
        Command::new("hash-to-efi-sig-list")
            .arg(&first)
            .arg(&esl)
            .status()
            .unwrap()
            .success()
    );
    let efitools: String = fs::read(&esl).unwrap()[44..76]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let recorded = fs::read_to_string("/var/lib/archix/boot/signed").unwrap_or_default();
    // Two more UKIs, so gc takes the first once its deployment is neither booted, current nor the default.
    setup.set_body(&[], &body(2));
    setup.ok(&["switch", "--noconfirm"]);
    machine.reboot(Some(&format!("archix-{}.conf", machine.current().unwrap())));
    setup.set_body(&[], &body(3));
    setup.ok(&["switch", "--noconfirm"]);
    let gone = !first.exists();
    let in_use = uki(machine, &format!("archix-{}.conf", machine.current().unwrap()))
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let listed = setup.run(&["secure-boot", "revoke", "--dry-run"]);
    let listed = String::from_utf8_lossy(&listed.stdout).to_string() + &String::from_utf8_lossy(&listed.stderr);
    assert!(
        recorded.contains(&efitools),
        "archix records the hash efitools computes, {efitools}:\n{recorded}"
    );
    assert!(gone, "dropping the deployment removes the first UKI");
    assert!(
        listed.contains("archix signed and no longer uses")
            && listed.contains(&efitools[..16])
            && !listed.contains(&in_use),
        "{listed}"
    );
}

#[test]
#[ignore]
fn status_and_switch_say_when_systemd_boot_is_too_old_to_count_boots() {
    let _system = lock_system();
    // Every LoaderFeatures bit below Type #1 UKI entries, as systemd-boot 257 reports them, and every one 262 has.
    let old = ("ARCHIX_TEST_LOADER_FEATURES", "65535");
    let new = ("ARCHIX_TEST_LOADER_FEATURES", "4194303");
    let setup = Setup::new(&["tree"], "");
    let switched = setup.ok_env(&[old], &["switch", "--noconfirm"]);
    let status_old = setup.ok_env(&[old], &["status"]);
    let status_new = setup.ok_env(&[new], &["status"]);
    let warning = "boot counting is off: the systemd-boot that started this boot is older than 260";
    assert!(switched.contains(warning), "{switched}");
    assert!(status_old.contains(warning), "{status_old}");
    assert!(!status_new.contains("boot counting is off"), "{status_new}");
}

#[test]
#[ignore]
fn with_systemds_pcr_key_every_uki_carries_a_signed_pcr_policy() {
    let _system = lock_system();
    let setup = Setup::new(&["tree"], "");
    setup.lay_out();
    let machine = &setup.machine;
    let unsigned = inspect(machine, &format!("archix-{}.conf", machine.current().unwrap()));
    let (private, public) = (
        system_path("/etc/systemd/tpm2-pcr-private-key.pem"),
        system_path("/etc/systemd/tpm2-pcr-public-key.pem"),
    );
    let openssl = |args: &[&str]| assert!(Command::new("openssl").args(args).status().unwrap().success());
    openssl(&[
        "genpkey",
        "-quiet",
        "-algorithm",
        "RSA",
        "-pkeyopt",
        "rsa_keygen_bits:2048",
        "-out",
        private.to_str().unwrap(),
    ]);
    openssl(&[
        "pkey",
        "-in",
        private.to_str().unwrap(),
        "-pubout",
        "-out",
        public.to_str().unwrap(),
    ]);
    // Same kernel, initramfs and command line, so only the key tells this UKI from the last one.
    setup.set_body(&[], "");
    let out = setup.ok(&["switch", "--noconfirm"]);
    let signed = inspect(machine, &format!("archix-{}.conf", machine.current().unwrap()));
    assert!(!unsigned.contains(".pcrsig"), "{unsigned}");
    assert!(
        signed.contains(".pcrsig") && signed.contains(".pcrpkey"),
        "{signed}\n{out}"
    );
}

#[test]
#[ignore]
fn every_deployment_gets_a_signed_boot_entry_and_booting_an_older_one_makes_it_current() {
    let _system = lock_system();
    let keep = ", deployments = { keep = 5 }";
    let setup = Setup::new(&["tree"], &keep[2..]);
    setup.lay_out();
    let machine = &setup.machine;
    let esp = machine.esp();
    let with_tree = machine.current().unwrap();
    let entry = format!("archix-{with_tree}.conf");
    assert!(entries(machine).contains(&entry), "{:?}", entries(machine));
    let info = inspect(machine, &entry);
    assert!(info.contains("root=/dev/vda2"), "{info}");
    let menu = fs::read_to_string(entry_file(machine, &entry)).unwrap();
    assert!(
        menu.contains(&format!("generation {with_tree}")),
        "the entry's title names the generation:\n{menu}"
    );
    assert!(system_path("/etc/systemd/system/sysinit.target.wants/archix-boot.service").is_symlink());
    assert!(system_path("/etc/systemd/system/sysinit.target.wants/archix-deploy-activate.service").is_symlink());
    assert!(system_path("/etc/mkinitcpio.conf.d/archix-deploy.conf").exists());

    setup.set_body(&[], &(boot_body(&esp, "") + keep));
    setup.ok(&["switch", "--noconfirm"]);
    let without_tree = machine.current().unwrap();
    setup.set_body(&["tree"], &(boot_body(&esp, "") + keep));
    setup.ok(&["switch", "--noconfirm"]);
    let newest = machine.current().unwrap();
    let kept: BTreeSet<String> = entries(machine).into_iter().collect();
    let expected: BTreeSet<String> = machine
        .deployments()
        .iter()
        .map(|n| format!("archix-{n}.conf"))
        .collect();
    assert_eq!(kept, expected, "one entry per kept deployment");
    assert!(kept.contains(&format!("archix-{newest}.conf")) && kept.contains(&format!("archix-{without_tree}.conf")));

    setup.ok(&[
        "boot-check",
        "--cmdline",
        &format!("root=/dev/vda2 archix.generation={without_tree}"),
    ]);
    assert_eq!(
        machine.current(),
        Some(without_tree),
        "booting an older entry makes it current"
    );
    assert!(installed("tree").is_none());

    sbctl_keys(&esp);
    let loader = esp.join("EFI/systemd/systemd-bootx64.efi");
    fs::create_dir_all(loader.parent().unwrap()).unwrap();
    fs::copy("/usr/lib/systemd/boot/efi/systemd-bootx64.efi", &loader).unwrap();
    let unsigned = std::os::unix::fs::MetadataExt::ino(&fs::metadata(&loader).unwrap());
    setup.set_body(&[], &(boot_body(&esp, ", secure_boot = true") + keep));
    setup.ok(&["switch", "--noconfirm"]);
    assert_ne!(
        std::os::unix::fs::MetadataExt::ino(&fs::metadata(&loader).unwrap()),
        unsigned,
        "the loader was signed in place, which a crash can leave half written"
    );
    let verified = Command::new("sbctl")
        .env("ESP_PATH", &esp)
        .arg("verify")
        .arg(&loader)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&verified.stdout).contains("is signed"));
    assert!(!loader.with_file_name("systemd-bootx64.efi.tmp").exists());
    let signed = uki(machine, &format!("archix-{}.conf", machine.current().unwrap()));
    let verify = Command::new("sbctl")
        .env("ESP_PATH", &esp)
        .arg("verify")
        .arg(&signed)
        .output()
        .unwrap();
    let verify = String::from_utf8_lossy(&verify.stdout).to_string() + &String::from_utf8_lossy(&verify.stderr);
    assert!(verify.contains("is signed"), "{verify}");
    let fallback = esp.join(format!("EFI/Linux/archix-fallback-{}.efi", machine.current().unwrap()));
    let verify = Command::new("sbctl")
        .env("ESP_PATH", &esp)
        .arg("verify")
        .arg(&fallback)
        .output()
        .unwrap();
    let verify = String::from_utf8_lossy(&verify.stdout).to_string() + &String::from_utf8_lossy(&verify.stderr);
    assert!(verify.contains("is signed"), "the fallback UKI isn't signed: {verify}");
}

/// The file names in an initramfs packed into a UKI.
fn initramfs_files(uki: &Path) -> String {
    let dir = tempfile::tempdir().unwrap();
    let initrd = dir.path().join("initrd");
    let copied = Command::new("objcopy")
        .args(["-O", "binary", "--only-section=.initrd"])
        .arg(uki)
        .arg(&initrd)
        .status()
        .unwrap();
    assert!(copied.success());
    let listed = Command::new("lsinitcpio").arg(&initrd).output().unwrap();
    String::from_utf8_lossy(&listed.stdout).into_owned()
}

#[test]
#[ignore]
fn deployment_1s_entry_boots_through_the_deployment_hook_from_the_first_switch_on() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    let machine = &setup.machine;
    let files = initramfs_files(&uki(machine, "archix-1.conf"));
    assert!(
        files.lines().any(|line| line.ends_with("hooks/archix-deploy")) && files.contains("usr/bin/archix"),
        "a deployment boots only through the hook, so deployment 1's initramfs has it from the start:\n{files}"
    );
}

/// A file from the initramfs packed into a UKI, empty when it has none.
fn initramfs_file(uki: &Path, path: &str) -> String {
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
    fs::read_to_string(out.join(path)).unwrap_or_default()
}

#[test]
#[ignore]
fn an_archix_the_switch_upgrades_writes_its_own_mount_unit_into_the_new_initramfs() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    setup.lay_out();
    let machine = &setup.machine;
    let lib = machine.deployment(machine.current().unwrap()).join("usr/lib/archix");
    // As after an upgrade in the switch: the new deployment's archix writes another mount unit than the running one.
    machine.store_writable(true);
    fs::rename(lib.join("archix-static"), lib.join("archix-static.real")).unwrap();
    write_executable(
        &lib.join("archix-static"),
        "#!/bin/sh\n/usr/lib/archix/archix-static.real \"$@\" || exit\n\
         [ \"$1\" != deploy-integrate ] || echo '# the upgraded archix' >> /usr/lib/archix/archix-deploy-mount.service\n",
    );
    machine.store_writable(false);
    setup.set_body(&["tree"], "");
    setup.ok(&["switch", "--noconfirm"]);
    let new = machine.current().unwrap();
    let unit = initramfs_file(
        &uki(machine, &format!("archix-{new}.conf")),
        "usr/lib/systemd/system/archix-deploy-mount.service",
    );
    assert!(
        unit.contains("ExecStart=/usr/bin/archix deploy-mount") && unit.ends_with("# the upgraded archix\n"),
        "the initramfs has the running archix's mount unit, not the new one's:\n{unit}"
    );
}

/// Plain pacman on a machine's root, as a user or yay runs it there.
fn plain_pacman(machine: &Machine, args: &[&str]) -> (bool, String) {
    let root = machine.root.display().to_string();
    let output = Command::new("pacman")
        .args(["--root", &root, "--dbpath", &format!("{root}/var/lib/pacman")])
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

fn qkk(machine: &Machine, names: &[&str]) -> String {
    let root = machine.root.display().to_string();
    let output = Command::new("pacman")
        .args(["--root", &root, "--dbpath", &format!("{root}/var/lib/pacman"), "-Qkk"])
        .args(names)
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr)
}

fn is_mountpoint(path: &Path) -> bool {
    archix::deploy::view::mount_points(&fs::read("/proc/self/mountinfo").unwrap()).contains(&path.to_path_buf())
}

#[test]
#[ignore]
fn eject_before_the_first_switch_takes_archixs_files_off_and_leaves_the_rest() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    let machine = &setup.machine;
    let root = machine.root.clone();
    let conf = root.join("etc/pacman.conf");
    let arch_default = fs::read_to_string(&conf).unwrap().replace("#CheckSpace", "CheckSpace");
    fs::write(&conf, &arch_default).unwrap();
    archix::integration::write_all(&setup.paths, Path::new("/usr/bin/archix"), &|path: &Path| {
        Ok(root.join(path.strip_prefix("/").unwrap()))
    })
    .unwrap();
    assert!(root.join("etc/pacman.d/hooks/archix.hook").is_file());
    let out = setup.ok(&["eject", "--noconfirm"]);
    assert!(out.contains("archix no longer manages this machine"), "{out}");
    for gone in [
        "etc/pacman.d/hooks/archix.hook",
        "etc/pacman.d/hooks/00-archix-deployment.hook",
        "etc/systemd/system/archix-boot.service",
        "etc/mkinitcpio.conf.d/archix-deploy.conf",
        "etc/profile.d/archix.sh",
    ] {
        assert!(root.join(gone).symlink_metadata().is_err(), "{gone} is still there");
    }
    assert_eq!(
        fs::read_to_string(&conf).unwrap(),
        arch_default,
        "CheckSpace is back, the rest as it was"
    );
    assert!(machine.entries().is_empty() && !root.join("archix").exists());
    assert!(
        Path::new("/var/lib/archix").is_dir(),
        "without --purge archix's state stays"
    );
}

#[test]
#[ignore]
fn eject_from_deployments_leaves_plain_arch_with_the_same_packages_that_plain_pacman_can_change() {
    let _system = lock_system();
    let setup = Setup::new(&["tree"], "");
    let machine = &setup.machine;
    let root = machine.root.clone();
    let conf = root.join("etc/pacman.conf");
    fs::write(
        &conf,
        fs::read_to_string(&conf).unwrap().replace("#CheckSpace", "CheckSpace"),
    )
    .unwrap();
    setup.lay_out();
    let ejected = machine.current().unwrap();
    let packages = ["tree", "bash"];
    let before = qkk(machine, &packages);
    let (refused, _) = plain_pacman(machine, &["-R", "tree"]);
    assert!(!refused, "plain pacman changed a deployment");
    let out = setup.ok(&["eject", "--noconfirm"]);
    println!("{out}");
    assert!(out.contains("plain Arch now"), "{out}");
    assert!(!root.join("archix").exists(), "/archix is still there");
    for at in ["usr", "etc", "opt", "boot", "var/lib/pacman/local"] {
        assert!(!is_mountpoint(&root.join(at)), "{at} is still bound from a deployment");
    }
    assert_eq!(
        qkk(machine, &packages),
        before,
        "pacman -Qkk differs from deployment {ejected}'s"
    );
    assert!(root.join("usr/bin/tree").is_file());
    let esp = machine.esp();
    let uki = esp.join("EFI/Linux/arch-linux.efi");
    let inspected = Command::new("ukify").arg("inspect").arg(&uki).output().unwrap();
    let inspected = String::from_utf8_lossy(&inspected.stdout);
    assert!(
        inspected.contains(".linux") && inspected.contains("root=/dev/vda2"),
        "{inspected}"
    );
    assert!(
        machine.loader_conf().contains("default arch-linux.efi\n"),
        "{}",
        machine.loader_conf()
    );
    assert!(machine.entries().is_empty(), "{:?}", machine.entries());
    assert!(archix::system::boot::fallbacks(&esp).is_empty() && !esp.join("EFI/archix").exists());
    for gone in [
        "etc/initcpio/install/archix-deploy",
        "etc/mkinitcpio.conf.d/archix-deploy.conf",
        "etc/pacman.d/hooks/00-archix-deployment.hook",
        "etc/systemd/system/archix-deploy-activate.service",
        "boot/archix",
    ] {
        assert!(root.join(gone).symlink_metadata().is_err(), "{gone} is still there");
    }
    let initramfs = initramfs_files(&uki);
    assert!(
        !initramfs.contains("archix-deploy"),
        "the plain initramfs has archix's hook:\n{initramfs}"
    );
    assert!(
        fs::read_to_string(&conf)
            .unwrap()
            .lines()
            .any(|line| line == "CheckSpace")
    );
    assert!(archix::deploy::eject::read_state(&setup.paths.state).unwrap().is_none());

    let copied = Command::new("cp")
        .args(["-a", "/var/lib/pacman/sync"])
        .arg(root.join("var/lib/pacman/"))
        .status()
        .unwrap();
    assert!(copied.success());
    for args in [&["-R", "tree"][..], &["-S", "tree"], &["-Syu"]] {
        let (ok, out) = plain_pacman(machine, args);
        assert!(
            ok && !out.contains("archix install"),
            "pacman {args:?} after the eject: {out}"
        );
    }
    assert!(root.join("usr/bin/tree").is_file());
}

#[test]
#[ignore]
fn the_fallback_uki_boots_the_current_deployment_under_its_own_title_and_follows_a_rollback() {
    let _system = lock_system();
    let keep = "deployments = { keep = 5 }";
    let setup = Setup::new(&[], keep);
    setup.lay_out();
    let machine = &setup.machine;
    let esp = machine.esp();
    let older = machine.current().unwrap();
    assert_eq!(
        archix::system::boot::fallbacks(&esp),
        [older],
        "the first layout's deployment gets one"
    );
    setup.set_body(&["tree"], keep);
    setup.ok(&["switch", "--noconfirm"]);
    let newest = machine.current().unwrap();
    assert_eq!(
        archix::system::boot::fallbacks(&esp),
        [newest],
        "the older fallback went"
    );
    let fallback = esp.join(format!("EFI/Linux/archix-fallback-{newest}.efi"));
    let inspected = Command::new("ukify").arg("inspect").arg(&fallback).output().unwrap();
    let inspected = String::from_utf8_lossy(&inspected.stdout);
    assert!(
        inspected.contains("PRETTY_NAME=\"archix (fallback)\"")
            && inspected.contains("IMAGE_ID=0archix-fallback")
            && inspected.contains("root=/dev/vda2"),
        "{inspected}"
    );
    let entry = fs::read_to_string(
        machine.esp().join("loader/entries").join(
            machine
                .entries()
                .into_iter()
                .find(|e| e.starts_with(&format!("archix-{newest}")))
                .unwrap(),
        ),
    )
    .unwrap();
    assert!(
        !entry.contains("(fallback)") && entry.contains(&format!("generation {newest}")),
        "{entry}"
    );
    assert_eq!(
        fs::read(&fallback).unwrap(),
        fs::read(uki(machine, &format!("archix-{newest}.conf"))).unwrap(),
        "the fallback is the entry's own signed UKI, so revoking adds nothing per switch"
    );
    assert!(
        !fallback.file_name().unwrap().to_string_lossy().contains('+'),
        "no boot counter"
    );

    fs::remove_dir_all(esp.join("loader/entries")).unwrap();
    let out = machine.reboot(Some(&format!("archix-fallback-{newest}.efi")));
    assert!(
        out.contains(&format!(
            "booting deployment {newest} (the boot entry archix-fallback-{newest}.efi)"
        )),
        "{out}"
    );

    setup.ok(&["rollback", &older.to_string(), "--noconfirm"]);
    assert_eq!(machine.current(), Some(older));
    assert_eq!(
        archix::system::boot::fallbacks(&esp),
        [older],
        "the fallback follows the rollback"
    );
    let out = machine.reboot(Some(&format!("archix-fallback-{older}.efi")));
    assert!(
        out.contains(&format!("booting deployment {older} (the boot entry")),
        "{out}"
    );

    let gone = format!("archix-fallback-{newest}.efi");
    fs::write(esp.join("EFI/Linux").join(&gone), "stale").unwrap();
    fs::write(
        setup.paths.state.join("boot/entries").join(&gone),
        format!("uki /EFI/Linux/{gone}\n"),
    )
    .unwrap();
    fs::rename(
        machine.deployment(newest),
        machine.root.join("archix/deploy/.deleting-x"),
    )
    .unwrap();
    let out = machine.reboot(Some(&gone));
    assert!(
        out.contains(&format!("booting deployment {older} (current;")),
        "a fallback whose deployment was collected boots current: {out}"
    );
}

#[test]
#[ignore]
fn a_lost_loader_conf_after_a_rollback_leaves_the_rolled_back_deployment_current() {
    let _system = lock_system();
    let keep = "deployments = { keep = 5 }";
    let setup = Setup::new(&[], keep);
    setup.lay_out();
    let machine = &setup.machine;
    let older = machine.current().unwrap();
    setup.set_body(&["tree"], keep);
    setup.ok(&["switch", "--noconfirm"]);
    let newest = machine.current().unwrap();
    setup.ok(&["rollback", &older.to_string(), "--noconfirm"]);
    assert_eq!(machine.current(), Some(older));
    let loader = machine.esp().join("loader/loader.conf");
    fs::write(&loader, "timeout 2\n").unwrap();
    let cmdline = ("ARCHIX_TEST_CMDLINE", "root=/dev/vda2 rw");
    let newest_entry = format!("archix-{newest}.conf");
    let out = setup.ok_env(
        &[cmdline, ("ARCHIX_TEST_ENTRY", newest_entry.as_str())],
        &["boot-check"],
    );
    assert_eq!(machine.current(), Some(older), "the rollback was undone: {out}");
    assert!(out.contains("doesn't name it"), "{out}");
    assert!(
        machine.loader_conf().contains(&format!("default archix-{older}.conf")),
        "{}",
        machine.loader_conf()
    );

    fs::write(&loader, format!("timeout 2\ndefault {newest_entry}\n")).unwrap();
    setup.ok_env(
        &[cmdline, ("ARCHIX_TEST_ENTRY", newest_entry.as_str())],
        &["boot-check"],
    );
    assert_eq!(
        machine.current(),
        Some(newest),
        "a newer entry loader.conf names, as after a crash before a commit's current, becomes current"
    );
}

/// Every file on a machine's ESP except systemd-boot's own, relative to it.
fn esp_files(machine: &Machine) -> Vec<String> {
    fn walk(esp: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(esp, &path, out);
            } else {
                out.push(path.strip_prefix(esp).unwrap().display().to_string());
            }
        }
    }
    let mut files = Vec::new();
    walk(&machine.esp(), &machine.esp(), &mut files);
    files.retain(|f| !f.starts_with("EFI/systemd/") && !f.starts_with("EFI/BOOT/"));
    files.sort();
    files
}

#[test]
#[ignore]
fn adding_packages_one_at_a_time_reuses_one_uki() {
    let _system = lock_system();
    let keep = "deployments = { keep = 20 }";
    let setup = Setup::new(&[], keep);
    setup.lay_out();
    let machine = &setup.machine;
    let first = machine.current().unwrap();
    for count in 1..=5 {
        setup.set_body(&UKI_TEST_PACKAGES[..count], keep);
        setup.ok(&["switch", "--noconfirm"]);
    }
    let files = esp_files(machine);
    let listing = files
        .iter()
        .map(|f| {
            let size = fs::metadata(machine.esp().join(f)).map_or(0, |m| m.len());
            format!("{size:>11}  {f}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    println!("ESP after 5 package adds:\n{listing}");
    let ukis: Vec<&String> = files
        .iter()
        .filter(|f| f.ends_with(".efi") && !f.starts_with("EFI/Linux/archix-fallback-"))
        .collect();
    let fallbacks: Vec<&String> = files.iter().filter(|f| f.starts_with("EFI/Linux/")).collect();
    assert_eq!(
        fallbacks,
        [&format!("EFI/Linux/archix-fallback-{}.efi", machine.current().unwrap())],
        "only the current deployment's fallback:\n{listing}"
    );
    assert_eq!(
        ukis.len(),
        1,
        "one UKI for an unchanged kernel and initramfs:\n{listing}"
    );
    for generation in first..=machine.current().unwrap() {
        let entry = format!("archix-{generation}.conf");
        let menu = fs::read_to_string(entry_file(machine, &entry)).unwrap_or_default();
        assert!(
            menu.contains(&format!("generation {generation})")) && uki(machine, &entry).is_file(),
            "deployment {generation} lost its own boot menu entry:\n{menu}"
        );
    }
}

#[test]
#[ignore]
fn without_a_trustworthy_booted_entry_boot_check_changes_nothing() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    setup.lay_out();
    let machine = &setup.machine;
    let older = machine.current().unwrap();
    setup.set_body(&["tree"], "");
    setup.ok(&["switch", "--noconfirm"]);
    let newest = machine.current().unwrap();
    let cmdline = ("ARCHIX_TEST_CMDLINE", "root=/dev/vda2 rw");
    for (case, entry) in [
        (
            "no LoaderEntrySelected, as when firmware starts a UKI or BOOTX64.EFI directly",
            "",
        ),
        ("an entry from outside archix", "memtest86+.conf"),
        ("an entry of a generation archix doesn't have", "archix-999.conf"),
    ] {
        let out = setup.ok_env(&[cmdline, ("ARCHIX_TEST_ENTRY", entry)], &["boot-check"]);
        println!("boot-check with {case}:\n{out}");
        assert_eq!(machine.current(), Some(newest), "{case}: nothing may change");
    }
    let entry = format!("archix-{older}.conf");
    // Someone with write access to the ESP edits the entry that booted, here to start another deployment's UKI.
    let file = entry_file(machine, &entry);
    let written = fs::read_to_string(&file).unwrap();
    fs::write(
        &file,
        written.replace(&format!("version {older}"), &format!("version {newest}")),
    )
    .unwrap();
    let out = setup.ok_env(&[cmdline, ("ARCHIX_TEST_ENTRY", entry.as_str())], &["boot-check"]);
    assert_eq!(
        machine.current(),
        Some(newest),
        "an entry that isn't what archix wrote may not change current"
    );
    assert!(out.contains("isn't the entry archix wrote"), "{out}");
    fs::write(&file, written).unwrap();
    let record = setup.paths.state.join("boot/entries").join(&entry);
    let recorded = fs::read(&record).unwrap();
    fs::remove_file(&record).unwrap();
    setup.ok_env(&[cmdline, ("ARCHIX_TEST_ENTRY", entry.as_str())], &["boot-check"]);
    assert_eq!(
        machine.current(),
        Some(newest),
        "an entry archix has no record of may not change current"
    );
    fs::write(&record, recorded).unwrap();
    setup.ok_env(&[cmdline, ("ARCHIX_TEST_ENTRY", entry.as_str())], &["boot-check"]);
    assert_eq!(
        machine.current(),
        Some(older),
        "booting deployment {older}'s own entry makes it current"
    );
}

#[test]
#[ignore]
fn gc_keeps_every_uki_a_remaining_entry_starts() {
    let _system = lock_system();
    let body = |esp: &Path, n: u32| {
        let params = if n < 4 {
            r#""root=/dev/vda2", "rw""#
        } else {
            r#""root=/dev/vda2", "rw", "quiet""#
        };
        format!(
            r#"boot = {{ loader = "systemd-boot", esp = "{}" }}, kernel_params = {{ {params} }}, deployments = {{ keep = 2 }}, files = {{ ["/etc/archix-test-gc"] = {{ text = "{n}" }} }}"#,
            esp.display()
        )
    };
    let setup = Setup::new(&[], "");
    let esp = setup.machine.esp();
    setup.set_body(&[], &body(&esp, 0));
    setup.ok(&["switch", "--noconfirm"]);
    let machine = &setup.machine;
    let booted = machine.current().unwrap();
    machine.reboot(Some(&format!("archix-{booted}.conf")));
    for n in 1..=7 {
        setup.set_body(&[], &body(&esp, n));
        setup.ok(&["switch", "--noconfirm"]);
    }
    println!("archix gc, booted into deployment {booted}:\n{}", setup.ok(&["gc"]));
    let files = esp_files(machine);
    println!(
        "ESP after 7 switches (the kernel command line changed at the 4th) and gc:\n{}",
        files.join("\n")
    );
    let started: BTreeSet<PathBuf> = machine.entries().iter().map(|entry| uki(machine, entry)).collect();
    for uki in &started {
        assert!(
            uki.is_file(),
            "an entry starts {}, which gc deleted:\n{}",
            uki.display(),
            files.join("\n")
        );
    }
    let on_esp: BTreeSet<PathBuf> = files
        .iter()
        .filter(|f| f.starts_with("EFI/archix/"))
        .map(|f| esp.join(f))
        .collect();
    assert_eq!(on_esp, started, "every UKI left is one an entry starts");
    assert!(
        entries(machine).contains(&format!("archix-{booted}.conf")),
        "the booted deployment keeps its entry"
    );
    assert_eq!(
        started.len(),
        2,
        "the booted deployment's UKI and the one from after the command line changed"
    );
}

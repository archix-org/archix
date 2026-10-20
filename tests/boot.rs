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
    setup.set_body(&[], &(boot_body(&esp, ", secure_boot = true") + keep));
    setup.ok(&["switch", "--noconfirm"]);
    let signed = uki(machine, &format!("archix-{}.conf", machine.current().unwrap()));
    let verify = Command::new("sbctl")
        .env("ESP_PATH", &esp)
        .arg("verify")
        .arg(&signed)
        .output()
        .unwrap();
    let verify = String::from_utf8_lossy(&verify.stdout).to_string() + &String::from_utf8_lossy(&verify.stderr);
    assert!(verify.contains("is signed"), "{verify}");
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

#[test]
#[ignore]
fn eject_refuses_a_machine_laid_out_as_deployments() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    let err = setup.fails(&["eject", "--noconfirm"]);
    assert!(err.contains("holds deployments") && err.contains("ejecting"), "{err}");
    assert!(!setup.machine.entries().is_empty(), "nothing was taken off");
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
    let ukis: Vec<&String> = files.iter().filter(|f| f.ends_with(".efi")).collect();
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
    let cmdline = format!("root=/dev/vda2 rw archix.generation={booted}");
    let env = [("ARCHIX_TEST_CMDLINE", cmdline.as_str())];
    for n in 1..=7 {
        setup.set_body(&[], &body(&esp, n));
        setup.ok_env(&env, &["switch", "--noconfirm"]);
    }
    println!(
        "archix gc, booted into deployment {booted}:\n{}",
        setup.ok_env(&env, &["gc"])
    );
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

//! Builds boot entries and rescues packages; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;

use archix::generations::Store;
use common::*;

const ESP: &str = "/tmp/archix-test-esp";

fn boot_body(extra: &str) -> String {
    format!(
        r#"boot = {{ loader = "systemd-boot", esp = "{ESP}", entries = 2 {extra} }}, kernel_params = {{ "root=/dev/vda2", "rw" }}"#
    )
}

/// Entry ids, as systemd-boot reports them: the file names without their boot counters.
fn entries() -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(format!("{ESP}/loader/entries"))
        .map(|dir| {
            dir.flatten()
                .map(|e| entry_id(&e.file_name().to_string_lossy()))
                .collect()
        })
        .unwrap_or_default();
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
fn entry_file(id: &str) -> String {
    let file = fs::read_dir(format!("{ESP}/loader/entries"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|file| entry_id(file) == entry_id(id))
        .unwrap_or_else(|| panic!("no entry {id}"));
    format!("{ESP}/loader/entries/{file}")
}

fn current() -> u32 {
    Store::new(&archix::paths::Paths::system()).current().unwrap().unwrap()
}

/// The UKI a boot menu entry starts.
fn uki(entry: &str) -> String {
    let text = fs::read_to_string(entry_file(entry)).unwrap();
    let path = text
        .lines()
        .find_map(|line| line.strip_prefix("efi ").or_else(|| line.strip_prefix("uki ")))
        .unwrap()
        .trim();
    format!("{ESP}{path}")
}

fn inspect(entry: &str) -> String {
    let output = Command::new("ukify").args(["inspect", &uki(entry)]).output().unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
#[ignore]
fn signed_images_are_recorded_by_hash_and_listed_for_revoking_once_off_the_esp() {
    let _system = lock_system();
    let _ = fs::remove_dir_all(ESP);
    fs::create_dir_all(ESP).unwrap();
    if !Path::new("/var/lib/sbctl/keys/db/db.key").exists() {
        let made = Command::new("sbctl")
            .env("ESP_PATH", ESP)
            .arg("create-keys")
            .status()
            .unwrap();
        assert!(made.success());
    }
    pacman(&["-S", "--needed", "efitools"]);
    let body = |n: u32| {
        format!(
            r#"boot = {{ loader = "systemd-boot", esp = "{ESP}", entries = 1, secure_boot = true }}, kernel_params = {{ "root=/dev/vda2", "rw", "archix.test={n}" }}"#
        )
    };
    let setup = Setup::new(&[], &body(1));
    setup.ok(&["switch", "--noconfirm"]);
    let first = uki(&format!("archix-{}.conf", current()));
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
    // Two more UKIs, so pruning takes the first once it's neither new nor current.
    setup.set_body(&[], &body(2));
    setup.ok(&["switch", "--noconfirm"]);
    setup.set_body(&[], &body(3));
    setup.ok(&["switch", "--noconfirm"]);
    let gone = !Path::new(&first).exists();
    let in_use = Path::new(&uki(&format!("archix-{}.conf", current())))
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let listed = setup.run(&["secure-boot", "revoke", "--dry-run"]);
    let listed = String::from_utf8_lossy(&listed.stdout).to_string() + &String::from_utf8_lossy(&listed.stderr);
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert!(
        recorded.contains(&efitools),
        "archix records the hash efitools computes, {efitools}:\n{recorded}"
    );
    assert!(gone, "pruning removes the first UKI");
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
    let _ = fs::remove_dir_all(ESP);
    fs::create_dir_all(ESP).unwrap();
    // Every LoaderFeatures bit below Type #1 UKI entries, as systemd-boot 257 reports them, and every one 262 has.
    let old = ("ARCHIX_TEST_LOADER_FEATURES", "65535");
    let new = ("ARCHIX_TEST_LOADER_FEATURES", "4194303");
    let setup = Setup::new(&[], &boot_body(""));
    let switched = setup.ok_env(&[old], &["switch", "--noconfirm"]);
    let status_old = setup.ok_env(&[old], &["status"]);
    let status_new = setup.ok_env(&[new], &["status"]);
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    let warning = "boot counting is off: the systemd-boot that started this boot is older than 260";
    assert!(switched.contains(warning), "{switched}");
    assert!(status_old.contains(warning), "{status_old}");
    assert!(!status_new.contains("boot counting is off"), "{status_new}");
}

#[test]
#[ignore]
fn with_systemds_pcr_key_every_uki_carries_a_signed_pcr_policy() {
    let _system = lock_system();
    fs::create_dir_all(ESP).unwrap();
    let (private, public) = (
        "/etc/systemd/tpm2-pcr-private-key.pem",
        "/etc/systemd/tpm2-pcr-public-key.pem",
    );
    let setup = Setup::new(&["tree"], &boot_body(""));
    setup.ok(&["switch", "--noconfirm"]);
    let unsigned = inspect(&format!("archix-{}.conf", current()));
    let openssl = |args: &[&str]| assert!(Command::new("openssl").args(args).status().unwrap().success());
    openssl(&[
        "genpkey",
        "-quiet",
        "-algorithm",
        "RSA",
        "-pkeyopt",
        "rsa_keygen_bits:2048",
        "-out",
        private,
    ]);
    openssl(&["pkey", "-in", private, "-pubout", "-out", public]);
    // Same kernel, initramfs and command line, so only the key tells this UKI from the last one.
    setup.set_body(&[], &boot_body(""));
    let out = setup.ok(&["switch", "--noconfirm"]);
    let signed = inspect(&format!("archix-{}.conf", current()));
    fs::remove_file(private).unwrap();
    fs::remove_file(public).unwrap();
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert!(!unsigned.contains(".pcrsig"), "{unsigned}");
    assert!(
        signed.contains(".pcrsig") && signed.contains(".pcrpkey"),
        "{signed}\n{out}"
    );
}

#[test]
#[ignore]
fn every_generation_gets_a_signed_boot_entry_and_boots_back() {
    let _system = lock_system();
    fs::create_dir_all(ESP).unwrap();
    let setup = Setup::new(&["tree"], &boot_body(""));
    setup.ok(&["switch", "--noconfirm"]);
    let with_tree = current();
    let entry = format!("archix-{with_tree}.conf");
    assert!(entries().contains(&entry), "{:?}", entries());
    let info = inspect(&entry);
    assert!(info.contains("root=/dev/vda2"), "{info}");
    let menu = fs::read_to_string(entry_file(&entry)).unwrap();
    assert!(
        menu.contains(&format!("generation {with_tree}")),
        "the entry's title names the generation:\n{menu}"
    );
    let kernel = fs::read_to_string(format!("/var/lib/archix/boot/{with_tree}.kernel")).unwrap();
    assert!(
        Path::new(&format!("/var/lib/archix/kernels/{kernel}/vmlinuz")).exists(),
        "the kernel's modules are kept"
    );
    assert!(Path::new("/etc/systemd/system/sysinit.target.wants/archix-boot.service").exists());

    setup.set_body(&[], &boot_body(""));
    setup.ok(&["switch", "--noconfirm"]);
    let without_tree = current();
    setup.set_body(&["tree"], &boot_body(""));
    setup.ok(&["switch", "--noconfirm"]);
    let newest = current();
    let kept: BTreeSet<String> = entries().into_iter().filter(|e| !e.contains("rescue")).collect();
    let expected = BTreeSet::from([format!("archix-{newest}.conf"), format!("archix-{without_tree}.conf")]);
    assert_eq!(kept, expected, "only the newest 2");

    setup.ok(&[
        "boot-check",
        "--cmdline",
        &format!("root=/dev/vda2 archix.generation={without_tree}"),
    ]);
    assert_eq!(current(), without_tree, "booting an older entry makes it current");
    assert!(installed("tree").is_none());

    assert!(
        Command::new("sbctl")
            .env("ESP_PATH", ESP)
            .arg("create-keys")
            .status()
            .unwrap()
            .success()
    );
    setup.set_body(&[], &boot_body(", secure_boot = true"));
    setup.ok(&["switch", "--noconfirm"]);
    let signed = uki(&format!("archix-{}.conf", current()));
    let verify = Command::new("sbctl")
        .env("ESP_PATH", ESP)
        .args(["verify", &signed])
        .output()
        .unwrap();
    let verify = String::from_utf8_lossy(&verify.stdout).to_string() + &String::from_utf8_lossy(&verify.stderr);
    assert!(verify.contains("is signed"), "{verify}");

    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert!(!Path::new("/etc/systemd/system/sysinit.target.wants/archix-boot.service").exists());
}

#[test]
#[ignore]
fn the_first_switch_with_boot_entries_already_has_a_rescue_entry() {
    let _system = lock_system();
    let _ = fs::remove_dir_all(ESP);
    fs::create_dir_all(ESP).unwrap();
    // A machine where archix has never managed booting: the package's static binary, but none of the hook files yet.
    let staticbin = Path::new("/usr/lib/archix/archix-static");
    fs::create_dir_all(staticbin.parent().unwrap()).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_archix"), staticbin).unwrap();
    for hook in [
        "/etc/initcpio/install/archix-rescue",
        "/etc/initcpio/hooks/archix-rescue",
        "/usr/lib/archix/archix-rescue.service",
        "/etc/mkinitcpio.conf.d/archix-rescue.conf",
    ] {
        let _ = fs::remove_file(hook);
    }
    let setup = Setup::new(&[], &boot_body(""));
    setup.ok(&["switch", "--noconfirm"]);
    let first = entries();
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    let _ = fs::remove_file(staticbin);
    assert!(
        first.iter().any(|entry| entry.ends_with("-rescue.conf")),
        "a power cut in the next switch needs the rescue hook in this generation's initramfs: {first:?}"
    );
}

#[test]
#[ignore]
fn eject_hands_booting_to_a_uki_mkinitcpio_keeps_building() {
    let _system = lock_system();
    let _ = fs::remove_dir_all(ESP);
    fs::create_dir_all(ESP).unwrap();
    let preset = "/etc/mkinitcpio.d/linux.preset";
    let original = fs::read_to_string(preset).unwrap();
    let setup = Setup::new(&[], &boot_body(""));
    setup.ok(&["switch", "--noconfirm"]);
    assert!(entries().iter().any(|e| e.starts_with("archix-")));

    let out = setup.ok(&["eject", "--noconfirm"]);
    let ejected = fs::read_to_string(preset).unwrap();
    fs::write(preset, &original).unwrap();
    assert!(entries().is_empty(), "archix's entries go: {:?}\n{out}", entries());
    assert!(!Path::new(&format!("{ESP}/EFI/archix")).exists());
    assert!(
        ejected.contains(&format!("default_uki=\"{ESP}/EFI/Linux/arch-linux.efi\"")),
        "{ejected}"
    );
    let info = Command::new("ukify")
        .args(["inspect", &format!("{ESP}/EFI/Linux/arch-linux.efi")])
        .output()
        .unwrap();
    let info = String::from_utf8_lossy(&info.stdout);
    assert!(info.contains("root=/dev/vda2 rw"), "{info}");
    let loader = fs::read_to_string(format!("{ESP}/loader/loader.conf")).unwrap();
    assert!(loader.lines().any(|line| line == "default arch-linux.efi"), "{loader}");
    for gone in [
        "/etc/systemd/system/archix-boot.service",
        "/etc/systemd/system/sysinit.target.wants/archix-boot.service",
        "/etc/systemd/system/archix-boot-complete.service",
        "/etc/systemd/system/multi-user.target.wants/archix-boot-complete.service",
        "/etc/pacman.d/hooks/archix.hook",
        "/etc/profile.d/archix.sh",
        "/etc/xdg/autostart/archix-status.desktop",
        "/etc/mkinitcpio.conf.d/archix-rescue.conf",
    ] {
        assert!(!Path::new(gone).exists(), "{gone} is left\n{out}");
    }

    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
}

/// Every file on the test ESP except systemd-boot's own, relative to it.
fn esp_files() -> Vec<String> {
    fn walk(dir: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.push(path.strip_prefix(ESP).unwrap().display().to_string());
            }
        }
    }
    let mut files = Vec::new();
    walk(Path::new(ESP), &mut files);
    files.retain(|f| !f.starts_with("EFI/systemd/") && !f.starts_with("EFI/BOOT/"));
    files.sort();
    files
}

#[test]
#[ignore]
fn adding_packages_one_at_a_time_reuses_one_uki() {
    let _system = lock_system();
    let _ = fs::remove_dir_all(ESP);
    fs::create_dir_all(ESP).unwrap();
    let body = format!(
        r#"boot = {{ loader = "systemd-boot", esp = "{ESP}", entries = 20 }}, kernel_params = {{ "root=/dev/vda2", "rw" }}"#
    );
    let setup = Setup::new(&[], &body);
    setup.ok(&["switch", "--noconfirm"]);
    let first = current();
    for count in 1..=UKI_TEST_PACKAGES.len() {
        setup.set_body(&UKI_TEST_PACKAGES[..count], &body);
        setup.ok(&["switch", "--noconfirm"]);
    }
    let files = esp_files();
    let listing = files
        .iter()
        .map(|f| {
            let size = fs::metadata(format!("{ESP}/{f}")).map_or(0, |m| m.len());
            format!("{size:>11}  {f}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    println!(
        "ESP after {} package adds (generations {first}..={}):\n{listing}",
        UKI_TEST_PACKAGES.len(),
        current()
    );
    let ukis: Vec<&String> = files
        .iter()
        .filter(|f| f.ends_with(".efi") && !f.contains("rescue"))
        .collect();
    assert_eq!(
        ukis.len(),
        1,
        "one UKI for an unchanged kernel and initramfs:\n{listing}"
    );
    // bootctl refuses an ESP that isn't a FAT partition, as this one isn't; the VM boot suite has systemd-boot read real entries.
    for generation in first..=current() {
        let entry = format!("archix-{generation}.conf");
        let menu = fs::read_to_string(entry_file(&entry)).unwrap_or_default();
        println!("{entry}:\n{menu}");
        assert!(
            menu.contains(&format!("generation {generation})")) && Path::new(&uki(&entry)).is_file(),
            "generation {generation} lost its own boot menu entry:\n{menu}"
        );
    }
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
}

#[test]
#[ignore]
fn without_a_trustworthy_booted_entry_boot_check_rolls_nothing_back() {
    let _system = lock_system();
    let _ = fs::remove_dir_all(ESP);
    fs::create_dir_all(ESP).unwrap();
    let body = format!(
        r#"boot = {{ loader = "systemd-boot", esp = "{ESP}", entries = 5 }}, kernel_params = {{ "root=/dev/vda2", "rw" }}"#
    );
    let setup = Setup::new(&[], &body);
    setup.ok(&["switch", "--noconfirm"]);
    let older = current();
    setup.set_body(&["tree"], &body);
    setup.ok(&["switch", "--noconfirm"]);
    let newest = current();
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
        assert_eq!(current(), newest, "{case}: nothing may roll back");
    }
    let entry = format!("archix-{older}.conf");
    // Someone with write access to the ESP edits the entry that booted, here to start another generation's UKI.
    let file = entry_file(&entry);
    let written = fs::read_to_string(&file).unwrap();
    fs::write(
        &file,
        written.replace(&format!("version {older}"), &format!("version {newest}")),
    )
    .unwrap();
    let out = setup.ok_env(&[cmdline, ("ARCHIX_TEST_ENTRY", entry.as_str())], &["boot-check"]);
    println!("boot-check with an edited {entry}:\n{out}");
    assert_eq!(
        current(),
        newest,
        "an entry that isn't what archix wrote may not roll anything back"
    );
    assert!(out.contains("isn't the entry archix wrote"), "{out}");
    fs::write(&file, written).unwrap();
    let out = setup.ok_env(&[cmdline, ("ARCHIX_TEST_ENTRY", entry.as_str())], &["boot-check"]);
    println!("boot-check with {entry}:\n{out}");
    assert_eq!(
        current(),
        older,
        "booting generation {older}'s own entry rolls back to it"
    );
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
}

#[test]
#[ignore]
fn gc_keeps_every_uki_a_remaining_entry_starts() {
    let _system = lock_system();
    let _ = fs::remove_dir_all(ESP);
    fs::create_dir_all(ESP).unwrap();
    let body = |n: u32| {
        let params = if n < 8 {
            r#""root=/dev/vda2", "rw""#
        } else {
            r#""root=/dev/vda2", "rw", "quiet""#
        };
        format!(
            r#"boot = {{ loader = "systemd-boot", esp = "{ESP}", entries = 5 }}, kernel_params = {{ {params} }}, generations = {{ keep = 3, keep_days = 0 }}, files = {{ ["/etc/archix-test-gc"] = {{ text = "{n}" }} }}"#
        )
    };
    let setup = Setup::new(&[], &body(0));
    setup.ok(&["switch", "--noconfirm"]);
    let booted = current();
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
    let files = esp_files();
    println!(
        "ESP after 15 switches (the kernel command line changed at the 8th) and gc:\n{}",
        files.join("\n")
    );
    let entries: Vec<&String> = files
        .iter()
        .filter(|f| f.starts_with("loader/entries/archix-"))
        .collect();
    let started: BTreeSet<String> = entries
        .iter()
        .map(|entry| uki(entry.trim_start_matches("loader/entries/")))
        .collect();
    for uki in &started {
        assert!(
            Path::new(uki).is_file(),
            "an entry starts {uki}, which gc deleted:\n{}",
            files.join("\n")
        );
    }
    let on_esp: BTreeSet<String> = files
        .iter()
        .filter(|f| f.starts_with("EFI/archix/"))
        .map(|f| format!("{ESP}/{f}"))
        .collect();
    assert_eq!(on_esp, started, "every UKI left is one an entry starts");
    assert!(
        crate::entries().contains(&format!("archix-{booted}.conf")),
        "the booted generation keeps its entry"
    );
    assert_eq!(
        started.len(),
        2,
        "the booted generation's UKI and the one from after the command line changed"
    );
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
}

#[test]
#[ignore]
fn rescue_restores_packages_without_pacman() {
    let _system = lock_system();
    let setup = Setup::new(&["tree"], "");
    setup.ok(&["switch", "--noconfirm"]);
    let number = current();
    let generation = Store::new(&archix::paths::Paths::system()).get(number).unwrap();
    let tree = &generation.packages["tree"];

    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("var/lib/archix/generations").join(number.to_string());
    fs::create_dir_all(&state).unwrap();
    fs::copy(
        format!("/var/lib/archix/generations/{number}/generation.json"),
        state.join("generation.json"),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        format!("generations/{number}"),
        root.path().join("var/lib/archix/current"),
    )
    .unwrap();
    let cache = root.path().join("var/cache/archix/pkg");
    fs::create_dir_all(&cache).unwrap();
    fs::copy(format!("/var/cache/archix/pkg/{}", tree.file), cache.join(&tree.file)).unwrap();

    let out = setup.ok(&["rescue", "--root", root.path().to_str().unwrap(), "--package", "tree"]);
    assert!(out.contains(&format!("restored tree {}", tree.version)), "{out}");
    assert!(root.path().join("usr/bin/tree").exists());
    let dbpath = root.path().join("var/lib/pacman");
    let query = Command::new("pacman")
        .args([
            "--root",
            root.path().to_str().unwrap(),
            "--dbpath",
            dbpath.to_str().unwrap(),
            "-Qk",
            "tree",
        ])
        .output()
        .unwrap();
    let query = String::from_utf8_lossy(&query.stdout);
    assert!(
        query.contains("0 missing files"),
        "pacman sees the rescued package and all its files:\n{query}"
    );
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
}

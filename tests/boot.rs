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

fn entries() -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(format!("{ESP}/EFI/Linux"))
        .map(|dir| {
            dir.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn current() -> u32 {
    Store::new(&archix::paths::Paths::system()).current().unwrap().unwrap()
}

fn inspect(entry: &str) -> String {
    let output = Command::new("ukify")
        .args(["inspect", &format!("{ESP}/EFI/Linux/{entry}")])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
#[ignore]
fn every_generation_gets_a_signed_boot_entry_and_boots_back() {
    let _system = lock_system();
    fs::create_dir_all(ESP).unwrap();
    let setup = Setup::new(&["tree"], &boot_body(""));
    setup.ok(&["switch", "--noconfirm"]);
    let with_tree = current();
    let entry = format!("archix-{with_tree}.efi");
    assert!(entries().contains(&entry), "{:?}", entries());
    let info = inspect(&entry);
    assert!(
        info.contains(&format!("archix.generation={with_tree}")) && info.contains("root=/dev/vda2"),
        "{info}"
    );
    assert!(
        info.contains(&format!("generation {with_tree}")),
        "the entry's title names the generation:\n{info}"
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
    let expected = BTreeSet::from([format!("archix-{newest}.efi"), format!("archix-{without_tree}.efi")]);
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
    let signed = format!("{ESP}/EFI/Linux/archix-{}.efi", current());
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

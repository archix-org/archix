//! Reads the real system; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

mod common;

use std::fs;
use std::path::Path;
use std::process::Command;

use common::*;

fn archix(config: &str, args: &[&str]) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_archix"))
        .args(["--config", config, "--host", "initbox"])
        .args(args)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), text)
}

/// A partial upgrade: lemon from a later snapshot than the rest of the image.
fn install_newer_lemon() {
    let file = std::env::temp_dir().join("lemon-3.54.0-1-x86_64.pkg.tar.zst");
    if !file.exists() {
        let url = "https://archive.archlinux.org/repos/2026/10/14/core/os/x86_64/lemon-3.54.0-1-x86_64.pkg.tar.zst";
        assert!(
            archix::archive::Archive::new(archix::archive::ARCHIVE_URL)
                .fetch(url, &file)
                .unwrap()
        );
    }
    pacman(&["-U", file.to_str().unwrap()]);
}

#[test]
#[ignore]
fn init_writes_explicit_packages_services_and_users_locked_to_what_is_installed() {
    let _system = lock_system();
    install_newer_lemon();
    let user = Command::new("useradd").args(["-m", "dotuser"]).status().unwrap();
    assert!(user.success());
    fs::write("/home/dotuser/.bashrc", "alias ll='ls -l'\n").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("cfg");
    let config = config.to_str().unwrap();

    let (ok, out) = archix(config, &["init"]);
    assert!(ok, "init failed:\n{out}");
    let lock = archix::lock::Lockfile::load(&dir.path().join("cfg/archix.lock"))
        .unwrap()
        .hosts["initbox"]
        .clone();
    assert_eq!(
        lock.date.to_string(),
        IMAGE_SNAPSHOT,
        "one newer package doesn't move the snapshot"
    );
    assert_eq!(lock.pinned, ["lemon"]);
    assert_eq!(lock.packages["lemon"].version, "3.54.0-1");
    let host = fs::read_to_string(format!("{config}/lua/hosts/initbox/init.lua")).unwrap();
    assert!(host.contains("dotuser = "), "users are declared:\n{host}");
    assert!(!host.contains("files = {") && !host.contains("home = {"), "init scrapes no files or dotfiles:\n{host}");
    assert!(!Path::new(&format!("{config}/files")).exists() && !Path::new(&format!("{config}/home")).exists());
    let packages = archix::installed::read(Path::new(&format!("{config}/lua/hosts/initbox/installed.lua"))).unwrap();
    assert!(packages.iter().any(|p| p == "base-devel"), "{packages:?}");

    let (ok, again) = archix(config, &["init"]);
    assert!(!ok && again.contains("already has an init.lua"), "{again}");
    pacman(&["-R", "lemon"]);
    assert!(
        Command::new("userdel")
            .args(["-r", "dotuser"])
            .status()
            .unwrap()
            .success()
    );
}

//! Reads the real system; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
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

const HAND_WRITTEN: &str = "/etc/sysctl.d/50-init-test.conf";

fn write_hand_written() {
    fs::create_dir_all("/etc/sysctl.d").unwrap();
    fs::write(HAND_WRITTEN, "vm.swappiness = 10\n").unwrap();
    fs::set_permissions(HAND_WRITTEN, fs::Permissions::from_mode(0o644)).unwrap();
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
fn init_describes_the_machine_so_nothing_needs_to_change() {
    let _system = lock_system();
    write_hand_written();
    install_newer_lemon();
    let user = Command::new("useradd").args(["-m", "dotuser"]).status().unwrap();
    assert!(user.success());
    fs::write("/home/dotuser/.bashrc", "alias ll='ls -l'\n").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("cfg");
    let config = config.to_str().unwrap();

    let (ok, out) = archix(config, &["init", "--defaults"]);
    fs::remove_file(HAND_WRITTEN).unwrap();
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
    assert!(
        out.contains("the system matches the lock and the config"),
        "init's own diff isn't empty:\n{out}"
    );
    let host = fs::read_to_string(format!("{config}/lua/hosts/initbox/init.lua")).unwrap();
    assert!(
        host.contains(
            "[\"/etc/sysctl.d/50-init-test.conf\"] = { source = \"files/initbox/etc/sysctl.d/50-init-test.conf\" }"
        ),
        "hand-written config files are captured:\n{host}"
    );
    assert_eq!(
        fs::read_to_string(format!("{config}/files/initbox/etc/sysctl.d/50-init-test.conf")).unwrap(),
        "vm.swappiness = 10\n"
    );
    assert!(host.contains("dotuser = "), "users are declared:\n{host}");
    assert!(!host.contains("home = {"), "--defaults never imports dotfiles:\n{host}");
    assert!(
        !fs::symlink_metadata("/home/dotuser/.bashrc")
            .unwrap()
            .file_type()
            .is_symlink()
            && !std::path::Path::new(&format!("{config}/home")).exists()
    );
    let packages = archix::installed::read(std::path::Path::new(&format!(
        "{config}/lua/hosts/initbox/installed.lua"
    )))
    .unwrap();
    assert!(packages.iter().any(|p| p == "base-devel"), "{packages:?}");

    write_hand_written();
    let (ok, diff) = archix(config, &["diff"]);
    fs::remove_file(HAND_WRITTEN).unwrap();
    assert!(ok, "{diff}");
    assert!(diff.contains("the system matches the lock and the config"), "{diff}");

    let (ok, again) = archix(config, &["init", "--defaults"]);
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

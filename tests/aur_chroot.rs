//! SCOPE 2.4's chroot builder on a real system; run only in the test VM via `tests/arch/run.sh`.

mod common;

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use archix::aur::chroot::DevtoolsBuilder;
use archix::plan::AurItem;
use common::*;
use tempfile::TempDir;

fn installed_names() -> Vec<String> {
    let output = Command::new("pacman").arg("-Qq").output().unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

fn file_in_package(package: &Path, path: &str) -> Option<String> {
    let decoder = zstd::stream::read::Decoder::new(fs::File::open(package).unwrap()).unwrap();
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        if entry.path().unwrap().to_string_lossy() == path {
            let mut text = String::new();
            entry.read_to_string(&mut text).unwrap();
            return Some(text);
        }
    }
    None
}

#[test]
#[ignore]
fn a_chroot_build_gets_its_makedepends_and_the_host_gains_none() {
    let _system = lock_system();
    let mut aur = LocalAur::new();
    aur.publish(TestPkg::new("archix-test-chroot-lib", "1.0"));
    aur.publish(TestPkg {
        makedepends: &["lemon", "archix-test-chroot-lib"],
        build: "test -f /usr/share/archix-test-chroot-lib/version && command -v lemon > \"$srcdir/marker\"",
        package: "install -Dm644 \"$srcdir/marker\" \"$pkgdir/usr/share/archix-test-chroot/marker\"",
        ..TestPkg::new("archix-test-chroot", "1.0")
    });
    let built_names = ["archix-test-chroot", "archix-test-chroot-lib"];
    let before = installed_names();
    assert!(!before.iter().any(|n| n == "lemon"), "the VM starts without lemon");

    let work = TempDir::new_in("/var/tmp").unwrap();
    let builder = DevtoolsBuilder {
        aur_url: aur.url(),
        cache: work.path().join("cache"),
        chroot: work.path().join("chroot"),
        log_dir: work.path().join("log"),
        ..DevtoolsBuilder::default()
    };
    let item = |name: &str| AurItem {
        name: name.to_string(),
        base: None,
        commit: Some(aur.commit(name, 0)),
        helper: None,
        reason: "declared".into(),
        risk: Vec::new(),
    };
    let files: Vec<PathBuf> = builder
        .build(
            &[item("archix-test-chroot"), item("archix-test-chroot-lib")],
            Path::new("/etc/pacman.conf"),
            Path::new("/var/lib/pacman/sync"),
        )
        .unwrap();

    let names: Vec<String> = files
        .iter()
        .map(|f| f.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        [
            "archix-test-chroot-lib-1.0-1-any.pkg.tar.zst",
            "archix-test-chroot-1.0-1-any.pkg.tar.zst"
        ],
        "built in dependency order"
    );
    let marker = file_in_package(&files[1], "usr/share/archix-test-chroot/marker").unwrap_or_default();
    assert!(
        marker.trim().ends_with("bin/lemon"),
        "the build had its makedepend: {marker:?}"
    );
    let after: Vec<String> = installed_names()
        .into_iter()
        .filter(|n| !built_names.contains(&n.as_str()))
        .collect();
    assert_eq!(before, after, "the host's packages are unchanged");
}

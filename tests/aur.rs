//! Builds AUR packages in a clean chroot; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

mod common;

use std::fs;
use std::process::Command;

use archix::generations::Store;
use archix::lock::AUR_REPO;
use common::*;
use tempfile::TempDir;

fn read(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_default().trim().to_string()
}

#[test]
#[ignore]
fn aur_packages_are_locked_built_reviewed_and_fall_back() {
    let _system = lock_system();
    let mut aur = LocalAur::new();
    aur.publish(TestPkg::new("archix-test-lib", "1.0"));
    aur.publish(TestPkg {
        package: "install -Dm755 /dev/stdin \"$pkgdir/usr/bin/archix-test-builddep\" <<'EOS'\n#!/bin/sh\necho built-with-builddep\nEOS",
        ..TestPkg::new("archix-test-builddep", "1.0")
    });
    aur.publish(TestPkg {
        depends: &["archix-test-lib", "jq"],
        makedepends: &["archix-test-builddep"],
        build: "archix-test-builddep > \"$srcdir/marker\"",
        package: "install -Dm644 \"$srcdir/marker\" \"$pkgdir/usr/share/archix-test-app/marker\"",
        ..TestPkg::new("archix-test-app", "2.0")
    });
    let body = format!("aur = {{ url = {:?} }}", aur.url());

    let setup = Setup::new(&["archix-test-app"], &body);
    let lock = setup.lock();
    let app = &lock.packages["archix-test-app"];
    assert_eq!(
        (app.repo.as_str(), app.version.as_str(), app.explicit),
        (AUR_REPO, "2.0-1", true)
    );
    assert!(app.commit.as_ref().is_some_and(|c| c.len() == 40));
    let lib = &lock.packages["archix-test-lib"];
    assert!(
        !lib.explicit && !lib.build_only,
        "a runtime AUR dependency is installed as a dependency"
    );
    assert!(
        lock.packages["archix-test-builddep"].build_only,
        "a make-only AUR dependency is only built"
    );
    assert_ne!(
        lock.packages["jq"].repo, AUR_REPO,
        "repo dependencies of AUR packages come from the repos"
    );

    for entry in fs::read_dir("/var/cache/pacman/pkg").unwrap().flatten() {
        if entry.file_name().to_string_lossy().starts_with("jq-") {
            fs::remove_file(entry.path()).unwrap();
        }
    }
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(
        out.contains("downloading jq-") && !out.contains("Retrieving packages"),
        "archix fetches the chroot and its repo dependencies itself, with retries, so pacman in the chroot downloads nothing:\n{out}"
    );
    assert_eq!(installed("archix-test-app").as_deref(), Some("2.0-1"));
    assert_eq!(installed("archix-test-lib").as_deref(), Some("1.0-1"));
    assert!(installed("jq").is_some());
    assert!(
        installed("archix-test-builddep").is_none(),
        "build-only packages stay out of the system"
    );
    assert_eq!(
        read("/usr/share/archix-test-app/marker"),
        "built-with-builddep",
        "the chroot had the build tool"
    );
    let store = Store::new(&setup.paths);
    let generation = store.get(store.current().unwrap().unwrap()).unwrap();
    assert_eq!(
        generation.packages["archix-test-app"].sha256.len(),
        64,
        "the generation records the built file"
    );
    assert!(
        setup.ok(&["switch", "--noconfirm"]).contains("nothing to do"),
        "built packages are reused"
    );

    aur.publish(TestPkg::new("archix-test-lib", "1.1"));
    let declined = setup.run(&["update", "--to", IMAGE_SNAPSHOT]);
    assert!(
        !declined.status.success(),
        "with no answer, a changed build recipe is not trusted"
    );
    assert!(String::from_utf8_lossy(&declined.stdout).contains("archix-test-lib: build recipe changed"));
    assert_eq!(setup.lock().packages["archix-test-lib"].version, "1.0-1");
    let out = setup.answering("y\n", &["update", "--to", IMAGE_SNAPSHOT]);
    assert!(
        out.contains("echo 1.1") || out.contains("pkgver=1.1"),
        "the review shows the diff:\n{out}"
    );
    assert!(
        out.contains("pkgver = 1.1"),
        "the review includes .SRCINFO, which archix takes dependencies and sources from:\n{out}"
    );
    assert_eq!(setup.lock().packages["archix-test-lib"].version, "1.1-1");

    aur.publish(TestPkg {
        build: "false",
        ..TestPkg::new("archix-test-lib", "1.2")
    });
    setup.ok(&["update", "--to", IMAGE_SNAPSHOT, "--noconfirm"]);
    aur.publish(TestPkg::new("archix-test-lib", "1.3"));
    // As after a devtools upgrade that bumps CHROOT_VERSION: arch-nspawn would refuse the old chroot.
    fs::write("/var/lib/archix/chroot/root/.arch-chroot", "v1\n").unwrap();
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(out.contains("recreating the build chroot"), "{out}");
    assert!(out.contains("archix-test-lib 1.2-1 can't be built"), "{out}");
    assert_eq!(
        installed("archix-test-lib").as_deref(),
        Some("1.3-1"),
        "--noconfirm takes the nearest newer version"
    );
    assert_eq!(
        setup.lock().packages["archix-test-lib"].version,
        "1.3-1",
        "the lock follows"
    );

    aur.publish(TestPkg {
        build: "false",
        ..TestPkg::new("archix-test-lib", "1.4")
    });
    setup.ok(&["update", "--to", IMAGE_SNAPSHOT, "--noconfirm"]);
    setup.ok(&["switch", "--noconfirm", "--accept-nearest", "older"]);
    assert_eq!(
        installed("archix-test-lib").as_deref(),
        Some("1.3-1"),
        "the nearest older version that differs"
    );

    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert!(installed("archix-test-app").is_none() && installed("archix-test-lib").is_none());
}

#[test]
#[ignore]
fn real_aur_packages_resolve() {
    let _system = lock_system();
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("init.lua"),
        r#"return { packages = { "base", "yay-bin" } }"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_archix"))
        .args([
            "--config",
            dir.path().to_str().unwrap(),
            "--host",
            "box",
            "update",
            "--to",
            IMAGE_SNAPSHOT,
        ])
        .arg("--noconfirm")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let lock = archix::lock::Lockfile::load(&dir.path().join("archix.lock")).unwrap();
    let yay = &lock.hosts["box"].packages["yay-bin"];
    assert_eq!(yay.repo, AUR_REPO);
    assert_eq!(yay.base.as_deref(), Some("yay-bin"));
}

#[test]
#[ignore]
fn skip_check_builds_a_recipe_whose_tests_fail() {
    let _system = lock_system();
    let mut aur = LocalAur::new();
    aur.publish(TestPkg {
        build: "true\n}\ncheck() {\n  false",
        ..TestPkg::new("archix-test-failing-check", "1.0")
    });
    let body = format!("aur = {{ url = {:?} }}", aur.url());
    let setup = Setup::new(&["archix-test-failing-check"], &body);
    let out = setup.fails(&["switch", "--noconfirm"]);
    assert!(out.contains("can't be built"), "{out}");
    let skipping = format!(
        "aur = {{ url = {:?}, skip_check = {{ \"archix-test-failing-check\" }} }}",
        aur.url()
    );
    setup.set_body(&["archix-test-failing-check"], &skipping);
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(installed("archix-test-failing-check").as_deref(), Some("1.0-1"));
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
}

#[test]
#[ignore]
fn a_package_cache_shares_builds_and_package_files_between_machines() {
    let _system = lock_system();
    let mut aur = LocalAur::new();
    aur.publish(TestPkg::new("archix-test-cached", "1.0"));
    let cache = TempDir::new().unwrap();
    let body = |archive: &str| {
        format!(
            "aur = {{ url = {:?} }}, package_caches = {{ {:?} }}{archive}",
            aur.url(),
            format!("file://{}", cache.path().display())
        )
    };
    let setup = Setup::new(&["archix-test-cached", "tree"], &body(""));
    setup.ok(&["switch", "--noconfirm"]);
    setup.ok(&["cache", "push", cache.path().to_str().unwrap()]);

    // Another machine: none of it built or downloaded locally, and the archive unreachable.
    setup.set_body(&[], &body(""));
    setup.ok(&["switch", "--noconfirm"]);
    let _ = fs::remove_dir_all("/var/cache/archix/aur-builds");
    for dir in ["/var/cache/archix/pkg", "/var/cache/pacman/pkg"] {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.contains("archix-test-cached") || name.starts_with("tree-") {
                fs::remove_file(entry.path()).unwrap();
            }
        }
    }
    // The other machine gets the committed lock, so its switch resolves nothing and needs no archive; locking an AUR package would.
    setup.set_body(&["archix-test-cached", "tree"], &body(""));
    let offline = body(", archive_url = \"http://127.0.0.1:9\"");
    let init = setup.read("init.lua").unwrap();
    fs::write(setup.dir.path().join("init.lua"), init.replace(&body(""), &offline)).unwrap();
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(
        out.contains("build from the package cache"),
        "the AUR build came from the cache:\n{out}"
    );
    assert!(installed("archix-test-cached").is_some() && installed("tree").is_some());
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
}

#[test]
#[ignore]
fn an_aur_package_standing_in_for_a_repo_package_is_used_only_when_declared() {
    let _system = lock_system();
    let mut aur = LocalAur::new();
    aur.publish(TestPkg {
        provides: &["glibc=9.99"],
        ..TestPkg::new("archix-test-libc", "9.99")
    });
    aur.publish(TestPkg {
        depends: &["glibc>=9"],
        ..TestPkg::new("archix-test-needs-libc9", "1.0")
    });
    let setup = Setup::new(&[], &format!("aur = {{ url = {:?} }}", aur.url()));
    let before = setup.read("archix.lock").unwrap();
    let init = setup.read("init.lua").unwrap();
    let with = |names: &str| init.replacen("packages = { ", &format!("packages = {{ {names}"), 1);
    fs::write(
        setup.dir.path().join("init.lua"),
        with(r#""archix-test-needs-libc9", "#),
    )
    .unwrap();
    let output = setup.run(&["lock", "--noconfirm"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!("archix lock, a package needing glibc>=9 that only an AUR stand-in provides:\n{text}");
    assert!(
        !output.status.success(),
        "the stand-in wasn't declared, so locking must fail:\n{text}"
    );
    assert!(
        text.contains("archix-test-libc") && text.contains("standing in for glibc"),
        "{text}"
    );
    assert_eq!(setup.read("archix.lock").unwrap(), before);

    fs::write(
        setup.dir.path().join("init.lua"),
        with(r#""archix-test-needs-libc9", "archix-test-libc", "#),
    )
    .unwrap();
    let out = setup.ok(&["lock", "--noconfirm"]);
    println!("archix lock with the stand-in declared:\n{out}");
    let lock = setup.lock();
    assert!(lock.packages.contains_key("archix-test-libc") && lock.packages.contains_key("archix-test-needs-libc9"));
    fs::write(setup.dir.path().join("init.lua"), init).unwrap();
    fs::write(setup.dir.path().join("archix.lock"), before).unwrap();
}

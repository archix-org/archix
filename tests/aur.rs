//! Builds AUR packages in a clean chroot; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

mod common;

use std::fs;
use std::process::Command;

use archix::lock::AUR_REPO;
use common::*;
use tempfile::TempDir;

/// A file of the system that boots next.
fn read(path: &str) -> String {
    fs::read_to_string(system_path(path))
        .unwrap_or_default()
        .trim()
        .to_string()
}

#[test]
#[ignore]
fn a_failed_build_moves_to_an_older_version_only_when_told_to() {
    let _system = lock_system();
    let mut aur = LocalAur::new();
    aur.publish(TestPkg::new("archix-test-older", "1.0"));
    aur.publish(TestPkg {
        build: "false",
        ..TestPkg::new("archix-test-older", "1.1")
    });
    let body = format!("aur = {{ url = {:?} }}", aur.url());
    let setup = Setup::new(&["archix-test-older"], &body);
    assert_eq!(setup.lock().packages["archix-test-older"].version, "1.1-1");
    let text = |output: &std::process::Output| {
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    };

    let entered = setup.answering_raw("\n\n\n", &["switch"]);
    let out = text(&entered);
    assert!(!entered.status.success(), "an empty answer doesn't downgrade:\n{out}");
    assert!(
        out.contains("DOWNGRADE archix-test-older from 1.1-1 to the OLDER 1.0-1? [y/N]"),
        "{out}"
    );
    assert_eq!(installed("archix-test-older"), None, "{out}");
    assert_eq!(setup.lock().packages["archix-test-older"].version, "1.1-1");

    let unattended = setup.run(&["switch", "--noconfirm"]);
    let out = text(&unattended);
    assert!(!unattended.status.success(), "--noconfirm doesn't downgrade:\n{out}");
    assert!(
        out.contains("archix-test-older 1.0-1 is OLDER than 1.1-1") && out.contains("--accept-nearest=older"),
        "{out}"
    );
    assert_eq!(installed("archix-test-older"), None, "{out}");

    let halfway = setup.answering_raw("y\ny\n\n", &["switch"]);
    let out = text(&halfway);
    assert!(
        !halfway.status.success(),
        "the recipe of an older version isn't trusted by default:\n{out}"
    );
    assert!(
        out.contains("an OLDER version (1.1-1 -> 1.0-1), and lock it? [y/N]"),
        "{out}"
    );
    assert_eq!(setup.lock().packages["archix-test-older"].version, "1.1-1");

    let out = setup.answering("y\ny\ny\n", &["switch"]);
    assert!(
        out.contains("archix-test-older: moving from 1.1-1 to 1.0-1, an OLDER version"),
        "{out}"
    );
    assert_eq!(installed("archix-test-older").as_deref(), Some("1.0-1"));
    assert_eq!(setup.lock().packages["archix-test-older"].version, "1.0-1");

    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert!(installed("archix-test-older").is_none());
}

#[test]
#[ignore]
fn a_build_on_the_host_gets_its_build_dependencies_only_for_the_build() {
    let _system = lock_system();
    let mut aur = LocalAur::new();
    aur.publish(TestPkg {
        makedepends: &["lemon"],
        build: "command -v lemon > \"$srcdir/marker\"",
        package: "install -Dm644 \"$srcdir/marker\" \"$pkgdir/usr/share/archix-test-hostbuilt/marker\"",
        ..TestPkg::new("archix-test-hostbuilt", "1.0")
    });
    let body = format!(
        "aur = {{ url = {:?}, build_on_host = {{ \"archix-test-hostbuilt\" }} }}",
        aur.url()
    );
    let setup = Setup::new(&[], &body);
    setup.first_switch();
    let host_has_lemon = || {
        Command::new("pacman")
            .args(["-Q", "lemon"])
            .output()
            .unwrap()
            .status
            .success()
    };
    assert!(!host_has_lemon(), "the container starts without lemon");

    setup.set_body(&["archix-test-hostbuilt"], &body);
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(out.contains("on the host"), "{out}");
    let marker = read("/usr/share/archix-test-hostbuilt/marker");
    assert!(
        marker.ends_with("bin/lemon"),
        "the build lacked its build dependency: {marker:?}"
    );
    assert_eq!(installed("lemon"), None, "build dependencies don't stay on the machine");
    assert!(!host_has_lemon(), "nor where pacman installed them for the build");
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
fn the_lock_and_status_say_which_aur_packages_float() {
    let _system = lock_system();
    let mut aur = LocalAur::new();
    aur.publish(TestPkg::new("archix-test-thing-git", "1.0.r3.gabc1234"));
    aur.publish(TestPkg::new("archix-test-steady", "1.0"));
    let body = format!("aur = {{ url = {:?} }}", aur.url());
    let setup = Setup::new(&[], &body);
    let init = setup.read("init.lua").unwrap().replacen(
        "packages = { ",
        "packages = { \"archix-test-thing-git\", \"archix-test-steady\", ",
        1,
    );
    setup.write("init.lua", &init);
    let locked = setup.ok(&["lock", "--noconfirm"]);
    assert!(
        locked.contains(
            "+ archix-test-thing-git 1.0.r3.gabc1234-1 (builds from a VCS source; its first build records the commit)"
        ),
        "{locked}"
    );
    assert!(locked.contains("+ archix-test-steady 1.0-1\n"), "{locked}");
    let status = setup.ok(&["status"]);
    assert!(
        status.contains("1 package builds from a VCS source archix.lock has no commit for yet, so its next build takes upstream's newest code, whose git commit is then recorded: archix-test-thing-git"),
        "{status}"
    );
}

#[test]
#[ignore]
fn a_new_aur_maintainer_is_named_when_the_lock_moves() {
    let _system = lock_system();
    let mut aur = LocalAur::new();
    let release = |version, maintainer| TestPkg {
        maintainer,
        ..TestPkg::new("archix-test-handover", version)
    };
    aur.publish(release("1.0", Some("alice")));
    let body = format!("aur = {{ url = {:?} }}", aur.url());
    let setup = Setup::new(&["archix-test-handover"], &body);
    aur.publish(release("1.1", Some("mallory")));
    let out = setup.ok(&["update", "--to", IMAGE_SNAPSHOT, "--noconfirm"]);
    assert!(
        out.contains(
            "archix-test-handover has a new AUR maintainer, alice -> mallory: read its recipe before you switch"
        ),
        "{out}"
    );
    aur.publish(release("1.2", None));
    let out = setup.ok(&["update", "--to", IMAGE_SNAPSHOT, "--noconfirm"]);
    assert!(
        out.contains("archix-test-handover has a new AUR maintainer, mallory -> nobody (orphaned): read its recipe before you switch"),
        "{out}"
    );
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
fn a_cache_build_that_isnt_the_locked_one_is_not_used() {
    let _system = lock_system();
    let name = "archix-test-swapped";
    let mut aur = LocalAur::new();
    aur.publish(TestPkg::new(name, "1.0"));
    let cache = TempDir::new().unwrap();
    let body = format!(
        "aur = {{ url = {:?} }}, package_caches = {{ {:?} }}",
        aur.url(),
        format!("file://{}", cache.path().display())
    );
    let setup = Setup::new(&[name], &body);
    setup.ok(&["switch", "--noconfirm"]);
    let built = fs::read_dir("/var/cache/archix/pkg")
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.to_string_lossy().contains(name) && path.to_string_lossy().ends_with(".pkg.tar.zst"))
        .unwrap();
    let locked = setup.lock().packages[name].sha256.clone();
    assert_eq!(
        locked,
        archix::fetch::sha256(&built).unwrap(),
        "the lock records the build it was made with"
    );
    setup.ok(&["cache", "push", cache.path().to_str().unwrap()]);
    let (init, lock) = (setup.read("init.lua").unwrap(), setup.read("archix.lock").unwrap());

    // Whoever can write to the cache swaps in another build of the same package, and a record that names it.
    let record = fs::read_dir(cache.path().join("aur-builds").join(name))
        .unwrap()
        .flatten()
        .next()
        .unwrap()
        .path();
    let text = fs::read_to_string(&record).unwrap();
    let file = cache.path().join(built.file_name().unwrap());
    let work = TempDir::new().unwrap();
    let unpack = Command::new("bsdtar")
        .arg("-xf")
        .arg(&file)
        .arg("-C")
        .arg(work.path())
        .status()
        .unwrap();
    assert!(unpack.success());
    let pkginfo = work.path().join(".PKGINFO");
    let info = fs::read_to_string(&pkginfo)
        .unwrap()
        .replace("pkgdesc = archix test package", "pkgdesc = swapped");
    fs::write(&pkginfo, info).unwrap();
    let pack = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "cd {} && bsdtar -cf - .PKGINFO .MTREE usr | zstd -q -f -o {}",
            work.path().display(),
            file.display()
        ))
        .status()
        .unwrap();
    assert!(pack.success());
    let swapped = archix::fetch::sha256(&file).unwrap();
    fs::write(&record, text.replace(&locked, &swapped)).unwrap();

    // Another machine with the committed lock and nothing built locally.
    setup.set_body(&[], &body);
    setup.ok(&["switch", "--noconfirm"]);
    let _ = fs::remove_dir_all("/var/cache/archix/aur-builds");
    for dir in ["/var/cache/archix/pkg", "/var/cache/pacman/pkg"] {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            if entry.file_name().to_string_lossy().contains(name) {
                fs::remove_file(entry.path()).unwrap();
            }
        }
    }
    setup.write("init.lua", &init);
    setup.write("archix.lock", &lock);
    let out = setup.ok(&["switch", "--noconfirm"]);
    let description = Command::new("pacman").args(["-Qi", name]).output().unwrap();
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert!(
        !out.contains("build from the package cache"),
        "the swapped build was used:\n{out}"
    );
    assert!(out.contains("doesn't match archix.lock"), "{out}");
    let description = String::from_utf8_lossy(&description.stdout).into_owned();
    assert!(
        !description
            .lines()
            .any(|line| line.starts_with("Description") && line.ends_with(": swapped")),
        "the package installed is the one built here:\n{description}"
    );
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
fn a_dependency_pinned_to_an_older_version_relocks_from_its_locked_commit_not_the_aur_newest() {
    let _system = lock_system();
    let mut aur = LocalAur::new();
    aur.publish(TestPkg::new("archix-test-pin-utils", "1.0"));
    aur.publish(TestPkg {
        depends: &["archix-test-pin-utils=1.0"],
        ..TestPkg::new("archix-test-pin-dkms", "1.0")
    });
    aur.publish(TestPkg::new("archix-test-pin-lib", "1.0"));
    aur.publish(TestPkg {
        depends: &["archix-test-pin-lib=1.0"],
        ..TestPkg::new("archix-test-pin-app", "1.0")
    });
    let setup = Setup::new(
        &["archix-test-pin-dkms", "archix-test-pin-utils", "archix-test-pin-app"],
        &format!("aur = {{ url = {:?} }}", aur.url()),
    );
    aur.publish(TestPkg::new("archix-test-pin-utils", "2.0"));
    aur.publish(TestPkg::new("archix-test-pin-lib", "2.0"));
    let _ = fs::remove_file(archix::aur::index::dump_path(
        std::path::Path::new("/var/cache/archix"),
        &aur.url(),
    ));
    let init = setup.read("init.lua").unwrap();
    setup.write(
        "init.lua",
        &init.replacen("packages = { ", "packages = { \"tree\", ", 1),
    );
    let output = setup.run(&["lock", "--noconfirm"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    setup.write("init.lua", &init);
    assert!(
        output.status.success(),
        "a declared dependency and an undeclared one, both locked at 1.0 while the AUR moved on to 2.0:\n{text}"
    );
    let lock = setup.lock();
    for name in [
        "archix-test-pin-dkms",
        "archix-test-pin-utils",
        "archix-test-pin-app",
        "archix-test-pin-lib",
    ] {
        assert_eq!(lock.packages[name].version, "1.0-1", "{name}");
    }
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

#[test]
#[ignore]
fn a_refused_switch_keeps_the_lock_it_resolved_and_the_aur_trust_given() {
    let _system = lock_system();
    let mut aur = LocalAur::new();
    let name = "archix-test-trusted";
    aur.publish(TestPkg::new(name, "1.0"));
    let body = format!("aur = {{ url = {:?} }}", aur.url());
    let setup = Setup::new(&[], &body);
    let before = setup.read("archix.lock").unwrap();
    let pipe = system_path("/etc/archix-test-pipe");
    assert!(Command::new("mkfifo").arg(&pipe).status().unwrap().success());
    let init = setup.read("init.lua").unwrap().replacen(
        "packages = { ",
        &format!(r#"files = {{ ["/etc/archix-test-pipe"] = {{ text = "x" }} }}, packages = {{ "{name}", "#),
        1,
    );
    setup.write("init.lua", &init);
    let text = |output: &std::process::Output| {
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    };

    let trusted = setup.answering_raw("y\n", &["switch"]);
    let out = text(&trusted);
    assert!(!trusted.status.success(), "the pipe refuses the switch:\n{out}");
    assert!(
        out.contains(&format!("trust {name} at")) && out.contains("is not a file or symlink"),
        "{out}"
    );
    assert_ne!(setup.read("archix.lock").unwrap(), before, "{out}");
    assert_eq!(
        setup.lock().packages[name].version,
        "1.0-1",
        "the resolved, trusted lock stays"
    );
    assert_eq!(installed(name), None, "{out}");

    let again = setup.answering_raw("", &["switch"]);
    let out = text(&again);
    assert!(!again.status.success(), "{out}");
    assert!(
        !out.contains("resolving") && !out.contains(&format!("trust {name} at")),
        "a rerun neither resolves again nor asks about the recipe again:\n{out}"
    );
    assert!(out.contains("is not a file or symlink"), "{out}");
    fs::remove_file(&pipe).unwrap();
}

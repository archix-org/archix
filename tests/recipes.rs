//! Builds the config's own recipes and patched Arch packages; run only inside the throwaway Arch container via `tests/arch/run.sh`.

mod common;

use std::fs;
use std::process::Command;

use archix::lock::LOCAL_REPO;
use common::*;

fn pkgbuild(version: &str) -> String {
    format!(
        "pkgname=archix-test-local\npkgver={version}\npkgrel=1\narch=(any)\nlicense=(MIT)\n\
         package() {{\n  mkdir -p \"$pkgdir/usr/share/archix-test-local\"\n  echo {version} > \"$pkgdir/usr/share/archix-test-local/version\"\n}}\n"
    )
}

#[test]
#[ignore]
fn local_recipes_are_locked_by_content_and_built_in_the_chroot() {
    let _system = lock_system();
    let setup = Setup::with_files(
        &["archix-test-local"],
        "",
        &[("pkgbuilds/archix-test-local/PKGBUILD", &pkgbuild("1.0"))],
    );
    let lock = setup.lock();
    let locked = &lock.packages["archix-test-local"];
    assert_eq!(locked.repo, LOCAL_REPO);
    assert_eq!(locked.base.as_deref(), Some("local:archix-test-local"));
    assert!(lock.recipes.contains_key("local:archix-test-local"));

    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(
        !out.contains("new AUR package"),
        "local recipes aren't reviewed:\n{out}"
    );
    assert_eq!(installed("archix-test-local").as_deref(), Some("1.0-1"));
    assert!(setup.ok(&["switch", "--noconfirm"]).contains("nothing to do"));

    let recipe = setup.dir.path().join("pkgbuilds/archix-test-local/PKGBUILD");
    fs::write(&recipe, pkgbuild("2.0")).unwrap();
    assert!(
        setup.ok(&["status"]).contains("the config changed since the lock"),
        "editing a recipe makes the lock stale"
    );
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(installed("archix-test-local").as_deref(), Some("2.0-1"));
    assert_eq!(
        fs::read_to_string("/usr/share/archix-test-local/version").unwrap(),
        "2.0\n"
    );

    fs::write(&recipe, pkgbuild("2.0").replace("echo 2.0", "echo rebuilt")).unwrap();
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(out.contains("reinstall  archix-test-local 2.0-1"), "{out}");
    assert_eq!(
        fs::read_to_string("/usr/share/archix-test-local/version").unwrap(),
        "rebuilt\n",
        "a recipe change without a version bump still reaches the system"
    );

    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert!(installed("archix-test-local").is_none());
}

const NEW_FILE_PATCH: &str = "--- /dev/null\n+++ b/ARCHIX-PATCHED\n@@ -0,0 +1 @@\n+patched by archix\n";

#[test]
#[ignore]
fn patched_packages_rebuild_archs_recipe_with_the_patches() {
    let _system = lock_system();
    let setup = Setup::with_files(
        &["tree"],
        r#"patches = { tree = { "patches/tree.patch" } }"#,
        &[("patches/tree.patch", NEW_FILE_PATCH)],
    );
    let locked = setup.lock().packages["tree"].clone();
    assert_eq!(locked.repo, LOCAL_REPO);
    assert_eq!(locked.base.as_deref(), Some("patch:tree"));
    assert!(locked.version.ends_with(".1"), "{}", locked.version);

    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(installed("tree").as_deref(), Some(locked.version.as_str()));
    assert!(Command::new("tree").arg("--version").status().unwrap().success());

    fs::write(
        setup.dir.path().join("patches/tree.patch"),
        "--- /dev/null\n+++ b/ARCHIX-PATCHED-AGAIN\n@@ -0,0 +1 @@\n+again\n",
    )
    .unwrap();
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(
        out.contains("reinstall  tree"),
        "changing a patch rebuilds at the same version:\n{out}"
    );
    assert_ne!(setup.lock().packages["tree"].commit, locked.commit);

    fs::write(
        setup.dir.path().join("patches/tree.patch"),
        "--- a/no-such-file\n+++ b/no-such-file\n@@ -1 +1 @@\n-x\n+y\n",
    )
    .unwrap();
    let err = setup.fails(&["switch", "--noconfirm"]);
    assert!(
        err.contains("tree"),
        "a patch that applies nowhere fails the build:\n{err}"
    );
    assert_eq!(
        installed("tree").as_deref(),
        Some(locked.version.as_str()),
        "the failed build changed nothing"
    );

    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
}

/// A stand-in kernel package: its modules directory for `release`, with a pkgbase file like pacman's kernels.
fn kernel_recipe(release: &str, version: &str) -> String {
    format!(
        "pkgname=archix-test-kernel\npkgver={version}\npkgrel=1\narch=(any)\nlicense=(MIT)\n\
         package() {{\n  mkdir -p \"$pkgdir/usr/lib/modules/{release}\"\n  \
         echo archix-test-kernel > \"$pkgdir/usr/lib/modules/{release}/pkgbase\"\n  \
         echo {version} > \"$pkgdir/usr/lib/modules/{release}/modules.order\"\n}}\n"
    )
}

#[test]
#[ignore]
fn the_running_kernels_modules_survive_a_switch_that_replaces_it() {
    let _system = lock_system();
    let running = fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap()
        .trim()
        .to_string();
    let modules = std::path::Path::new("/usr/lib/modules").join(&running);
    assert!(!modules.exists(), "the container has no modules for the host's kernel");
    let setup = Setup::with_files(
        &["archix-test-kernel"],
        "",
        &[("pkgbuilds/archix-test-kernel/PKGBUILD", &kernel_recipe(&running, "1"))],
    );
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(!out.contains("reboot to finish"), "{out}");
    assert!(modules.join("pkgbase").is_file());

    fs::write(
        setup.dir.path().join("pkgbuilds/archix-test-kernel/PKGBUILD"),
        kernel_recipe(&format!("{running}-next"), "2"),
    )
    .unwrap();
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(out.contains("reboot to finish"), "{out}");
    assert_eq!(
        fs::read_to_string(modules.join("modules.order")).unwrap(),
        "1\n",
        "the running kernel's modules are still there after pacman replaced its package"
    );
    assert!(
        std::path::Path::new("/usr/lib/modules")
            .join(format!("{running}-next"))
            .is_dir()
    );
    assert!(
        fs::read_to_string("/var/lib/archix/kept-modules")
            .unwrap()
            .contains(&running),
        "they're recorded for cleanup after the reboot"
    );

    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert!(installed("archix-test-kernel").is_none());
    fs::remove_dir_all(&modules).unwrap();
    let _ = fs::remove_file("/var/lib/archix/kept-modules");
    let _ = fs::remove_dir_all(format!("/var/lib/archix/kernels/{running}"));
}

fn recipe(name: &str, version: &str, depends: &str, package: &str) -> String {
    format!(
        "pkgname={name}\npkgver={version}\npkgrel=1\narch=(any)\nlicense=(MIT)\ndepends=({depends})\n\
         package() {{\n{package}\n}}\n"
    )
}

fn sleeper(version: &str) -> String {
    recipe(
        "archix-test-sleeper",
        version,
        "",
        "  install -Dm755 /usr/bin/sleep \"$pkgdir/usr/bin/archix-test-sleep\"",
    )
}

fn perl_lib(version: &str) -> String {
    recipe(
        "archix-test-perllib",
        version,
        "perl",
        &format!(
            "  mkdir -p \"$pkgdir/usr/share/perl5/vendor_perl\"\n  echo 'package ArchixTest; our $V = \"{version}\"; 1;' > \"$pkgdir/usr/share/perl5/vendor_perl/ArchixTest.pm\""
        ),
    )
}

const PERL_APP: &str = "  mkdir -p \"$pkgdir/usr/lib/archix-test-perlapp\"\n  echo 'use ArchixTest; sleep;' > \"$pkgdir/usr/lib/archix-test-perlapp/app.pl\"";

fn service(exec: &str) -> String {
    format!("[Service]\nExecStart={exec}\n[Install]\nWantedBy=multi-user.target")
}

fn main_pid(unit: &str) -> String {
    let output = Command::new("systemctl")
        .args(["show", "-P", "MainPID", unit])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
#[ignore]
fn services_using_replaced_files_or_changed_interpreted_code_restart() {
    let _system = lock_system();
    let body = format!(
        r#"units = {{ ["archix-test-native.service"] = {native:?}, ["archix-test-perl.service"] = {perl:?}, ["archix-test-kept.service"] = {kept:?} }},
        services = {{ ["archix-test-native.service"] = true, ["archix-test-perl.service"] = true, ["archix-test-kept.service"] = true }},
        restart_if_changed = {{ ["archix-test-kept.service"] = false }}"#,
        native = service("/usr/bin/archix-test-sleep infinity"),
        perl = service("/usr/bin/perl /usr/lib/archix-test-perlapp/app.pl"),
        kept = service("/usr/bin/archix-test-sleep infinity"),
    );
    let packages = ["archix-test-sleeper", "archix-test-perllib", "archix-test-perlapp"];
    let setup = Setup::with_files(
        &packages,
        &body,
        &[
            ("pkgbuilds/sleeper/PKGBUILD", &sleeper("1")),
            ("pkgbuilds/perllib/PKGBUILD", &perl_lib("1")),
            (
                "pkgbuilds/perlapp/PKGBUILD",
                &recipe("archix-test-perlapp", "1", "archix-test-perllib", PERL_APP),
            ),
        ],
    );
    setup.ok(&["switch", "--noconfirm"]);
    let before: Vec<String> = ["archix-test-native", "archix-test-perl", "archix-test-kept"]
        .iter()
        .map(|unit| main_pid(&format!("{unit}.service")))
        .collect();
    assert!(before.iter().all(|pid| pid != "0"), "{before:?}");

    fs::write(setup.dir.path().join("pkgbuilds/sleeper/PKGBUILD"), sleeper("2")).unwrap();
    fs::write(setup.dir.path().join("pkgbuilds/perllib/PKGBUILD"), perl_lib("2")).unwrap();
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert_ne!(
        main_pid("archix-test-native.service"),
        before[0],
        "a daemon whose binary was replaced restarts:\n{out}"
    );
    assert_ne!(
        main_pid("archix-test-perl.service"),
        before[1],
        "a Perl daemon restarts when a library it depends on changes:\n{out}"
    );
    assert_eq!(
        main_pid("archix-test-kept.service"),
        before[2],
        "restart_if_changed = false is respected"
    );
    assert!(
        out.contains("still using replaced files") && out.contains("archix-test-kept.service"),
        "{out}"
    );

    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
}

#[test]
#[ignore]
fn packages_shipping_the_same_file_are_refused_before_anything_changes() {
    let _system = lock_system();
    let ships = "  mkdir -p \"$pkgdir/usr/share/archix-test\"\n  echo x > \"$pkgdir/usr/share/archix-test/same\"";
    let setup = Setup::with_files(
        &["archix-test-one", "archix-test-two"],
        "",
        &[
            ("pkgbuilds/one/PKGBUILD", &recipe("archix-test-one", "1", "", ships)),
            ("pkgbuilds/two/PKGBUILD", &recipe("archix-test-two", "1", "", ships)),
        ],
    );
    let err = setup.fails(&["switch", "--noconfirm"]);
    assert!(
        err.contains("usr/share/archix-test/same is in both archix-test-one and archix-test-two")
            && err.contains("nothing was changed"),
        "{err}"
    );
    assert!(installed("archix-test-one").is_none() && installed("archix-test-two").is_none());
    assert!(!std::path::Path::new("/var/lib/archix/journal.json").exists());

    let conflicting =
        recipe("archix-test-two", "1", "", ships).replace("depends=()", "depends=()\nconflicts=(archix-test-one)");
    fs::write(setup.dir.path().join("pkgbuilds/two/PKGBUILD"), &conflicting).unwrap();
    let init = setup.dir.path().join("init.lua");
    let original = fs::read_to_string(&init).unwrap();
    let without = |drop: &str| {
        original
            .replace(&format!(", \"{drop}\""), "")
            .replace("return {", "return { undeclared_packages = \"warn\",")
    };
    fs::write(&init, without("archix-test-two")).unwrap();
    setup.ok(&["switch", "--noconfirm"]);
    fs::write(&init, without("archix-test-one")).unwrap();
    setup.ok(&["switch", "--noconfirm"]);
    assert!(
        installed("archix-test-two").is_some() && installed("archix-test-one").is_none(),
        "a package that conflicts with a kept one replaces it, as pacman does"
    );
    fs::write(&init, without("archix-test-two")).unwrap();
    setup.ok(&["switch", "--noconfirm"]);
    assert!(
        installed("archix-test-one").is_some() && installed("archix-test-two").is_none(),
        "a kept package that conflicts with an incoming one is replaced too"
    );
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
}

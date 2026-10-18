//! Needs pacman and network access; run inside Arch with `tests/arch/run.sh`.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use archix::lock::{LOCK_FILE, Lockfile};
use tempfile::TempDir;

fn config(packages: &[&str]) -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("init.lua"), host_file(packages)).unwrap();
    dir
}

fn host_file(packages: &[&str]) -> String {
    let quoted: Vec<String> = packages.iter().map(|p| format!("{p:?}")).collect();
    format!("return {{ packages = {{ {} }} }}", quoted.join(", "))
}

fn archix(dir: &Path, args: &[&str]) -> Output {
    let cache = dir.join(".cache");
    Command::new(env!("CARGO_BIN_EXE_archix"))
        .args(["--config", dir.to_str().unwrap(), "--host", "box"])
        .args(args)
        .env("XDG_CACHE_HOME", cache)
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
#[ignore]
fn lock_records_the_full_closure_with_declared_providers() {
    let dir = config(&["base", "pipewire-jack", "obs-studio"]);
    stdout(&archix(dir.path(), &["lock"]));
    let lock = Lockfile::load(&dir.path().join(LOCK_FILE)).unwrap();
    let host = &lock.hosts["box"];
    assert!(host.packages.len() > 100, "only {} packages", host.packages.len());
    assert!(host.packages["glibc"].sha256.len() == 64);
    assert!(host.packages["pipewire-jack"].explicit);
    assert!(!host.packages["glibc"].explicit);
    assert!(
        !host.packages.contains_key("jack2"),
        "the declared provider should win over jack2"
    );

    let again = stdout(&archix(dir.path(), &["lock"]));
    assert!(again.contains("is up to date"), "{again}");
}

#[test]
#[ignore]
fn a_dependency_on_a_real_package_gets_that_package_not_a_stand_in() {
    let dir = config(&["jre-openjdk-headless", "python-certifi", "rustup", "maturin"]);
    stdout(&archix(dir.path(), &["lock"]));
    let host = &Lockfile::load(&dir.path().join(LOCK_FILE)).unwrap().hosts["box"];
    for name in ["ca-certificates", "ca-certificates-mozilla"] {
        assert!(
            host.packages.get(name).is_some_and(|pkg| !pkg.explicit),
            "python-certifi needs {name}, not just ca-certificates-utils, which provides the name but no certificates"
        );
    }
    assert!(
        !host.packages.contains_key("rust"),
        "the declared rustup still provides maturin's rust"
    );
}

#[test]
#[ignore]
fn lock_keeps_the_date_until_update() {
    let dir = config(&["base"]);
    stdout(&archix(dir.path(), &["lock"]));
    let path = dir.path().join(LOCK_FILE);
    let mut lock = Lockfile::load(&path).unwrap();
    lock.hosts.get_mut("box").unwrap().date = "2026-06-01".parse().unwrap();
    lock.save(&path).unwrap();
    fs::write(dir.path().join("init.lua"), host_file(&["base", "htop"])).unwrap();
    stdout(&archix(dir.path(), &["lock"]));
    let relocked = Lockfile::load(&path).unwrap();
    assert_eq!(relocked.hosts["box"].date.to_string(), "2026-06-01");
    assert!(relocked.hosts["box"].packages.contains_key("htop"));

    let updated = stdout(&archix(dir.path(), &["update"]));
    assert!(updated.contains("snapshot 2026-06-01 -> "), "{updated}");
}

#[test]
#[ignore]
fn a_package_newer_than_the_locked_snapshot_is_refused_and_the_date_stays() {
    let dir = config(&["base"]);
    stdout(&archix(dir.path(), &["update", "--to", "2026-08-19", "--noconfirm"]));
    let path = dir.path().join(LOCK_FILE);
    let before = fs::read_to_string(&path).unwrap();
    // cadical reached [extra] after 2026-08-19 and is also on the AUR under the same name.
    fs::write(dir.path().join("init.lua"), host_file(&["base", "cadical"])).unwrap();
    let output = archix(dir.path(), &["lock"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!(
        "archix lock, with cadical added to a 2026-08-19 lock:\n{}{stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !output.status.success(),
        "cadical isn't in the 2026-08-19 snapshot, so locking it there must fail"
    );
    assert!(
        stderr.contains("`cadical` isn't in the 2026-08-19 snapshot") && stderr.contains("archix update"),
        "{stderr}"
    );
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        before,
        "a refused lock leaves archix.lock as it was"
    );
    assert_eq!(
        Lockfile::load(&path).unwrap().hosts["box"].date.to_string(),
        "2026-08-19"
    );
}

#[test]
#[ignore]
fn with_the_archive_unreachable_a_name_the_snapshot_lacks_is_refused_not_taken_from_the_aur() {
    let dir = config(&["base"]);
    stdout(&archix(dir.path(), &["update", "--to", "2026-08-19", "--noconfirm"]));
    let path = dir.path().join(LOCK_FILE);
    let before = fs::read_to_string(&path).unwrap();
    // Nothing listens on port 9, so the newest snapshot can't be checked; the 2026-08-19 databases are cached already.
    fs::write(
        dir.path().join("init.lua"),
        r#"return { packages = { "base", "cadical" }, archive_url = "http://127.0.0.1:9" }"#,
    )
    .unwrap();
    let output = archix(dir.path(), &["lock"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!("archix lock, cadical added, archive unreachable:\n{text}");
    assert!(
        !output.status.success(),
        "with the archive unreachable, cadical can't be verified, so locking must fail:\n{text}"
    );
    assert!(
        text.contains("couldn't verify against the archive") && text.contains("`cadical`"),
        "{text}"
    );
    assert!(
        !text.contains("new AUR package"),
        "the AUR's cadical mustn't even be offered:\n{text}"
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), before);
}

#[test]
#[ignore]
fn unknown_packages_fail_to_lock() {
    let dir = config(&["base", "definitely-not-a-package"]);
    let output = archix(dir.path(), &["lock"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("`definitely-not-a-package` is in none of the configured repos, pkgbuilds/ or the AUR"),
        "{stderr}"
    );
    assert!(!dir.path().join(LOCK_FILE).exists());
}

#[test]
#[ignore]
fn diff_lists_installs_and_removals() {
    let dir = config(&["base", "htop"]);
    stdout(&archix(dir.path(), &["lock"]));
    let diff = stdout(&archix(dir.path(), &["diff"]));
    assert!(diff.contains("install    htop "), "{diff}");
    assert!(
        diff.contains("remove     rust "),
        "the test image's rust is not declared:\n{diff}"
    );
}

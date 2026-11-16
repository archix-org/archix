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

fn archix_code(config: &str, args: &[&str]) -> (Option<i32>, String) {
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
    (output.status.code(), text)
}

/// A config `archix init` wrote for this machine, in a directory other accounts can read.
fn init_config() -> (tempfile::TempDir, String) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
    let config = dir.path().join("cfg").to_str().unwrap().to_string();
    let (ok, out) = archix(&config, &["init"]);
    assert!(ok, "init failed:\n{out}");
    (dir, config)
}

/// Adds a module with `fields` (Lua table fields) to an init-written config.
fn manage(config: &str, fields: &str) {
    fs::write(
        format!("{config}/init.lua"),
        "return { imports = { \"hosts.\" .. archix.hostname, \"managed\" } }\n",
    )
    .unwrap();
    fs::write(format!("{config}/lua/managed.lua"), format!("return {{ {fields} }}\n")).unwrap();
}

/// Adds a module declaring `files` (a Lua table body) to an init-written config.
fn manage_files(config: &str, files: &str) {
    manage(config, &format!("files = {{ {files} }}"));
}

/// A file as a switch would have left it: `content` with `mode`, owned by root.
fn place(path: &str, content: &str, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn plan_json(config: &str) -> (Option<i32>, serde_json::Value, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_archix"))
        .args(["--config", config, "--host", "initbox", "diff", "--json"])
        .output()
        .unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let plan = serde_json::from_slice(&output.stdout).unwrap_or(serde_json::Value::Null);
    (output.status.code(), plan, said)
}

/// Removes a test user and their home when dropped, also when the test panics.
struct TestUser(&'static str);

impl TestUser {
    fn new(name: &'static str) -> TestUser {
        let _ = Command::new("userdel").args(["-r", name]).output();
        assert!(Command::new("useradd").args(["-m", name]).status().unwrap().success());
        TestUser(name)
    }
}

impl Drop for TestUser {
    fn drop(&mut self) {
        let _ = Command::new("userdel").args(["-r", self.0]).status();
    }
}

/// Removes an explicitly installed test package when dropped.
struct Installed(&'static str);

impl Drop for Installed {
    fn drop(&mut self) {
        let _ = Command::new("pacman").args(["-Rns", "--noconfirm", self.0]).status();
    }
}

#[test]
#[ignore]
fn diff_right_after_init_has_nothing_to_do() {
    let _system = lock_system();
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("cfg");
    let config = config.to_str().unwrap();
    let (ok, out) = archix(config, &["init"]);
    assert!(ok, "init failed:\n{out}");
    let (code, text) = archix_code(config, &["diff"]);
    assert_eq!(code, Some(0), "SCOPE 8: diff after init exits 0, got:\n{text}");
    let (code, json) = archix_code(config, &["diff", "--json"]);
    assert_eq!(code, Some(0), "{json}");
    let plan: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(plan["schema_version"].is_number(), "{json}");
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
    assert!(
        !host.contains("files = {") && !host.contains("home = {"),
        "init scrapes no files or dotfiles:\n{host}"
    );
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

#[test]
#[ignore]
fn a_package_installed_after_init_is_drift_installed() {
    let _system = lock_system();
    let (_dir, config) = init_config();
    let (code, text) = archix_code(&config, &["diff"]);
    assert_eq!(code, Some(0), "the control: diff after init exits 0, got:\n{text}");
    let _tree = Installed("tree");
    pacman(&["-S", "tree"]);
    let (code, text) = archix_code(&config, &["diff"]);
    assert_eq!(code, Some(1), "{text}");
    let line = text
        .lines()
        .find(|l| l.contains(" tree "))
        .unwrap_or_else(|| panic!("no line names tree:\n{text}"));
    assert!(line.contains("remove") && line.contains("drift-installed"), "{line}");
    let (_, plan, said) = plan_json(&config);
    let removed: Vec<&str> = plan["packages"]
        .as_array()
        .unwrap_or_else(|| panic!("{said}"))
        .iter()
        .filter(|p| p["action"] == "remove")
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert_eq!(removed, ["tree"], "only the hand install is drift: {said}");
}

#[test]
#[ignore]
fn a_hand_edit_of_a_managed_file_is_a_modify_line() {
    let _system = lock_system();
    let path = "/etc/archix-identity.conf";
    let _restore = RestoreFile::new(Path::new(path));
    let (_dir, config) = init_config();
    manage_files(&config, &format!("[{path:?}] = {{ text = \"managed\\n\" }}"));
    place(path, "managed\n", 0o644);
    let (code, text) = archix_code(&config, &["diff"]);
    assert_eq!(code, Some(0), "the control: the managed file as declared, got:\n{text}");
    place(path, "edited by hand\n", 0o644);
    let (code, text) = archix_code(&config, &["diff"]);
    assert_eq!(code, Some(1), "{text}");
    let line = text
        .lines()
        .find(|l| l.contains(path))
        .unwrap_or_else(|| panic!("no line names {path}:\n{text}"));
    assert!(line.contains("modify"), "{line}");
    assert!(
        text.contains("-edited by hand") && text.contains("+managed"),
        "the line has its unified diff:\n{text}"
    );
}

#[test]
#[ignore]
fn a_normal_users_diff_is_incomplete_and_lists_only_root_only_items() {
    let _system = lock_system();
    let path = "/etc/archix-identity-private.conf";
    let _restore = RestoreFile::new(Path::new(path));
    let (_dir, config) = init_config();
    manage_files(
        &config,
        &format!("[{path:?}] = {{ text = \"private\\n\", mode = \"0600\" }}"),
    );
    place(path, "private\n", 0o600);
    let (code, text) = archix_code(&config, &["diff"]);
    assert_eq!(
        code,
        Some(0),
        "the control: root reads the 0600 file and finds it as declared, got:\n{text}"
    );
    let user = TestUser::new("archix-differ");
    let home = format!("/home/{}", user.0);
    let output = Command::new("runuser")
        .args([
            "-u",
            user.0,
            "--",
            "env",
            &format!("HOME={home}"),
            &format!("XDG_CACHE_HOME={home}/.cache"),
        ])
        .arg(env!("CARGO_BIN_EXE_archix"))
        .args(["--config", &config, "--host", "initbox", "diff", "--json"])
        .output()
        .unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(4), "{said}");
    let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| panic!("{e}: {said}"));
    let unchecked: Vec<&str> = plan["unchecked"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|u| u.as_str())
        .collect();
    assert_eq!(unchecked, [format!("file {path}: not readable without root")], "{said}");
    for group in ["packages", "files", "services", "users"] {
        assert_eq!(
            plan[group],
            serde_json::json!([]),
            "a user's diff sees the same {group} as root's: {said}"
        );
    }
}

fn hex_digest(program: &str, bytes: &[u8]) -> String {
    use std::io::Write;
    let mut child = Command::new(program)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(bytes).unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap()
        .to_string()
}

/// What a leak of `value` could look like: itself, and its sha256, sha512 and md5 hex, whole and as their 12-character prefixes (which every longer prefix contains), in either case.
fn derived_markers(name: &str, value: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut out = vec![(format!("{name} plaintext"), value.to_vec())];
    for program in ["sha256sum", "sha512sum", "md5sum"] {
        let hex = hex_digest(program, value);
        assert!(hex.len() >= 32, "{program}: {hex}");
        for form in [hex.clone(), hex.to_uppercase()] {
            out.push((format!("{name} {program} prefix"), form.as_bytes()[..12].to_vec()));
            out.push((format!("{name} {program}"), form.into_bytes()));
        }
    }
    out
}

/// Removes this machine's secrets identity when dropped, if the test made it.
struct MadeIdentity(Option<std::path::PathBuf>);

impl Drop for MadeIdentity {
    fn drop(&mut self) {
        if let Some(dir) = &self.0 {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

#[test]
#[ignore]
fn diff_never_shows_a_secret_or_an_unkeyed_hash_of_it() {
    let _system = lock_system();
    let path = "/etc/archix-leak.conf";
    let _restore = RestoreFile::new(Path::new(path));
    let state = archix::paths::Paths::system().state;
    let existed = archix::secrets::crypto::dir(&state).exists();
    let (machine_key, made) = archix::secrets::crypto::init(&state, false).unwrap();
    let _identity = MadeIdentity((made && !existed).then(|| archix::secrets::crypto::dir(&state)));
    let admin = age::x25519::Identity::generate().to_public().to_string();
    let (dir, config) = init_config();
    manage(
        &config,
        &format!(
            r#"secrets = {{ machine_key = "{machine_key}", admins = {{ "{admin}" }}, items = {{ leak = {{ file = "secrets/leak.age" }} }} }},
  files = {{ [{path:?}] = {{ text = "KEY=" .. archix.secret("leak") .. "\n", mode = "0600" }} }}"#
        ),
    );
    let secret = marker(40);
    let mut set = Command::new(env!("CARGO_BIN_EXE_archix"))
        .args(["--config", &config, "--host", "initbox", "secrets", "set", "leak"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut set.stdin.take().unwrap(), secret.as_bytes()).unwrap();
    let set = set.wait_with_output().unwrap();
    assert!(
        set.status.success(),
        "secrets set: {}",
        String::from_utf8_lossy(&set.stderr)
    );

    let saved = dir.path().join("plan.json");
    let mut outputs = Vec::new();
    for args in [
        vec!["diff"],
        vec!["diff", "--json"],
        vec!["diff", "--save", saved.to_str().unwrap()],
    ] {
        let (code, text) = archix_code(&config, &args);
        assert!(matches!(code, Some(1) | Some(4)), "archix {args:?}:\n{text}");
        outputs.push(text);
    }
    outputs.push(fs::read_to_string(&saved).unwrap());
    assert!(
        outputs[0].contains(path) && outputs[0].contains("secret"),
        "the file shows as a secret line:\n{}",
        outputs[0]
    );
    let mut markers = derived_markers("secret", secret.as_bytes());
    markers.extend(derived_markers("rendered file", format!("KEY={secret}\n").as_bytes()));
    let hits = sweep_outputs(&markers, &outputs);
    assert!(hits.is_empty(), "SCOPE 3.3: diff leaked {hits:#?}");
}

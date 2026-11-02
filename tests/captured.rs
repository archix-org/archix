//! What root commands do with a config another account owns: capture it as that account, evaluate it under a transient uid with limits, and write into its directory only as that account; run as root inside the Arch container via `tests/arch/run.sh`.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use archix::captured::{self, Applied, Owner, Work};
use archix::eval::isolated::{self, Limits};

const USER: &str = "archix-owner";

fn sh(command: &str) -> String {
    let output = Command::new("sh").arg("-c").arg(command).output().unwrap();
    assert!(
        output.status.success(),
        "{command}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn setup() -> Owner {
    unsafe { std::env::set_var("ARCHIX_TEST_SELF", env!("CARGO_BIN_EXE_archix")) };
    if Command::new("id").arg(USER).output().unwrap().status.success() {
        return owner();
    }
    sh(&format!("useradd -m -u 1500 {USER} 2> /dev/null || id {USER}"));
    owner()
}

fn owner() -> Owner {
    Owner::by_uid(sh(&format!("id -u {USER}")).trim().parse().unwrap()).unwrap()
}

/// A config directory `owner` owns, in a git repository of theirs, under their home so they can reach it.
fn config(owner: &Owner, name: &str, init: &str) -> PathBuf {
    let dir = owner.home.join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("init.lua"), init).unwrap();
    sh(&format!(
        "chown -R {USER}: {} && runuser -u {USER} -- git -C {} init -q",
        dir.display(),
        dir.display()
    ));
    dir
}

fn captures() -> PathBuf {
    let dir = PathBuf::from("/run/archix/test-captures");
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_hostile_git_config_in_the_owners_repo_never_runs_as_root() {
    let owner = setup();
    let dir = config(&owner, "hostile", "return { packages = { \"vim\" } }\n");
    let marks = PathBuf::from("/tmp/archix-hostile");
    let _ = fs::remove_dir_all(&marks);
    fs::create_dir_all(&marks).unwrap();
    fs::set_permissions(&marks, fs::Permissions::from_mode(0o1777)).unwrap();
    let mark = |what: &str| format!("sh -c 'id -u > {}/{what}'", marks.display());
    fs::write(
        dir.join(".git/config"),
        format!(
            "[core]\n\tfsmonitor = {}\n\thooksPath = {}\n[filter \"x\"]\n\tclean = {}\n\tsmudge = {}\n",
            mark("fsmonitor"),
            marks.display(),
            mark("clean"),
            mark("smudge")
        ),
    )
    .unwrap();
    fs::write(dir.join(".gitattributes"), "* filter=x\n").unwrap();
    sh(&format!("chown -R {USER}: {}", dir.display()));
    let work = Work::new(&captures()).unwrap();
    let captured = captured::capture(&dir, &work).unwrap();
    assert!(captured.dir.join("init.lua").is_file());
    assert_eq!(
        fs::read_dir(&marks).unwrap().count(),
        0,
        "the capture ran something from the owner's .git/config"
    );
    captured::record_for_owner(&dir, &captured, 7).unwrap();
    for entry in fs::read_dir(&marks).unwrap().flatten() {
        assert_eq!(
            fs::read_to_string(entry.path()).unwrap().trim(),
            owner.uid.to_string(),
            "{:?} ran as someone other than the config's owner",
            entry.file_name()
        );
    }
    let reference = sh(&format!(
        "runuser -u {USER} -- git -C {} rev-parse refs/archix/generations/7",
        dir.display()
    ));
    assert_eq!(reference.trim(), captured.tree);
}

#[test]
fn the_captured_copy_is_roots_so_changing_the_config_after_the_capture_changes_nothing() {
    let owner = setup();
    let dir = config(&owner, "toctou", "return { packages = { \"vim\" } }\n");
    let work = Work::new(&captures()).unwrap();
    let captured = captured::capture(&dir, &work).unwrap();
    sh(&format!(
        "runuser -u {USER} -- sh -c 'echo \"return {{ packages = {{ \\\"evil\\\" }} }}\" > {}/init.lua'",
        dir.display()
    ));
    let config = isolated::evaluate(&captured.dir, "host", &BTreeMap::new(), &Limits::default()).unwrap();
    assert!(!config.to_string().contains("evil"), "{config}");
    let meta = fs::metadata(captured.dir.join("init.lua")).unwrap();
    assert_eq!(meta.uid(), 0);
    let attempt = Command::new("runuser")
        .args(["-u", USER, "--", "sh", "-c"])
        .arg(format!("echo x >> {}/init.lua", captured.dir.display()))
        .output()
        .unwrap();
    assert!(!attempt.status.success(), "the owner could write root's copy");
    let applied = Applied {
        git_dir: work.dir.join("applied.git"),
    };
    let tree = applied.store_tree(&captured).unwrap();
    assert_eq!(tree, captured.tree);
    applied
        .record(1, &tree, b"lock", &isolated::plan_json(&config))
        .unwrap();
    let (stored, lock, plan) = applied.read(1, None).unwrap();
    assert_eq!((stored, lock), (captured.tree.clone(), b"lock".to_vec()));
    assert_eq!(isolated::read_plan(&plan).unwrap(), config);
    assert!(
        applied
            .read(1, Some("0000000000000000000000000000000000000000"))
            .is_err()
    );
}

#[test]
fn a_file_its_owner_cant_read_fails_the_capture_by_name() {
    let owner = setup();
    let dir = config(&owner, "unreadable", "return {}\n");
    fs::write(dir.join("secret.txt"), "x").unwrap();
    sh(&format!(
        "chown root: {0}/secret.txt && chmod 600 {0}/secret.txt",
        dir.display()
    ));
    let work = Work::new(&captures()).unwrap();
    let error = format!("{:#}", captured::capture(&dir, &work).unwrap_err());
    assert!(error.contains("secret.txt") && error.contains(USER), "{error}");
}

fn evaluate(owner: &Owner, name: &str, init: &str, limits: &Limits) -> Result<String, String> {
    let dir = config(owner, name, init);
    let work = Work::new(&captures()).unwrap();
    let captured = captured::capture(&dir, &work).map_err(|e| format!("{e:#}"))?;
    isolated::evaluate(&captured.dir, "host", &BTreeMap::new(), limits)
        .map(|config| config.to_string())
        .map_err(|e| format!("{e:#}"))
}

#[test]
fn the_config_cant_reach_outside_its_tree_or_run_anything() {
    let owner = setup();
    fs::write("/root/archix-only-root", "root's").unwrap();
    for (name, init, want) in [
        ("io", "local f = io.open('/etc/passwd') return {}", "io"),
        ("os", "os.execute('touch /tmp/archix-escaped') return {}", "os"),
        ("load", "load('return 1')() return {}", "load"),
        ("dofile", "dofile('/etc/passwd') return {}", "dofile"),
        (
            "require-up",
            "require('../../../../root/archix-only-root') return {}",
            "archix-only-root",
        ),
        (
            "read-file",
            "return { files = { ['/etc/x'] = { text = archix.read_file('/root/archix-only-root') } } }",
            "archix-only-root",
        ),
    ] {
        let result = evaluate(&owner, &format!("escape-{name}"), init, &Limits::default());
        let error = result.expect_err(&format!("{name}: the config's escape worked"));
        assert!(error.contains(want), "{name}: {error}");
    }
    assert!(!Path::new("/tmp/archix-escaped").exists());
}

#[test]
fn a_runaway_config_is_stopped_by_its_limits() {
    let owner = setup();
    let limits = Limits {
        cpu_seconds: 2,
        memory_bytes: 1 << 30,
        wall: Duration::from_secs(20),
    };
    let error = evaluate(&owner, "spin", "while true do end", &limits).unwrap_err();
    assert!(error.contains("CPU"), "{error}");
    let error = evaluate(&owner, "huge", "local s = string.rep('x', 2^40) return {}", &limits).unwrap_err();
    assert!(error.contains("too large"), "{error}");
    let error = evaluate(
        &owner,
        "hog",
        "local t = {} for i = 1, 1e12 do t[i] = ('x'):rep(64) .. i end return {}",
        &limits,
    )
    .unwrap_err();
    assert!(
        error.contains("not enough memory") || error.contains("MiB") || error.contains("CPU"),
        "{error}"
    );
    let sleeper = Limits {
        cpu_seconds: 60,
        memory_bytes: 1 << 30,
        wall: Duration::from_secs(2),
    };
    let error = evaluate(&owner, "wall", "while true do end", &sleeper).unwrap_err();
    assert!(error.contains("longer than 2 s"), "{error}");
}

#[test]
fn the_evaluator_runs_as_a_transient_uid_the_owner_cant_ptrace() {
    let owner = setup();
    let dir = config(&owner, "ptrace", "while true do end");
    let work = Work::new(&captures()).unwrap();
    let captured = captured::capture(&dir, &work).unwrap();
    let tree = captured.dir.clone();
    let mine = captured.dir.to_string_lossy().into_owned();
    let running = std::thread::spawn(move || {
        let limits = Limits {
            cpu_seconds: 30,
            memory_bytes: 1 << 30,
            wall: Duration::from_secs(8),
        };
        isolated::evaluate(&tree, "host", &BTreeMap::new(), &limits)
    });
    let mut found = None;
    for _ in 0..200 {
        found = fs::read_dir("/proc").unwrap().flatten().find_map(|entry| {
            let cmdline = fs::read(entry.path().join("cmdline")).ok()?;
            let uid = entry.metadata().ok()?.uid();
            let cmdline = String::from_utf8_lossy(&cmdline).into_owned();
            (cmdline.contains("evaluate-isolated") && cmdline.contains(&*mine) && uid >= 61184)
                .then(|| (entry.file_name().to_string_lossy().parse::<i32>().unwrap(), uid))
        });
        if found.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let (pid, uid) = found.expect("the evaluator never showed up in /proc");
    assert_ne!(uid, owner.uid);
    let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    assert!(
        status
            .lines()
            .any(|line| line.starts_with("NoNewPrivs:") && line.ends_with('1')),
        "{status}"
    );
    let attach = Command::new("runuser")
        .args(["-u", USER, "--", "perl", "-e"])
        .arg(format!(
            "my $r = syscall(101, 16, {pid}, 0, 0); exit($r == 0 ? 0 : $! + 0)"
        ))
        .status()
        .unwrap();
    assert_eq!(attach.code(), Some(libc::EPERM), "the owner could ptrace the evaluator");
    let error = format!("{:#}", running.join().unwrap().unwrap_err());
    assert!(error.contains("longer than"), "{error}");
}

#[test]
fn root_writes_into_the_owners_directory_only_as_the_owner_and_never_through_a_symlink() {
    let owner = setup();
    let dir = config(&owner, "drift", "return {}\n");
    fs::write("/root/archix-target", "untouched").unwrap();
    std::os::unix::fs::symlink("/root/archix-target", dir.join("installed.lua")).unwrap();
    sh(&format!("chown -h {USER}: {}/installed.lua", dir.display()));
    let error = format!(
        "{:#}",
        captured::write_as_owner(&dir.join("installed.lua"), b"planted").unwrap_err()
    );
    assert!(error.contains("isn't a regular file"), "{error}");
    assert_eq!(fs::read_to_string("/root/archix-target").unwrap(), "untouched");
    let linked = owner.home.join("drift-link");
    let _ = fs::remove_file(&linked);
    std::os::unix::fs::symlink("/etc", &linked).unwrap();
    assert!(captured::write_as_owner(&linked.join("archix-planted"), b"x").is_err());
    assert!(!Path::new("/etc/archix-planted").exists());
    fs::remove_file(dir.join("installed.lua")).unwrap();
    captured::write_as_owner(&dir.join("installed.lua"), b"return {}\n").unwrap();
    let meta = fs::metadata(dir.join("installed.lua")).unwrap();
    assert_eq!(
        (meta.uid(), fs::read_to_string(dir.join("installed.lua")).unwrap()),
        (owner.uid, "return {}\n".into())
    );
}

#[test]
fn every_newer_archix_reads_older_plans_and_refuses_newer_ones() {
    let old = br#"{"format":1,"config":{"packages":["vim"],"n":3,"f":1.5}}"#;
    let config = isolated::read_plan(old).unwrap();
    assert_eq!(config.to_string(), r#"{"f":1.5,"n":3,"packages":["vim"]}"#);
    let error = isolated::read_plan(br#"{"format":99,"config":{}}"#)
        .unwrap_err()
        .to_string();
    assert!(error.contains("newer"), "{error}");
}

#[test]
fn a_config_diff_never_prints_a_password_hash() {
    let text = "+ hashed_password = \"$6$salt$abcdef\", other = \"$y$j9T$x$y\"\n";
    assert_eq!(
        captured::redact_hashes(text),
        "+ hashed_password = \"<hash>\", other = \"<hash>\"\n"
    );
}

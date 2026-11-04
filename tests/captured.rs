//! What root commands do with a config another account owns: capture it as that account, evaluate it under a transient uid with limits, and write into its directory only as that account; run as root inside the Arch container via `tests/arch/run.sh`.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

mod common;

use archix::captured::{self, Applied, Owner, Work};
use archix::eval::isolated::{self, Limits};
use archix::lock::{HostLock, Lockfile};

const USER: &str = "archix-owner";

fn rendered_lock(date: &str) -> String {
    let mut lockfile = Lockfile::default();
    lockfile.hosts.insert(
        "box".to_string(),
        HostLock {
            date: date.parse().unwrap(),
            repos: Vec::new(),
            declared: Vec::new(),
            prefer: Vec::new(),
            pinned: Vec::new(),
            held: BTreeMap::new(),
            packages: BTreeMap::new(),
            flatpak: BTreeMap::new(),
            recipes: BTreeMap::new(),
        },
    );
    lockfile.render()
}

/// Runs archix on `dir` with ARCHIX_TEST_AFTER_CAPTURE set to `race`, and returns its status and everything it said.
fn archix(dir: &Path, race: Option<&str>, args: &[&str]) -> (bool, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_archix"));
    command
        .args(["--config", dir.to_str().unwrap(), "--host", "box"])
        .args(args);
    if let Some(race) = race {
        command.env("ARCHIX_TEST_AFTER_CAPTURE", race);
    }
    let out = command.output().unwrap();
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

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
    // A machine's setup copies /etc, which useradd's temporary files would race.
    let _system = common::lock_system();
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
    sh(&format!(
        "runuser -u {USER} -- sh -c 'cd {} && git status --short > /dev/null; git add -A; git reset -q'",
        dir.display()
    ));
    let fired: Vec<String> = fs::read_dir(&marks)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    println!(
        "EVIDENCE hostile .git/config: plain git as the owner ran {fired:?} from core.fsmonitor and filter.x.clean"
    );
    assert!(
        fired.contains(&"fsmonitor".to_string()) && fired.contains(&"clean".to_string()),
        "the positive control: plain git runs the payload: {fired:?}"
    );
    fs::remove_dir_all(&marks).unwrap();
    fs::create_dir_all(&marks).unwrap();
    fs::set_permissions(&marks, fs::Permissions::from_mode(0o1777)).unwrap();
    let work = Work::new(&captures()).unwrap();
    let captured = captured::capture(&dir, &work).unwrap();
    println!(
        "EVIDENCE hostile .git/config: archix's capture ran {} of them",
        fs::read_dir(&marks).unwrap().count()
    );
    assert!(captured.dir.join("init.lua").is_file());
    assert_eq!(
        fs::metadata(work.dir.join("git/HEAD")).unwrap().uid(),
        owner.uid,
        "git ran as someone other than the config's owner"
    );
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
    // Reachable by any uid, so only the capture can be what keeps the change out.
    fs::set_permissions(&owner.home, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
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
    let generation = applied
        .record(1, &tree, b"lock", &isolated::plan_json(&config))
        .unwrap();
    let stored = applied.verified(1, &generation.commit, Some(&tree)).unwrap();
    assert_eq!((stored.tree, stored.lock), (captured.tree.clone(), b"lock".to_vec()));
    assert_eq!(isolated::read_plan(&stored.plan).unwrap(), config);
    // Files older than the index aren't hashed again by git add, so an index another repository filled would name blobs this one lacks, as when setup makes @archix's repository mid-switch.
    sh(&format!(
        "find {} -type f -exec touch -d '2 days ago' {{}} +",
        captured.dir.display()
    ));
    applied.store_tree(&captured).unwrap();
    let other = Applied {
        git_dir: work.dir.join("other.git"),
    };
    assert_eq!(
        other.store_tree(&captured).unwrap(),
        captured.tree,
        "a second repository gets every blob"
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
    for (name, init, want) in [
        ("io", "local f = io.open('/etc/passwd') return {}", "'io'"),
        ("os", "os.execute('touch /tmp/archix-escaped') return {}", "'os'"),
        ("load", "load('return 1')() return {}", "'load'"),
        ("dofile", "dofile('/etc/passwd') return {}", "'dofile'"),
        ("loadfile", "loadfile('/etc/passwd') return {}", "'loadfile'"),
    ] {
        let result = evaluate(&owner, &format!("escape-{name}"), init, &Limits::default());
        let error = result.expect_err(&format!("{name}: the config's escape worked"));
        assert!(error.contains(want), "{name}: {error}");
    }
    assert!(!Path::new("/tmp/archix-escaped").exists());
    // Files any uid can read, so only the confinement can be what stops these, not the evaluator's uid.
    let outside = PathBuf::from("/tmp/archix-outside");
    let _ = fs::remove_dir_all(&outside);
    fs::create_dir_all(&outside).unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(outside.join("mod.lua"), "return { packages = { 'escaped' } }\n").unwrap();
    fs::set_permissions(outside.join("mod.lua"), fs::Permissions::from_mode(0o644)).unwrap();
    let dir = config(&owner, "escape-links", "return require('linked.mod')\n");
    fs::create_dir_all(dir.join("lua")).unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("lua/linked")).unwrap();
    sh(&format!("chown -R {USER}: {}", dir.display()));
    let work = Work::new(&captures()).unwrap();
    let captured = captured::capture(&dir, &work).unwrap();
    let result = isolated::evaluate(&captured.dir, "host", &BTreeMap::new(), &Limits::default());
    let error = format!("{:#}", result.expect_err("require followed a link out of the tree"));
    assert!(error.contains("leaves the config directory"), "require: {error}");

    let dir = config(
        &owner,
        "escape-read",
        "return { packages = { archix.read_file('linked/mod.lua') } }\n",
    );
    std::os::unix::fs::symlink(&outside, dir.join("linked")).unwrap();
    sh(&format!("chown -R {USER}: {}", dir.display()));
    let work = Work::new(&captures()).unwrap();
    let captured = captured::capture(&dir, &work).unwrap();
    let result = isolated::evaluate(&captured.dir, "host", &BTreeMap::new(), &Limits::default());
    let error = format!("{:#}", result.expect_err("read_file followed a link out of the tree"));
    assert!(error.contains("path leaves the config directory"), "read_file: {error}");

    let dir = config(
        &owner,
        "escape-up",
        "return { packages = { archix.read_file('../../../../../../tmp/archix-outside/mod.lua') } }\n",
    );
    let work = Work::new(&captures()).unwrap();
    let captured = captured::capture(&dir, &work).unwrap();
    let result = isolated::evaluate(&captured.dir, "host", &BTreeMap::new(), &Limits::default());
    let error = format!("{:#}", result.expect_err("read_file went up out of the tree"));
    assert!(
        error.contains("path leaves the config directory"),
        "read_file with ..: {error}"
    );
    fs::remove_dir_all(&outside).unwrap();
}

#[test]
fn a_runaway_config_is_stopped_by_its_limits() {
    let owner = setup();
    let limits = Limits {
        cpu_seconds: 2,
        memory_bytes: 1 << 30,
        wall: Duration::from_secs(20),
    };
    let began = std::time::Instant::now();
    let error = evaluate(&owner, "spin", "while true do end", &limits).unwrap_err();
    println!(
        "EVIDENCE limits: `while true do end` with RLIMIT_CPU 2 s stopped after {:.1} s: {error}",
        began.elapsed().as_secs_f64()
    );
    assert!(error.contains("CPU"), "{error}");
    let began = std::time::Instant::now();
    let error = evaluate(&owner, "huge", "local s = string.rep('x', 2^40) return {}", &limits).unwrap_err();
    println!(
        "EVIDENCE limits: `string.rep(\"x\", 2^40)` with RLIMIT_AS 1 GiB stopped after {:.1} s: {error}",
        began.elapsed().as_secs_f64()
    );
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
    let began = std::time::Instant::now();
    let error = evaluate(&owner, "wall", "while true do end", &sleeper).unwrap_err();
    println!(
        "EVIDENCE limits: `while true do end` with RLIMIT_CPU 60 s and a 2 s wall clock stopped after {:.1} s: {error}",
        began.elapsed().as_secs_f64()
    );
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
    let hash = || {
        sh("sha256sum /root/archix-target")
            .split_whitespace()
            .next()
            .unwrap()
            .to_string()
    };
    let before = hash();
    std::os::unix::fs::symlink("/root/archix-target", dir.join("installed.lua")).unwrap();
    sh(&format!("chown -h {USER}: {}/installed.lua", dir.display()));
    let error = format!(
        "{:#}",
        captured::write_as_owner(&dir.join("installed.lua"), b"planted").unwrap_err()
    );
    assert!(error.contains("isn't a regular file"), "{error}");
    let after = hash();
    println!(
        "EVIDENCE symlinked drift file: installed.lua -> /root/archix-target refused ({error}); target sha256 {before} before, {after} after"
    );
    assert_eq!(after, before, "the target changed through the link");
    let linked = owner.home.join("drift-link");
    let _ = fs::remove_file(&linked);
    std::os::unix::fs::symlink("/etc", &linked).unwrap();
    assert!(captured::write_as_owner(&linked.join("archix-planted"), b"x").is_err());
    assert!(!Path::new("/etc/archix-planted").exists());
    fs::remove_file(dir.join("installed.lua")).unwrap();
    let nested = dir.join("lua/hosts/box/installed.lua");
    captured::write_as_owner(&nested, b"return {}\n").unwrap();
    for made in [dir.join("lua"), dir.join("lua/hosts/box"), nested] {
        assert_eq!(
            fs::metadata(&made).unwrap().uid(),
            owner.uid,
            "{} is the owner's",
            made.display()
        );
    }
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
    assert_eq!(
        captured::redact_hashes("- initial_hashed_password = 'abJnggxhB/yWI', shell = \"/bin/sh\"\n"),
        "- initial_hashed_password = '<hash>', shell = \"/bin/sh\"\n",
        "a DES hash has no $ to spot it by"
    );
}

#[test]
fn the_lock_a_root_command_reads_is_the_captured_one_so_changing_it_after_the_capture_is_ignored() {
    let owner = setup();
    let dir = config(&owner, "lock-race", "return {}\n");
    fs::write(dir.join("archix.lock"), rendered_lock("2026-10-10")).unwrap();
    let swapped = dir.with_file_name("lock-race-swapped");
    fs::write(&swapped, rendered_lock("2001-02-03")).unwrap();
    sh(&format!("chown -R {USER}: {}", dir.display()));
    let race = format!("cp {} {}/archix.lock", swapped.display(), dir.display());
    let (_, said) = archix(&dir, Some(&race), &["status"]);
    assert!(
        said.contains("box is locked to snapshot 2026-10-10"),
        "the lock wasn't the captured one:\n{said}"
    );
    assert!(!said.contains("2001-02-03"), "{said}");
    assert!(
        fs::read_to_string(dir.join("archix.lock"))
            .unwrap()
            .contains("2001-02-03"),
        "the race never happened"
    );
}

#[test]
fn the_config_a_root_command_evaluates_is_the_captured_one_so_changing_it_after_the_capture_is_ignored() {
    let owner = setup();
    let dir = config(&owner, "eval-race", "return { packages = {} }\n");
    fs::write(dir.join("archix.lock"), rendered_lock("2026-10-10")).unwrap();
    sh(&format!("chown -R {USER}: {}", dir.display()));
    fs::set_permissions(&owner.home, fs::Permissions::from_mode(0o755)).unwrap();
    let race = format!("echo 'this is not lua' > {}/init.lua", dir.display());
    let (ok, said) = archix(&dir, Some(&race), &["status"]);
    println!(
        "EVIDENCE TOCTOU: init.lua replaced with `this is not lua` by ARCHIX_TEST_AFTER_CAPTURE, after the capture and before the evaluation; status exited {}",
        if ok { 0 } else { 1 }
    );
    assert!(ok && said.contains("box is locked to snapshot 2026-10-10"), "{said}");
    assert!(
        fs::read_to_string(dir.join("init.lua")).unwrap().contains("not lua"),
        "the race never happened"
    );
}

#[test]
fn the_recipes_a_root_command_reads_are_the_captured_ones_so_changing_them_after_the_capture_is_ignored() {
    let owner = setup();
    let dir = config(&owner, "recipe-race", "return { packages = { \"archix-race\" } }\n");
    let recipe = dir.join("pkgbuilds/archix-race");
    fs::create_dir_all(&recipe).unwrap();
    let pkgbuild = |version: &str| {
        format!("pkgname=archix-race\npkgver={version}\npkgrel=1\narch=(any)\nlicense=(MIT)\npackage() {{ :; }}\n")
    };
    fs::write(recipe.join("PKGBUILD"), pkgbuild("1.0")).unwrap();
    sh(&format!("chown -R {USER}: {}", dir.display()));
    let (ok, said) = archix(&dir, None, &["update", "--to", "2026-10-10", "--noconfirm"]);
    assert!(ok, "{said}");
    let stale = "the config changed since the lock";
    let (_, said) = archix(&dir, None, &["status"]);
    assert!(!said.contains(stale), "the lock should be fresh:\n{said}");
    let swapped = dir.with_file_name("recipe-race-swapped");
    fs::write(&swapped, pkgbuild("6.6.6")).unwrap();
    let race = format!("cp {} {}/PKGBUILD", swapped.display(), recipe.display());
    let (_, said) = archix(&dir, Some(&race), &["status"]);
    assert!(
        !said.contains(stale),
        "the recipe changed after the capture counted:\n{said}"
    );
    assert!(
        fs::read_to_string(recipe.join("PKGBUILD")).unwrap().contains("6.6.6"),
        "the race never happened"
    );
    let (_, said) = archix(&dir, None, &["status"]);
    assert!(
        said.contains(stale),
        "without a race the new recipe does count:\n{said}"
    );
}

#[test]
fn a_gitignored_lock_fails_loudly_rather_than_reading_as_no_lock() {
    let owner = setup();
    let dir = config(&owner, "lock-ignored", "return {}\n");
    fs::write(dir.join("archix.lock"), rendered_lock("2026-10-10")).unwrap();
    fs::write(dir.join(".gitignore"), "archix.lock\n").unwrap();
    sh(&format!("chown -R {USER}: {}", dir.display()));
    let (ok, said) = archix(&dir, None, &["status"]);
    assert!(
        !ok && said.contains("archix.lock") && said.contains("gitignore"),
        "{said}"
    );
}

#[test]
fn only_root_and_the_evaluator_can_read_a_capture() {
    let owner = setup();
    let dir = config(&owner, "private", "return { packages = { \"vim\" } }\n");
    fs::write(dir.join("notes.txt"), "not for other users\n").unwrap();
    sh(&format!("chown -R {USER}: {}", dir.display()));
    let work = Work::new(&captures()).unwrap();
    let captured = captured::capture(&dir, &work).unwrap();
    let mode = fs::metadata(&captured.dir).unwrap().mode() & 0o777;
    assert_eq!(mode & 0o007, 0, "others can reach the capture: {mode:o}");
    let attempt = |user: &str, what: String| {
        Command::new("runuser")
            .args(["-u", user, "--", "sh", "-c"])
            .arg(what)
            .output()
            .unwrap()
            .status
            .success()
    };
    for user in ["nobody", USER] {
        for path in [
            format!("cat {}/notes.txt", captured.dir.display()),
            format!("ls {}", captured.dir.display()),
            format!("find {} -type f -exec cat {{}} +", work.dir.join("root").display()),
        ] {
            assert!(!attempt(user, path.clone()), "{user} could run {path}");
        }
    }
    let config = isolated::evaluate(&captured.dir, "host", &BTreeMap::new(), &Limits::default()).unwrap();
    assert!(config.to_string().contains("vim"), "{config}");
    assert_eq!(
        fs::metadata(&captured.dir).unwrap().gid(),
        0,
        "the capture stays root's between evaluations"
    );
    assert_eq!(fs::metadata(&captured.dir).unwrap().mode() & 0o777, 0o700);
}

#[test]
fn a_generation_is_read_back_only_if_its_record_and_every_object_agree() {
    let owner = setup();
    let dir = config(&owner, "verified", "return { packages = { \"vim\" } }\n");
    let work = Work::new(&captures()).unwrap();
    let captured = captured::capture(&dir, &work).unwrap();
    let applied = Applied {
        git_dir: work.dir.join("root/applied.git"),
    };
    let tree = applied.store_tree(&captured).unwrap();
    let first = applied
        .record(1, &tree, b"first lock", b"{\"format\":1,\"config\":{}}")
        .unwrap();
    let second = applied
        .record(2, &tree, b"second lock", b"{\"format\":1,\"config\":{}}")
        .unwrap();
    let stored = applied.verified(1, &first.commit, Some(&tree)).unwrap();
    assert_eq!(stored.lock, b"first lock");

    let error = format!("{:#}", applied.verified(1, &second.commit, None).unwrap_err());
    assert!(error.contains("not the"), "the ref moved away from the record: {error}");
    let elsewhere = "0".repeat(40);
    let error = format!(
        "{:#}",
        applied.verified(1, &first.commit, Some(&elsewhere)).unwrap_err()
    );
    assert!(error.contains("tree") && error.contains(&elsewhere), "{error}");

    let materialized = work.dir.join("root/materialized");
    applied.materialize(&tree, &materialized).unwrap();
    assert!(materialized.join("init.lua").is_file());
    assert_eq!(fs::metadata(&materialized).unwrap().mode() & 0o777, 0o700);

    let blob = sh(&format!(
        "git --git-dir={} rev-parse {}:archix.lock",
        applied.git_dir.display(),
        first.commit
    ));
    let blob = blob.trim();
    let object = applied.git_dir.join("objects").join(&blob[..2]).join(&blob[2..]);
    fs::set_permissions(&object, fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(&object, b"not what was stored").unwrap();
    let error = format!("{:#}", applied.verified(1, &first.commit, Some(&tree)).unwrap_err());
    assert!(error.contains("damaged"), "a changed object must not be used: {error}");
}

#[test]
fn a_record_without_a_commit_still_reads_and_one_with_it_keeps_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".archix/snapshots/1");
    fs::create_dir_all(&path).unwrap();
    fs::write(
        path.join("record.json"),
        r#"{"format":1,"number":1,"kind":"switch","tree":"abc"}"#,
    )
    .unwrap();
    let store = archix::snapshot::Store::open(dir.path());
    let record = store.get(1).unwrap().unwrap();
    assert_eq!((record.tree.as_deref(), record.commit), (Some("abc"), None));
    fs::write(
        path.join("record.json"),
        r#"{"format":1,"number":1,"kind":"switch","tree":"abc","commit":"def","generation":4}"#,
    )
    .unwrap();
    let record = store.get(1).unwrap().unwrap();
    assert_eq!((record.commit.as_deref(), record.generation), (Some("def"), Some(4)));
}

#[test]
#[ignore]
fn the_lock_recorded_with_a_generation_is_the_one_applied_even_if_the_live_one_changed() {
    let _system = common::lock_system();
    let setup = common::Setup::new(&[], "");
    setup.first_switch();
    let lock_path = setup.config.join("archix.lock");
    let applied_lock = fs::read_to_string(&lock_path).unwrap();
    let swapped = setup.config.with_extension("swapped");
    fs::write(&swapped, rendered_lock("2001-02-03")).unwrap();
    setup.write(
        "init.lua",
        &format!("{}\n-- a change\n", setup.read("init.lua").unwrap()),
    );
    let race = format!("cp {} {}", swapped.display(), lock_path.display());
    setup.ok_env(&[("ARCHIX_TEST_AFTER_CAPTURE", &race)], &["switch", "--noconfirm"]);
    let store = archix::generations::Store::new(&setup.paths);
    let number = store.current().unwrap().unwrap();
    let generation = store.get(number).unwrap();
    let recorded = generation
        .applied
        .expect("the generation names its commit in root's repository");
    let root = Applied::at(&setup.paths.root, &setup.paths.state);
    let stored = root.verified(number, &recorded.commit, Some(&recorded.tree)).unwrap();
    let text = String::from_utf8(stored.lock).unwrap();
    assert!(
        !text.contains("2001-02-03"),
        "the recorded lock is the raced one:\n{text}"
    );
    assert_eq!(text, applied_lock);
    let _ = fs::remove_file(swapped);
}

//! The /etc merge into a pending deployment; run only inside the privileged Arch container via `tests/arch/run.sh`.

mod common;

use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use archix::etcmerge::{self, State};
use archix::paths::Paths;
use common::{Setup, lock_system};

fn sh(script: &str) -> String {
    let out = Command::new("bash").args(["-ec", script]).output().unwrap();
    assert!(
        out.status.success(),
        "{script}\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr)
}

const PASSWD: &str = "root:x:0:0::/root:/bin/bash\nalice:x:1000:1000::/home/alice:/bin/bash\n";
const GROUP: &str = "root:x:0:\nwheel:x:998:alice\nalice:x:1000:\n";
const SHADOW: &str = "root:$r:20000::::::\nalice:$a1:20000:0:99999:7:::\n";
const GSHADOW: &str = "root:::\nwheel:::alice\nalice:!::\n";

/// A root whose running deployment 1 has its /etc bound at /etc, as after its boot, with deployment 2 staged from it and pending.
struct Root {
    _dir: tempfile::TempDir,
    root: PathBuf,
    child: Option<Child>,
}

impl Root {
    fn new() -> Root {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let etc = root.join("archix/deploy/1/etc");
        for sub in [
            "NetworkManager/system-connections",
            "systemd/system/multi-user.target.wants",
            "conf.d",
        ] {
            fs::create_dir_all(etc.join(sub)).unwrap();
        }
        fs::set_permissions(
            etc.join("NetworkManager/system-connections"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        for (name, content, mode) in [
            ("passwd", PASSWD, 0o644),
            ("group", GROUP, 0o644),
            ("shadow", SHADOW, 0o600),
            ("gshadow", GSHADOW, 0o600),
            ("hostname", "old\n", 0o644),
            ("machine-id", "0123\n", 0o444),
            ("delete-me", "x\n", 0o644),
            ("owned", "x\n", 0o644),
            ("acl-file", "x\n", 0o640),
            ("conf.d/keep", "kept\n", 0o644),
        ] {
            fs::write(etc.join(name), content).unwrap();
            fs::set_permissions(etc.join(name), fs::Permissions::from_mode(mode)).unwrap();
        }
        for dir in ["etc", "run/archix", "var/lib/archix"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        sh(&format!(
            "mount --bind {} {}",
            etc.display(),
            root.join("etc").display()
        ));
        fs::write(root.join("run/archix/deployment"), "1\n").unwrap();
        std::os::unix::fs::symlink("deploy/1", root.join("archix/current")).unwrap();
        Root {
            _dir: dir,
            root,
            child: None,
        }
    }

    fn paths(&self) -> Paths {
        Paths::under(&self.root)
    }

    fn live(&self, rel: &str) -> PathBuf {
        self.root.join("etc").join(rel)
    }

    fn theirs(&self, rel: &str) -> PathBuf {
        self.root.join("archix/deploy/2/etc").join(rel)
    }

    fn point_current(&self, number: u32) {
        let tmp = self.root.join("archix/.current.tmp");
        std::os::unix::fs::symlink(format!("deploy/{number}"), &tmp).unwrap();
        fs::rename(&tmp, self.root.join("archix/current")).unwrap();
    }

    /// What a switch does: the base, then the staging copy, then `stage` as the transaction's changes, then the commit.
    fn stage(&self, stage: impl FnOnce(&Root)) {
        etcmerge::prepare(&self.paths(), 2).unwrap();
        sh(&format!(
            "mkdir -p {two}; cp -a {one}/etc {two}/etc",
            one = self.root.join("archix/deploy/1").display(),
            two = self.root.join("archix/deploy/2").display()
        ));
        stage(self);
        self.point_current(2);
    }

    fn state(&self) -> Option<State> {
        etcmerge::load(&self.paths(), 2)
    }

    fn start(&mut self, env: &[(&str, &str)]) {
        let child = Command::new(env!("CARGO_BIN_EXE_archix"))
            .args(["etc-merge", "--root"])
            .arg(&self.root)
            .envs(env.iter().copied())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        self.child = Some(child);
        self.wait_for("the first pass", || self.state().is_some_and(|s| s.last_pass > 0));
    }

    fn signal(&self, signal: i32) {
        let pid = self.child.as_ref().unwrap().id() as i32;
        unsafe { libc::kill(pid, signal) };
    }

    /// Stops the watcher as shutdown does and returns its output.
    fn stop(&mut self) -> Output {
        self.signal(libc::SIGTERM);
        let started = Instant::now();
        let out = self.child.take().unwrap().wait_with_output().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the pass at shutdown took {:?}",
            started.elapsed()
        );
        out
    }

    fn kill(&mut self) -> Output {
        self.signal(libc::SIGKILL);
        self.child.take().unwrap().wait_with_output().unwrap()
    }

    fn wait_for(&self, what: &str, done: impl Fn() -> bool) {
        let started = Instant::now();
        while !done() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "waited 10 s for {what}; state: {:?}",
                self.state()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// The reboot into deployment 2: its /etc is bound at /etc, and the boot runs `archix etc-merge --boot`.
    fn reboot(&self) -> String {
        sh(&format!(
            "umount {etc}; mount --bind {two} {etc}",
            etc = self.root.join("etc").display(),
            two = self.theirs("").display()
        ));
        fs::write(self.root.join("run/archix/deployment"), "2\n").unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_archix"))
            .args(["etc-merge", "--boot", "--root"])
            .arg(&self.root)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", text(&out));
        text(&out)
    }

    fn notes(&self) -> String {
        fs::read_to_string(self.paths().notes()).unwrap_or_default()
    }

    fn status(&self) -> String {
        etcmerge::status_lines(&self.paths()).join("\n")
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = Command::new("umount")
            .arg("-R")
            .arg("-l")
            .arg(self.root.join("etc"))
            .status();
        let _ = Command::new("umount").arg("-l").arg(self.root.join("archix")).status();
    }
}

/// Writes `rel` as an editor or NetworkManager does: a temporary file, then a rename over the original.
fn write_renamed(etc: &Path, rel: &str, content: &str, mode: u32) {
    let path = etc.join(rel);
    let tmp = path.with_file_name(format!(".{}.XYZ123", path.file_name().unwrap().to_string_lossy()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .unwrap();
    file.write_all(content.as_bytes()).unwrap();
    file.sync_all().unwrap();
    fs::rename(&tmp, &path).unwrap();
}

fn xattr(path: &Path, name: &str) -> Option<Vec<u8>> {
    archix::deploy::xattr::get(path, &CString::new(name).unwrap()).unwrap()
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

#[test]
#[ignore]
fn live_edits_reach_the_pending_deployment_within_seconds_with_modes_owners_xattrs_and_acls() {
    let _system = lock_system();
    let mut root = Root::new();
    root.stage(|root| {
        fs::write(root.theirs("hostname"), "package\n").unwrap();
        fs::write(root.theirs("only-theirs"), "the package's\n").unwrap();
        fs::write(root.theirs("machine-id"), "fresh\n").unwrap();
    });
    root.start(&[]);
    let live = root.root.join("etc");
    write_renamed(
        &live,
        "NetworkManager/system-connections/wifi.nmconnection",
        "[wifi]\npsk=secret\n",
        0o600,
    );
    fs::create_dir_all(live.join("newdir/sub")).unwrap();
    fs::write(live.join("newdir/sub/file"), "in a new directory\n").unwrap();
    fs::remove_file(live.join("delete-me")).unwrap();
    std::os::unix::fs::symlink(
        "/usr/lib/systemd/system/foo.service",
        live.join("systemd/system/multi-user.target.wants/foo.service"),
    )
    .unwrap();
    std::os::unix::fs::chown(live.join("owned"), Some(1234), Some(1235)).unwrap();
    fs::set_permissions(live.join("owned"), fs::Permissions::from_mode(0o4750)).unwrap();
    archix::deploy::xattr::set(&live.join("owned"), c"user.archix", b"yes").unwrap();
    archix::deploy::xattr::set(&live.join("owned"), c"trusted.archix", b"trusted").unwrap();
    sh(&format!("setfacl -m u:1234:r {}", live.join("acl-file").display()));
    write_renamed(&live, "hostname", "mine\n", 0o644);
    write_renamed(&live, "machine-id", "4567\n", 0o444);
    fs::write(live.join(".updated"), "live stamp\n").unwrap();
    fs::write(live.join("conf.d/keep"), "edited in place\n").unwrap();

    let nm = root.theirs("NetworkManager/system-connections/wifi.nmconnection");
    root.wait_for("every change", || {
        read(&nm) == "[wifi]\npsk=secret\n"
            && read(&root.theirs("newdir/sub/file")) == "in a new directory\n"
            && !root.theirs("delete-me").exists()
            && root
                .theirs("systemd/system/multi-user.target.wants/foo.service")
                .is_symlink()
            && fs::metadata(root.theirs("owned")).is_ok_and(|m| m.uid() == 1234)
            && xattr(&root.theirs("acl-file"), "system.posix_acl_access").is_some()
            && read(&root.theirs("hostname")) == "mine\n"
            && read(&root.theirs("conf.d/keep")) == "edited in place\n"
            && read(&root.theirs("machine-id")) == "4567\n"
    });
    let meta = fs::metadata(&nm).unwrap();
    assert_eq!(meta.mode() & 0o7777, 0o600, "a 0600 connection stays 0600");
    assert_eq!((meta.uid(), meta.gid()), (0, 0));
    assert_eq!(
        fs::metadata(root.theirs("NetworkManager/system-connections"))
            .unwrap()
            .mode()
            & 0o7777,
        0o700
    );
    let owned = fs::metadata(root.theirs("owned")).unwrap();
    assert_eq!((owned.uid(), owned.gid(), owned.mode() & 0o7777), (1234, 1235, 0o4750));
    assert_eq!(
        xattr(&root.theirs("owned"), "user.archix").as_deref(),
        Some(&b"yes"[..])
    );
    assert_eq!(
        xattr(&root.theirs("owned"), "trusted.archix").as_deref(),
        Some(&b"trusted"[..])
    );
    assert_eq!(
        sh(&format!("getfacl -cp {}", root.theirs("acl-file").display())),
        sh(&format!("getfacl -cp {}", live.join("acl-file").display()))
    );
    assert_eq!(
        fs::read_link(root.theirs("systemd/system/multi-user.target.wants/foo.service")).unwrap(),
        Path::new("/usr/lib/systemd/system/foo.service"),
        "a link into /usr/lib is copied as a link, never followed"
    );
    assert_eq!(read(&root.theirs("hostname.pacnew")), "package\n");
    assert_eq!(read(&root.theirs("only-theirs")), "the package's\n");
    assert!(!root.theirs("machine-id.pacnew").exists());
    assert!(!root.theirs(".updated").exists(), ".updated is never carried");
    let leftovers = sh(&format!(
        "find {} -name '*.archix-merge*' -o -name '.*XYZ123'",
        root.theirs("").display()
    ));
    assert_eq!(leftovers, "", "no temporary file reaches the deployment");
    let status = root.status();
    assert!(
        status.contains("/etc changes carried into the pending deployment 2, 1 conflicts (.pacnew)")
            && status.contains("/etc/hostname: changed on the running system and in deployment 2"),
        "{status}"
    );
    let out = root.stop();
    assert!(out.status.success(), "{}", text(&out));
}

#[test]
#[ignore]
fn account_files_merge_by_name_and_a_number_clash_stops_them_until_fixed() {
    let _system = lock_system();
    let mut root = Root::new();
    root.stage(|root| {
        for (file, line) in [
            ("passwd", "pkguser:x:960:960::/:/usr/bin/nologin\n"),
            ("group", "pkguser:x:960:\n"),
            ("shadow", "pkguser:!*:20050::::::\n"),
        ] {
            let mut text = read(&root.theirs(file));
            text.push_str(line);
            fs::write(root.theirs(file), text).unwrap();
        }
    });
    root.start(&[]);
    let live = root.root.join("etc");
    write_renamed(&live, "shadow", &SHADOW.replace("$a1", "$a2"), 0o600);
    write_renamed(
        &live,
        "passwd",
        &format!("{PASSWD}bob:x:1001:1001::/home/bob:/bin/bash\n"),
        0o644,
    );
    write_renamed(
        &live,
        "group",
        &format!("{}bob:x:1001:\n", GROUP.replace("alice\n", "alice,bob\n")),
        0o644,
    );
    root.wait_for("the accounts", || {
        read(&root.theirs("shadow")).contains("alice:$a2:") && read(&root.theirs("group")).contains("bob:x:1001:")
    });
    assert_eq!(
        read(&root.theirs("passwd")),
        format!("{PASSWD}pkguser:x:960:960::/:/usr/bin/nologin\nbob:x:1001:1001::/home/bob:/bin/bash\n")
    );
    assert!(read(&root.theirs("group")).contains("wheel:x:998:alice,bob\n"));
    assert_eq!(fs::metadata(root.theirs("shadow")).unwrap().mode() & 0o7777, 0o600);
    assert!(read(&root.theirs("shadow")).contains("pkguser:!*"));

    let with_carol = format!("{PASSWD}bob:x:1001:1001::/home/bob:/bin/bash\ncarol:x:960:960::/home/carol:/bin/bash\n");
    write_renamed(&live, "passwd", &with_carol, 0o644);
    root.wait_for("the clash", || root.state().is_some_and(|s| s.clash.is_some()));
    assert!(!read(&root.theirs("passwd")).contains("carol"));
    let clash = root.state().unwrap().clash.unwrap();
    assert!(
        clash.contains("UID 960 would be both carol's") && clash.contains("pkguser's in deployment 2"),
        "{clash}"
    );
    assert!(
        root.notes()
            .contains("stopped carrying users and groups into deployment 2"),
        "{}",
        root.notes()
    );
    assert!(
        root.status().contains("users and groups aren't carried: UID 960"),
        "{}",
        root.status()
    );

    write_renamed(
        &live,
        "passwd",
        &with_carol.replace("carol:x:960:960", "carol:x:1002:1002"),
        0o644,
    );
    root.wait_for("the fix", || read(&root.theirs("passwd")).contains("carol:x:1002:"));
    assert_eq!(root.state().unwrap().clash, None);

    write_renamed(
        &live,
        "passwd",
        &format!("{with_carol}dave:x:960:960::/:/bin/bash\n").replace("carol:x:960:960", "carol:x:1002:1002"),
        0o644,
    );
    root.wait_for("the second clash", || root.state().is_some_and(|s| s.clash.is_some()));
    root.stop();
    let boot = root.reboot();
    assert!(
        boot.contains("weren't carried into deployment 2: UID 960 would be both dave's"),
        "{boot}"
    );
    assert!(root.notes().contains("dave's"), "{}", root.notes());
    assert!(
        !read(&root.live("passwd")).contains("dave"),
        "the clash never reaches the deployment"
    );

    fs::create_dir_all(root.root.join("archix/deploy/2/pacman")).unwrap();
    let activate = |root: &Root| {
        Command::new(env!("CARGO_BIN_EXE_archix"))
            .args(["deploy-activate", "--root"])
            .arg(&root.root)
            .output()
            .unwrap()
    };
    let mut passwd = read(&root.live("passwd"));
    passwd.push_str("dave:x:960:960::/:/bin/bash\n");
    fs::write(root.live("passwd"), passwd).unwrap();
    let out = activate(&root);
    assert!(
        !out.status.success() && text(&out).contains("UID 960 is shared by"),
        "a clash that reaches a deployment some other way stops its activation: {}",
        text(&out)
    );
}

#[test]
#[ignore]
fn an_overflowed_event_queue_is_caught_up_by_a_full_pass() {
    let _system = lock_system();
    let mut root = Root::new();
    root.stage(|_| {});
    root.start(&[("ARCHIX_TEST_ETC_MERGE_QUEUE", "2")]);
    root.signal(libc::SIGSTOP);
    let live = root.root.join("etc");
    for n in 0..5 {
        fs::write(live.join(format!("burst-{n}")), format!("{n}\n")).unwrap();
    }
    fs::create_dir_all(live.join("burst-dir/deeper")).unwrap();
    fs::write(live.join("burst-dir/deeper/file"), "deep\n").unwrap();
    fs::remove_file(live.join("delete-me")).unwrap();
    root.signal(libc::SIGCONT);
    root.wait_for("the rescan", || {
        (0..5).all(|n| root.theirs(&format!("burst-{n}")).exists())
            && root.theirs("burst-dir/deeper/file").exists()
            && !root.theirs("delete-me").exists()
    });
    fs::write(live.join("burst-dir/deeper/later"), "watched\n").unwrap();
    root.wait_for("a file in the rescanned directory", || {
        root.theirs("burst-dir/deeper/later").exists()
    });
    let out = root.stop();
    assert!(text(&out).contains("event queue overflowed"), "{}", text(&out));
}

#[test]
#[ignore]
fn the_shutdown_pass_carries_the_last_change_and_the_boot_records_a_clean_merge() {
    let _system = lock_system();
    let mut root = Root::new();
    root.stage(|_| {});
    root.start(&[]);
    fs::write(root.live("written-at-shutdown"), "last\n").unwrap();
    let out = root.stop();
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(read(&root.theirs("written-at-shutdown")), "last\n");
    assert!(root.state().unwrap().final_pass);
    let boot = root.reboot();
    assert!(!boot.contains("didn't shut down cleanly"), "{boot}");
    assert!(root.notes().is_empty(), "{}", root.notes());
    assert!(root.state().is_none(), "the merge is done once its deployment booted");
    let status = root.status();
    assert!(
        status.contains("/etc changes were carried from deployment 1 into this one, 0 conflicts"),
        "{status}"
    );
}

#[test]
#[ignore]
fn a_power_cut_in_the_window_is_carried_from_the_old_etc_at_boot_and_noted() {
    let _system = lock_system();
    let mut root = Root::new();
    root.stage(|_| {});
    root.start(&[]);
    root.kill();
    fs::write(root.live("after-the-last-pass"), "on disk\n").unwrap();
    write_renamed(&root.root.join("etc"), "shadow", &SHADOW.replace("$a1", "$cut"), 0o600);
    let boot = root.reboot();
    assert!(
        boot.contains("deployment 1 didn't shut down cleanly before deployment 2 first booted")
            && boot.contains("/etc/after-the-last-pass"),
        "{boot}"
    );
    assert_eq!(read(&root.live("after-the-last-pass")), "on disk\n");
    assert!(read(&root.live("shadow")).contains("alice:$cut:"));
    assert!(root.notes().contains("didn't shut down cleanly"), "{}", root.notes());
    assert!(
        root.status().contains("deployment 1 didn't shut down cleanly"),
        "{}",
        root.status()
    );
}

#[test]
#[ignore]
fn a_shutdown_pass_that_fails_runs_out_of_time_or_is_killed_never_blocks_and_is_shown_at_boot() {
    let _system = lock_system();
    for (case, env) in [
        ("failed", ("ARCHIX_TEST_FAIL", "etc-merge:troubled")),
        ("out of time", ("ARCHIX_TEST_ETC_MERGE_DEADLINE_MS", "0")),
        ("killed", ("ARCHIX_TEST_CRASH", "etc-merge-final-pass")),
    ] {
        let mut root = Root::new();
        root.stage(|_| {});
        root.start(&[env]);
        fs::write(root.live("troubled"), "late\n").unwrap();
        let out = root.stop();
        if case != "killed" {
            assert!(out.status.success(), "{case}: {}", text(&out));
            let state = root.state().unwrap();
            let recorded = match case {
                "failed" => state.failed.contains_key("troubled"),
                _ => state.partial > 0 && state.failed.is_empty(),
            };
            assert!(recorded, "{case}: {state:?}");
            assert!(!root.theirs("troubled").exists(), "{case}");
        }
        let boot = root.reboot();
        let named = match case {
            "killed" => boot.contains("didn't shut down cleanly") && boot.contains("/etc/troubled"),
            "out of time" => boot.contains("ran out of time with") && boot.contains("/etc/troubled"),
            _ => boot.contains(
                "couldn't carry these changes made on deployment 1 into deployment 2 before the reboot: /etc/troubled",
            ),
        };
        assert!(named, "{case}: {boot}");
        assert!(root.notes().contains("/etc/troubled"), "{case}: {}", root.notes());
        assert_eq!(
            read(&root.live("troubled")),
            "late\n",
            "{case}: the boot carries it from the old /etc"
        );
    }
}

#[test]
#[ignore]
fn the_merge_writes_through_a_namespace_of_its_own_when_archix_is_read_only() {
    let _system = lock_system();
    let mut root = Root::new();
    root.stage(|_| {});
    let archix = root.root.join("archix");
    sh(&format!(
        "mount --bind {a} {a}; mount -o remount,bind,ro {a}",
        a = archix.display()
    ));
    let err = fs::write(root.theirs("from-the-test"), "x").unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EROFS));
    root.start(&[]);
    fs::write(root.live("through-the-namespace"), "x\n").unwrap();
    root.wait_for("the write", || root.theirs("through-the-namespace").exists());
    root.stop();
}

#[test]
#[ignore]
fn a_rollback_carries_people_and_passwords_into_the_older_etc_and_keeps_its_modes() {
    let _system = lock_system();
    let root = Root::new();
    let older = root.root.join("older-etc");
    sh(&format!("cp -a {} {}", root.live("").display(), older.display()));
    write_renamed(
        &root.root.join("etc"),
        "shadow",
        &format!("{}bob:$b:1::::::\n", SHADOW.replace("$a1", "$new")),
        0o600,
    );
    write_renamed(
        &root.root.join("etc"),
        "passwd",
        &format!("{PASSWD}bob:x:1001:1001::/home/bob:/bin/bash\n"),
        0o644,
    );
    let skipped = etcmerge::carry_accounts(&root.paths(), &root.live(""), &older).unwrap();
    assert!(skipped.is_empty(), "{skipped:?}");
    assert!(read(&older.join("passwd")).contains("bob:x:1001:"));
    assert!(read(&older.join("shadow")).contains("alice:$new:") && read(&older.join("shadow")).contains("bob:$b:"));
    assert_eq!(fs::metadata(older.join("shadow")).unwrap().mode() & 0o7777, 0o600);
    let registry = archix::etcmerge::ids::Registry::load(&root.paths().state);
    assert_eq!(registry.uids.get(&1001).map(String::as_str), Some("bob"));
}

/// A mark program that edits the live /etc while the transaction runs, after the staging copy and before the commit.
fn edit_during_transaction(machine_root: &Path) -> PathBuf {
    let script = machine_root.join("edit-during-transaction.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\n[ \"$1\" = deploy-commit-sync ] || exit 0\necho edited > {etc}/during-transaction\necho live > {etc}/archix-test-merge\n",
            etc = machine_root.join("etc").display()
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    script
}

#[test]
#[ignore]
fn a_switch_carries_edits_made_during_its_transaction_and_until_shutdown_but_not_over_its_managed_files() {
    let _system = lock_system();
    let body = |n: u32| format!(r#"files = {{ ["/etc/archix-test-merge"] = {{ text = "{n}" }} }}"#);
    let setup = Setup::new(&[], &body(0));
    setup.lay_out();
    let machine = &setup.machine;
    let booted = machine.current().unwrap();
    setup.set_body(&[], &body(1));
    let script = edit_during_transaction(&machine.root);
    setup.ok_env(
        &[
            ("ARCHIX_TEST_MARK", script.to_str().unwrap()),
            ("ARCHIX_TEST_ETC_MERGE", "1"),
        ],
        &["switch", "--noconfirm"],
    );
    let pending = machine.current().unwrap();
    assert_ne!(pending, booted);
    let unit = etcmerge::unit_name(&machine.root);
    let etc = machine.deployment(pending).join("etc");
    let wait = |what: &str, done: &dyn Fn() -> bool| {
        let started = Instant::now();
        while !done() {
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "waited for {what}: {}",
                sh(&format!("journalctl -u {unit} --no-pager | tail -20"))
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    };
    wait("the edit made during the transaction", &|| {
        read(&etc.join("during-transaction")) == "edited\n"
    });
    fs::write(machine.root.join("etc/after-the-commit"), "later\n").unwrap();
    wait("the edit after the commit", &|| {
        read(&etc.join("after-the-commit")) == "later\n"
    });
    assert_eq!(
        read(&etc.join("archix-test-merge")),
        "1\n",
        "archix's new version of its own file stays"
    );
    assert!(!etc.join("archix-test-merge.pacnew").exists());
    let status = setup.ok(&["status"]);
    assert!(
        status.contains(&format!("/etc changes carried into the pending deployment {pending}")),
        "{status}"
    );
    sh(&format!("systemctl stop {unit}"));
    let state = etcmerge::load(&Paths::under(&machine.root), pending).unwrap();
    assert!(state.final_pass, "{state:?}");
    machine.reboot(Some(&format!("archix-{pending}.conf")));
    let out = Command::new(env!("CARGO_BIN_EXE_archix"))
        .args(["etc-merge", "--boot", "--root"])
        .arg(&machine.root)
        .output()
        .unwrap();
    assert!(
        out.status.success() && !text(&out).contains("didn't shut down cleanly"),
        "{}",
        text(&out)
    );
    let status = setup.ok(&["status"]);
    assert!(
        status.contains(&format!("were carried from deployment {booted} into this one")),
        "{status}"
    );
    assert_eq!(read(&machine.root.join("etc/during-transaction")), "edited\n");
}

#[test]
#[ignore]
fn an_install_with_the_config_in_etc_shows_the_carried_changes_and_the_conflict_in_status() {
    let _system = lock_system();
    let mut setup = Setup::new(&[], "");
    setup.lay_out();
    setup.move_config_to_etc();
    let machine = &setup.machine;
    let booted = machine.current().unwrap();
    setup.ok_env(&[("ARCHIX_TEST_ETC_MERGE", "1")], &["install", "tree", "--noconfirm"]);
    let pending = machine.current().unwrap();
    assert_ne!(pending, booted);
    let unit = etcmerge::unit_name(&machine.root);
    let etc = machine.deployment(pending).join("etc");
    let wait = |what: &str, done: &dyn Fn() -> bool| {
        let started = Instant::now();
        while !done() {
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "waited for {what}: {}",
                sh(&format!("journalctl -u {unit} --no-pager | tail -20"))
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    };
    wait("the watcher's first pass", &|| {
        etcmerge::load(&Paths::under(&machine.root), pending).is_some_and(|s| s.last_pass > 0)
    });
    machine.store_writable(true);
    fs::write(etc.join("shells"), "/bin/sh\n/bin/zsh\n").unwrap();
    machine.store_writable(false);
    fs::write(machine.root.join("etc/shells"), "/bin/sh\n/bin/vmtest-shell\n").unwrap();
    fs::create_dir_all(machine.root.join("etc/vmtest-dir/sub")).unwrap();
    fs::write(machine.root.join("etc/vmtest-dir/sub/file"), "made after the switch\n").unwrap();
    wait("the live edit", &|| {
        read(&etc.join("shells")).contains("/bin/vmtest-shell")
    });
    let status = setup.ok(&["status"]);
    sh(&format!("systemctl stop {unit}"));
    assert!(
        status.contains(&format!("/etc changes carried into the pending deployment {pending}"))
            && status.contains("/etc/shells: changed on the running system"),
        "{status}"
    );
}

#[test]
#[ignore]
fn a_switch_without_systemd_running_commits_without_an_etc_merge_or_a_failed_step() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    setup.lay_out();
    let booted = setup.machine.current().unwrap();
    setup.set_body(&[], r#"files = { ["/etc/archix-test-chroot"] = { text = "1" } }"#);
    let out = Command::new("unshare")
        .args(["-m", "--propagation", "private", "sh", "-c"])
        .arg("mount -t tmpfs tmpfs /run/systemd && exec \"$@\"")
        .arg("sh")
        .arg(env!("CARGO_BIN_EXE_archix"))
        .args([
            "--config",
            setup.config.to_str().unwrap(),
            "--host",
            "box",
            "switch",
            "--noconfirm",
        ])
        .envs(setup.machine.env())
        .env("ARCHIX_TEST_ETC_MERGE", "1")
        .output()
        .unwrap();
    assert!(out.status.success() && !text(&out).contains("failed"), "{}", text(&out));
    assert_ne!(setup.machine.current().unwrap(), booted);
    let unit = etcmerge::unit_name(&setup.machine.root);
    let active = Command::new("systemctl")
        .args(["is-active", "--quiet", &unit])
        .status()
        .unwrap();
    assert!(!active.success(), "{unit} shouldn't run");
}

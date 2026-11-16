//! The rescoped switch (SCOPE 2.2, 3.4, 4, 5) against the test VM's own root; run only inside it via `tests/arch/run.sh --test switch_v2`.

mod common;

use std::fs;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::*;

const HOST: &str = "switchbox";
const RECORD: &str = "/var/lib/archix/last-applied.json";
const SWITCHES: &str = "/var/log/archix/switches";

fn archix(config: &str, args: &[&str]) -> (Option<i32>, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_archix"))
        .args(["--config", config, "--host", HOST])
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

fn spawn(config: &str, args: &[&str]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_archix"))
        .args(["--config", config, "--host", HOST])
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn finish(child: Child) -> (Option<i32>, String) {
    let output = child.wait_with_output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.code(), text)
}

const SWITCH: &[&str] = &["switch", "--noconfirm", "--no-snapshot"];

/// What every switch test leaves behind, put back when dropped: archix's record, the config dir and whatever `undo` names.
struct Scene {
    _dir: tempfile::TempDir,
    config: String,
    _record: RestoreFile,
    undo: Vec<String>,
}

impl Scene {
    /// A config `archix init` wrote for this machine, plus a module with `fields`.
    fn new(fields: &str) -> Scene {
        use std::os::unix::fs::PermissionsExt;
        let record = RestoreFile::new(Path::new(RECORD));
        let _ = fs::remove_file(RECORD);
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let config = dir.path().join("cfg").to_str().unwrap().to_string();
        let (code, out) = archix(&config, &["init"]);
        assert_eq!(code, Some(0), "init failed:\n{out}");
        let scene = Scene {
            _dir: dir,
            config,
            _record: record,
            undo: Vec::new(),
        };
        scene.manage(fields);
        scene
    }

    fn manage(&self, fields: &str) {
        fs::write(
            format!("{}/init.lua", self.config),
            "return { imports = { \"hosts.\" .. archix.hostname, \"managed\" } }\n",
        )
        .unwrap();
        fs::create_dir_all(format!("{}/lua", self.config)).unwrap();
        fs::write(
            format!("{}/lua/managed.lua", self.config),
            format!("return {{ {fields} }}\n"),
        )
        .unwrap();
    }

    fn undo(&mut self, script: &str) {
        self.undo.push(script.to_string());
    }

    fn run(&self, args: &[&str]) -> (Option<i32>, String) {
        archix(&self.config, args)
    }

    fn diff_json(&self) -> serde_json::Value {
        let output = Command::new(env!("CARGO_BIN_EXE_archix"))
            .args(["--config", &self.config, "--host", HOST, "diff", "--json"])
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&output.stderr);
        assert!(
            matches!(output.status.code(), Some(0) | Some(1)),
            "diff --json:\n{said}"
        );
        serde_json::from_slice(&output.stdout).unwrap_or_else(|e| panic!("{e}:\n{said}"))
    }

    fn assert_converged(&self, what: &str) {
        let (code, text) = self.run(&["diff"]);
        assert_eq!(
            code,
            Some(0),
            "{what}: diff after the switch should have nothing to do:\n{text}"
        );
    }
}

impl Drop for Scene {
    fn drop(&mut self) {
        for script in self.undo.iter().rev() {
            let _ = Command::new("bash").args(["-c", script]).status();
        }
    }
}

fn latest_journal() -> PathBuf {
    let n = archix::journal::list(Path::new("/"))
        .unwrap()
        .into_iter()
        .max()
        .expect("a journal");
    PathBuf::from(SWITCHES).join(n.to_string())
}

fn journal_count() -> usize {
    archix::journal::list(Path::new("/")).unwrap().len()
}

fn meta(dir: &Path) -> archix::journal::Meta {
    serde_json::from_str(&fs::read_to_string(dir.join("meta.json")).unwrap()).unwrap()
}

fn events(dir: &Path) -> Vec<archix::journal::Event> {
    fs::read_to_string(dir.join("journal.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn actions(dir: &Path) -> Vec<(archix::journal::ActionKind, String)> {
    events(dir)
        .into_iter()
        .filter_map(|e| match e {
            archix::journal::Event::Action { kind, target, .. } => Some((kind, target)),
            _ => None,
        })
        .collect()
}

fn pacman_ok(args: &[&str]) -> bool {
    Command::new("pacman").args(args).status().unwrap().success()
}

/// pacman takes and releases its lock: nothing archix did left the database locked.
fn assert_pacman_usable() {
    assert!(!Path::new("/var/lib/pacman/db.lck").exists(), "db.lck is left behind");
    assert!(
        pacman_ok(&["-D", "--asexplicit", "pacman"]),
        "pacman can't write its database"
    );
}

fn wait_for(path: &str, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(300);
    while !Path::new(path).exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A pacman hook that holds a transaction installing `target` until the test lets it go, so the test can act mid-commit.
struct Pause {
    hook: PathBuf,
    paused: String,
    go: String,
}

impl Pause {
    fn on(target: &str) -> Pause {
        let tag = format!("archix-test-pause-{target}");
        let (paused, go) = (format!("/tmp/{tag}.paused"), format!("/tmp/{tag}.go"));
        let _ = fs::remove_file(&paused);
        let _ = fs::remove_file(&go);
        let hook = PathBuf::from(format!("/etc/pacman.d/hooks/zz-{tag}.hook"));
        fs::create_dir_all(hook.parent().unwrap()).unwrap();
        fs::write(
            &hook,
            format!(
                "[Trigger]\nOperation = Install\nType = Package\nTarget = {target}\n[Action]\nDescription = archix test pause\nWhen = PreTransaction\nExec = /usr/bin/sh -c 'touch {paused}; while [ ! -e {go} ]; do sleep 0.1; done'\n"
            ),
        )
        .unwrap();
        Pause { hook, paused, go }
    }

    fn wait(&self) {
        wait_for(&self.paused, "the paused transaction");
    }

    fn release(&self) {
        fs::write(&self.go, "").unwrap();
    }
}

impl Drop for Pause {
    fn drop(&mut self) {
        let _ = fs::write(&self.go, "");
        let _ = fs::remove_file(&self.hook);
    }
}

#[test]
#[ignore]
fn fidelity_packages_install_and_remove() {
    let _system = lock_system();
    let mut scene = Scene::new(r#"packages = { "tree" }"#);
    scene.undo("pacman -Rns --noconfirm tree");
    let plan = scene.diff_json();
    let installs: Vec<&str> = plan["packages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["action"] == "install")
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert_eq!(installs, ["tree"], "{plan:#}");
    let (code, text) = scene.run(SWITCH);
    assert_eq!(code, Some(0), "{text}");
    assert!(installed("tree").is_some());
    let journal = latest_journal();
    assert_eq!(meta(&journal).outcome, archix::journal::Outcome::Ok);
    assert!(
        actions(&journal)
            .iter()
            .any(|(k, t)| *k == archix::journal::ActionKind::Install && t.starts_with("tree ")),
        "{:?}",
        actions(&journal)
    );
    let window = meta(&journal).alpm_window;
    assert!(window.start.is_some() && window.end.is_some(), "{window:?}");
    assert_pacman_usable();
    scene.assert_converged("install");

    scene.manage("");
    let (code, text) = scene.run(SWITCH);
    assert_eq!(code, Some(0), "{text}");
    assert!(installed("tree").is_none(), "no longer declared, so removed");
    assert!(
        actions(&latest_journal())
            .iter()
            .any(|(k, t)| *k == archix::journal::ActionKind::Remove && t.starts_with("tree ")),
    );
    scene.assert_converged("remove");
}

#[test]
#[ignore]
fn fidelity_a_file() {
    let _system = lock_system();
    let path = "/etc/archix-switch-fidelity.conf";
    let _restore = RestoreFile::new(Path::new(path));
    let mut scene = Scene::new(&format!(
        "files = {{ [{path:?}] = {{ text = \"one\\n\", mode = \"0640\" }} }}"
    ));
    scene.undo(&format!("rm -f {path}.archix-save"));
    let (code, text) = scene.run(SWITCH);
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(fs::read_to_string(path).unwrap(), "one\n");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o7777, 0o640);
    scene.assert_converged("create");

    fs::write(path, "edited by hand\n").unwrap();
    scene.manage(&format!(
        "files = {{ [{path:?}] = {{ text = \"two\\n\", mode = \"0640\" }} }}"
    ));
    let (code, text) = scene.run(SWITCH);
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(fs::read_to_string(path).unwrap(), "two\n");
    assert_eq!(
        fs::read_to_string(format!("{path}.archix-save")).unwrap(),
        "edited by hand\n",
        "SCOPE 6: the hand edit is kept"
    );
    scene.assert_converged("modify");
}

#[test]
#[ignore]
fn fidelity_a_user() {
    let _system = lock_system();
    let name = "archix-swuser";
    let mut scene = Scene::new(&format!(
        "users = {{ [{name:?}] = {{ system = true, shell = \"/usr/bin/nologin\", description = \"archix switch test\" }} }}"
    ));
    scene.undo(&format!("userdel {name}; groupdel {name}"));
    let (code, text) = scene.run(SWITCH);
    assert_eq!(code, Some(0), "{text}");
    let passwd = fs::read_to_string("/etc/passwd").unwrap();
    assert!(
        passwd
            .lines()
            .any(|l| l.starts_with(&format!("{name}:")) && l.contains("archix switch test")),
        "{passwd}"
    );
    assert!(
        actions(&latest_journal())
            .iter()
            .any(|(k, t)| *k == archix::journal::ActionKind::User && t == name)
    );
    scene.assert_converged("user");
}

#[test]
#[ignore]
fn fidelity_a_service() {
    let _system = lock_system();
    let unit = "archix-switch-test.service";
    let mut scene = Scene::new(&format!(
        "units = {{ [{unit:?}] = \"[Unit]\\nDescription=archix switch test\\n[Service]\\nType=oneshot\\nRemainAfterExit=yes\\nExecStart=/usr/bin/true\\n[Install]\\nWantedBy=multi-user.target\\n\" }}, services = {{ [{unit:?}] = true }}"
    ));
    scene.undo(&format!(
        "systemctl disable --now {unit}; rm -f /etc/systemd/system/{unit}; systemctl daemon-reload"
    ));
    let (code, text) = scene.run(SWITCH);
    assert_eq!(code, Some(0), "{text}");
    assert!(
        Command::new("systemctl")
            .args(["is-enabled", "--quiet", unit])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("systemctl")
            .args(["is-active", "--quiet", unit])
            .status()
            .unwrap()
            .success()
    );
    scene.assert_converged("service");
}

#[test]
#[ignore]
fn a_saved_plan_is_refused_after_drift_and_applied_when_nothing_moved() {
    let _system = lock_system();
    let mut scene = Scene::new(r#"packages = { "tree" }"#);
    scene.undo("pacman -Rns --noconfirm tree; pacman -Rns --noconfirm jq");
    let saved = format!("{}/../plan.json", scene.config);
    let (code, text) = scene.run(&["diff", "--save", &saved]);
    assert_eq!(code, Some(1), "{text}");
    assert!(pacman_ok(&["-S", "--noconfirm", "jq"]));
    let journals = journal_count();
    let (code, text) = scene.run(&["switch", "--no-snapshot", "--plan", &saved]);
    assert_eq!(code, Some(2), "{text}");
    assert!(text.contains("the local package db changed since the plan"), "{text}");
    assert!(text.contains("run diff again"), "{text}");
    assert!(installed("tree").is_none(), "nothing was installed");
    assert_eq!(journal_count(), journals, "a refused saved plan opens no journal");

    scene.manage(r#"packages = { "tree", "jq" }"#);
    let (code, text) = scene.run(&["diff", "--save", &saved]);
    assert_eq!(code, Some(1), "{text}");
    let (code, text) = scene.run(&["switch", "--no-snapshot", "--plan", &saved]);
    assert_eq!(
        code,
        Some(0),
        "a plan nothing moved under is applied without a question:\n{text}"
    );
    assert!(installed("tree").is_some());
    scene.assert_converged("saved plan");
}

#[test]
#[ignore]
fn a_unit_that_wont_start_stops_the_switch_and_the_next_switch_converges() {
    let _system = lock_system();
    let unit = "archix-switch-broken.service";
    let body = |exec: &str| {
        format!(
            "packages = {{ \"tree\" }}, units = {{ [{unit:?}] = \"[Unit]\\nDescription=archix switch test\\n[Service]\\nType=oneshot\\nRemainAfterExit=yes\\nExecStart={exec}\\n[Install]\\nWantedBy=multi-user.target\\n\" }}, services = {{ [{unit:?}] = true }}"
        )
    };
    let mut scene = Scene::new(&body("/usr/bin/false"));
    scene.undo(&format!(
        "systemctl disable --now {unit}; rm -f /etc/systemd/system/{unit}; systemctl daemon-reload; pacman -Rns --noconfirm tree"
    ));
    let (code, text) = scene.run(SWITCH);
    assert_eq!(code, Some(3), "{text}");
    let journal = latest_journal();
    for part in [
        "failed at services",
        "Nothing is undone automatically",
        "No pre snapshot was taken",
        &journal.display().to_string(),
    ] {
        assert!(text.contains(part), "{part:?} missing from:\n{text}");
    }
    let m = meta(&journal);
    assert_eq!(m.outcome, archix::journal::Outcome::Failed);
    assert_eq!(m.failed_step.as_deref(), Some("services"));
    assert!(m.alpm_window.end.is_some(), "the lock window was closed");
    assert!(installed("tree").is_some(), "the steps before the failure stay done");
    assert_pacman_usable();

    scene.manage(&body("/usr/bin/true"));
    let (code, text) = scene.run(SWITCH);
    assert_eq!(code, Some(0), "{text}");
    scene.assert_converged("after the unit was fixed");
}

#[test]
#[ignore]
fn a_file_write_error_stops_the_switch_and_the_next_switch_converges() {
    let _system = lock_system();
    let blocker = "/etc/archix-switch-blocked";
    let path = format!("{blocker}/conf");
    let mut scene = Scene::new(&format!(
        "packages = {{ \"tree\" }}, files = {{ [{path:?}] = {{ text = \"x\\n\" }} }}"
    ));
    scene.undo(&format!("rm -rf {blocker}; pacman -Rns --noconfirm tree"));
    fs::write(blocker, "a file where the directory should be\n").unwrap();
    let (code, text) = scene.run(SWITCH);
    assert_eq!(code, Some(3), "{text}");
    assert!(text.contains("failed at files"), "{text}");
    assert!(text.contains("Nothing is undone automatically"), "{text}");
    let journal = latest_journal();
    assert!(text.contains(&journal.display().to_string()), "{text}");
    assert_eq!(meta(&journal).failed_step.as_deref(), Some("files"));
    assert!(installed("tree").is_some());
    assert_pacman_usable();

    fs::remove_file(blocker).unwrap();
    let (code, text) = scene.run(SWITCH);
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(fs::read_to_string(&path).unwrap(), "x\n");
    scene.assert_converged("after the blocker went");
}

#[test]
#[ignore]
fn pacman_is_refused_by_the_lock_during_a_switch_and_a_switch_during_pacman() {
    let _system = lock_system();
    let mut scene = Scene::new(r#"packages = { "figlet" }"#);
    scene.undo("pacman -Rns --noconfirm figlet; pacman -Rns --noconfirm cowsay");
    {
        let pause = Pause::on("figlet");
        let child = spawn(&scene.config, SWITCH);
        pause.wait();
        let output = Command::new("pacman")
            .args(["-S", "--noconfirm", "cowsay"])
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "pacman ran during the switch's transaction");
        assert!(said.contains("unable to lock database"), "{said}");
        pause.release();
        let (code, text) = finish(child);
        assert_eq!(code, Some(0), "{text}");
    }
    assert!(installed("figlet").is_some() && installed("cowsay").is_none());

    scene.manage(r#"packages = { "figlet", "toilet" }"#);
    scene.undo("pacman -Rns --noconfirm toilet");
    let pause = Pause::on("cowsay");
    let mut pacman = Command::new("pacman")
        .args(["-S", "--noconfirm", "cowsay"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    pause.wait();
    let journals = journal_count();
    let (code, text) = scene.run(SWITCH);
    assert_eq!(code, Some(2), "{text}");
    assert!(text.contains("db.lck exists"), "{text}");
    assert_eq!(journal_count(), journals + 1, "the refusal is journaled");
    assert_eq!(meta(&latest_journal()).failed_step.as_deref(), Some("lock"));
    assert!(installed("toilet").is_none());
    pause.release();
    assert!(pacman.wait().unwrap().success());
    assert!(!Path::new("/var/lib/pacman/db.lck").exists());
    scene.manage(r#"packages = { "figlet", "toilet", "cowsay" }"#);
    let (code, text) = scene.run(SWITCH);
    assert_eq!(code, Some(0), "{text}");
    scene.assert_converged("after pacman finished");
}

/// A pty pair whose slave becomes the controlling terminal of the child it's given to.
fn openpty() -> (OwnedFd, OwnedFd) {
    let (mut master, mut slave) = (0, 0);
    let made = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(made, 0, "openpty: {}", std::io::Error::last_os_error());
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

#[test]
#[ignore]
fn sighup_and_a_closed_terminal_mid_commit_dont_stop_the_switch() {
    let _system = lock_system();
    let mut scene = Scene::new(r#"packages = { "sl" }"#);
    scene.undo("pacman -Rns --noconfirm sl");
    let saved = format!("{}/../plan.json", scene.config);
    let (code, text) = scene.run(&["diff", "--save", &saved]);
    assert_eq!(code, Some(1), "{text}");
    let pause = Pause::on("sl");
    let (master, slave) = openpty();
    let tty = |fd: &OwnedFd| Stdio::from(fd.try_clone().unwrap());
    let mut command = Command::new(env!("CARGO_BIN_EXE_archix"));
    command
        .args([
            "--config",
            &scene.config,
            "--host",
            HOST,
            "switch",
            "--no-snapshot",
            "--plan",
            &saved,
        ])
        .stdin(tty(&slave))
        .stdout(tty(&slave))
        .stderr(tty(&slave));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let _ = fs::remove_file("/tmp/archix-test-hup.close");
    let mut child = command.spawn().unwrap();
    drop(slave);
    let reading = master.try_clone().unwrap();
    let reader = std::thread::spawn(move || {
        let mut file = fs::File::from(reading);
        let mut seen = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            if Path::new("/tmp/archix-test-hup.close").exists() {
                return seen;
            }
            let mut poll = libc::pollfd {
                fd: file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut poll, 1, 100) } <= 0 {
                continue;
            }
            match file.read(&mut buf) {
                Ok(0) | Err(_) => return seen,
                Ok(n) => seen.extend_from_slice(&buf[..n]),
            }
        }
    });
    pause.wait();
    fs::write("/tmp/archix-test-hup.close", "").unwrap();
    let said = reader.join().unwrap();
    drop(master);
    let _ = fs::remove_file("/tmp/archix-test-hup.close");
    unsafe { libc::kill(child.id() as i32, libc::SIGHUP) };
    pause.release();
    let status = child.wait().unwrap();
    assert_eq!(
        status.code(),
        Some(0),
        "the switch survived SIGHUP and its terminal going away; it said before that:\n{}",
        String::from_utf8_lossy(&said)
    );
    assert!(installed("sl").is_some());
    assert!(pacman_ok(&["-Qkk", "sl"]), "sl's files are all there");
    assert_eq!(meta(&latest_journal()).outcome, archix::journal::Outcome::Ok);
    assert_pacman_usable();
    scene.assert_converged("after the hangup");
}

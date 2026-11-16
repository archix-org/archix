//! `home-manager build` at switch step 2 and `<store path>/activate` after the system steps, both as the user (SCOPE 7).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::exec::{Cmd, Exec, System, checked};
use super::generate::{Rendered, write_cmd};
use super::{Account, NIX_CONFIG, account, as_user, euid, linger, users};
use crate::value::Value;

/// The gcroot build leaves in the flake dir, so a GC between step 2 and activation can't take the generation.
pub const GCROOT: &str = "gcroot";

pub struct Runner {
    pub exec: Box<dyn Exec>,
    /// Where /etc/passwd is read from.
    pub root: PathBuf,
    pub self_uid: u32,
    /// Users with home.<user>.linger = true.
    pub linger: BTreeSet<String>,
}

impl Runner {
    /// A runner for the machine, with linger as `config` declares it.
    pub fn new(config: &Value) -> Runner {
        Runner {
            exec: Box::new(System),
            root: PathBuf::from("/"),
            self_uid: euid(),
            linger: users(config).into_keys().filter(|user| linger(config, user)).collect(),
        }
    }

    fn flake_ref(flake_dir: &Path) -> Result<String> {
        let dir = flake_dir.to_str().context("the flake dir isn't UTF-8")?;
        Ok(format!("path:{dir}"))
    }

    /// The commands build() runs, in order.
    pub fn build_cmds(&self, user: &str, flake_dir: &Path) -> Result<[Cmd; 2]> {
        let uid = account(&self.root, user)?.uid;
        let flake = Self::flake_ref(flake_dir)?;
        let nix_config = format!("NIX_CONFIG={NIX_CONFIG}");
        let hm = vec![
            "env".into(),
            nix_config.clone(),
            "home-manager".into(),
            "build".into(),
            "--flake".into(),
            format!("{flake}#{user}"),
            "--no-out-link".into(),
        ];
        let gcroot = flake_dir.join(GCROOT);
        let root = vec![
            "env".into(),
            nix_config,
            "nix".into(),
            "build".into(),
            "--out-link".into(),
            gcroot.to_str().context("the flake dir isn't UTF-8")?.into(),
            "--print-out-paths".into(),
            format!("{flake}#homeConfigurations.{user:?}.activationPackage"),
        ];
        Ok([
            Cmd::new(as_user(user, uid, self.self_uid, hm)),
            Cmd::new(as_user(user, uid, self.self_uid, root)),
        ])
    }

    /// Builds the user's generation from `flake_dir`, roots it there and returns its store path.
    pub fn build(&self, user: &str, flake_dir: &Path) -> Result<PathBuf> {
        let [hm, root] = self.build_cmds(user, flake_dir)?;
        checked(&*self.exec, &hm, &format!("home-manager build for {user}"))?;
        let out = checked(&*self.exec, &root, &format!("rooting {user}'s home-manager generation"))?;
        let path = out
            .stdout
            .lines()
            .map(str::trim)
            .rfind(|line| line.starts_with("/nix/store/"))
            .with_context(|| format!("nix build printed no store path for {user}'s generation"))?;
        Ok(PathBuf::from(path))
    }

    /// The command activate() runs once the user's manager is up.
    pub fn activate_cmd(&self, user: &str, store_path: &Path) -> Result<Cmd> {
        let account = account(&self.root, user)?;
        let store = store_path.to_str().context("the store path isn't UTF-8")?;
        if !store.starts_with("/nix/store/") || store.contains("/../") {
            bail!("{store} isn't a home-manager generation in /nix/store");
        }
        let run = format!("/run/user/{}", account.uid);
        let home = account.home.to_str().context("the home directory isn't UTF-8")?;
        let argv = vec![
            "env".into(),
            format!("HOME={home}"),
            format!("USER={user}"),
            format!("XDG_RUNTIME_DIR={run}"),
            format!("DBUS_SESSION_BUS_ADDRESS=unix:path={run}/bus"),
            format!("NIX_CONFIG={NIX_CONFIG}"),
            format!("{}/activate", store.trim_end_matches('/')),
        ];
        Ok(Cmd::new(as_user(user, account.uid, self.self_uid, argv)))
    }

    /// Activates the generation built at step 2, as the user, with their systemd manager running.
    pub fn activate(&self, user: &str, store_path: &Path) -> Result<()> {
        let cmd = self.activate_cmd(user, store_path)?;
        let uid = account(&self.root, user)?.uid;
        let manager = format!("user@{uid}.service");
        let running = self
            .exec
            .run(&Cmd::new(
                ["systemctl", "is-active", "--quiet", &manager]
                    .map(String::from)
                    .to_vec(),
            ))?
            .success;
        if !running {
            if !self.linger.contains(user) {
                bail!(
                    "home-manager for {user}: their systemd user manager isn't running (not logged in), and home.{user}.linger isn't declared; log {user} in, or declare home.{user}.linger = true"
                );
            }
            let start = Cmd::new(["systemctl", "start", &manager].map(String::from).to_vec());
            checked(&*self.exec, &start, &format!("starting {user}'s systemd user manager"))?;
        }
        checked(
            &*self.exec,
            &cmd,
            &format!("activating {user}'s home-manager generation"),
        )?;
        Ok(())
    }
}

/// diff's build: the rendered flake written to a scratch dir in the user's cache, built there with its own gcroot.
pub fn build_for_diff(user: &str, account: &Account, rendered: &Rendered) -> Result<PathBuf> {
    let dir = account.home.join(".cache/archix/home-manager-diff").join(user);
    let cmd = write_cmd(user, account, euid(), &dir, rendered.tar()?)?;
    checked(&System, &cmd, &format!("writing {user}'s scratch home-manager flake"))?;
    Runner {
        exec: Box::new(System),
        root: PathBuf::from("/"),
        self_uid: euid(),
        linger: BTreeSet::new(),
    }
    .build(user, &dir)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::home::hm::Output;

    struct Fake {
        log: Rc<RefCell<Vec<Vec<String>>>>,
        answer: Box<dyn Fn(&Cmd) -> Output>,
    }

    impl Exec for Fake {
        fn run(&self, cmd: &Cmd) -> Result<Output> {
            self.log.borrow_mut().push(cmd.argv.clone());
            Ok((self.answer)(cmd))
        }
    }

    fn make(
        self_uid: u32,
        linger: &[&str],
        answer: Box<dyn Fn(&Cmd) -> Output>,
    ) -> (Runner, Rc<RefCell<Vec<Vec<String>>>>, tempfile::TempDir) {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("etc")).unwrap();
        std::fs::write(root.path().join("etc/passwd"), "al:x:1000:1000::/home/al:/bin/zsh\n").unwrap();
        let log = Rc::new(RefCell::new(Vec::new()));
        let runner = Runner {
            exec: Box::new(Fake {
                log: log.clone(),
                answer,
            }),
            root: root.path().to_path_buf(),
            self_uid,
            linger: linger.iter().map(|s| s.to_string()).collect(),
        };
        (runner, log, root)
    }

    fn ok(stdout: &str) -> Output {
        Output {
            success: true,
            stdout: stdout.into(),
        }
    }

    #[test]
    fn build_runs_home_manager_as_the_user_and_roots_the_generation_in_the_flake_dir() {
        let store = "/nix/store/abc-home-manager-generation";
        let (runner, log, _root) = make(0, &[], Box::new(move |_| ok(&format!("{store}\n"))));
        let dir = Path::new("/home/al/.config/archix/home-manager/al");
        assert_eq!(runner.build("al", dir).unwrap(), Path::new(store));
        let log = log.borrow();
        assert_eq!(
            log[0],
            [
                "runuser",
                "-u",
                "al",
                "--",
                "env",
                "NIX_CONFIG=experimental-features = nix-command flakes",
                "home-manager",
                "build",
                "--flake",
                "path:/home/al/.config/archix/home-manager/al#al",
                "--no-out-link"
            ]
        );
        assert_eq!(
            log[1],
            [
                "runuser",
                "-u",
                "al",
                "--",
                "env",
                "NIX_CONFIG=experimental-features = nix-command flakes",
                "nix",
                "build",
                "--out-link",
                "/home/al/.config/archix/home-manager/al/gcroot",
                "--print-out-paths",
                "path:/home/al/.config/archix/home-manager/al#homeConfigurations.\"al\".activationPackage"
            ]
        );
    }

    #[test]
    fn a_failed_build_is_an_error_naming_the_user() {
        let (runner, _, _root) = make(0, &[], Box::new(|_| Output::default()));
        let err = runner.build("al", Path::new("/d")).unwrap_err().to_string();
        assert!(err.contains("home-manager build for al"), "{err}");
    }

    #[test]
    fn activate_runs_the_generation_as_the_user_with_their_bus() {
        let (runner, log, _root) = make(0, &[], Box::new(|_| ok("")));
        runner.activate("al", Path::new("/nix/store/abc-gen")).unwrap();
        let log = log.borrow();
        assert_eq!(log[0], ["systemctl", "is-active", "--quiet", "user@1000.service"]);
        assert_eq!(
            log[1],
            [
                "runuser",
                "-u",
                "al",
                "--",
                "env",
                "HOME=/home/al",
                "USER=al",
                "XDG_RUNTIME_DIR=/run/user/1000",
                "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus",
                "NIX_CONFIG=experimental-features = nix-command flakes",
                "/nix/store/abc-gen/activate"
            ]
        );
    }

    #[test]
    fn as_the_user_itself_no_runuser_is_needed() {
        let (runner, _, _root) = make(1000, &[], Box::new(|_| ok("")));
        let cmd = runner.activate_cmd("al", Path::new("/nix/store/abc-gen")).unwrap();
        assert_eq!(cmd.argv[0], "env");
    }

    #[test]
    fn activate_refuses_without_a_running_manager_unless_linger_is_declared() {
        let down = |cmd: &Cmd| Output {
            success: cmd.argv[1] != "is-active",
            stdout: String::new(),
        };
        let (runner, log, _root) = make(0, &[], Box::new(down));
        let err = runner
            .activate("al", Path::new("/nix/store/abc-gen"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("home.al.linger"), "{err}");
        assert_eq!(log.borrow().len(), 1, "nothing ran after the check");

        let (runner, log, _root) = make(0, &["al"], Box::new(down));
        runner.activate("al", Path::new("/nix/store/abc-gen")).unwrap();
        let log = log.borrow();
        assert_eq!(log[1], ["systemctl", "start", "user@1000.service"]);
        assert!(log[2].last().unwrap().ends_with("/activate"));
    }

    #[test]
    fn activate_refuses_a_path_outside_the_store() {
        let (runner, log, _root) = make(0, &["al"], Box::new(|_| ok("")));
        assert!(runner.activate("al", Path::new("/tmp/evil")).is_err());
        assert!(runner.activate("al", Path::new("/nix/store/../../tmp/x")).is_err());
        assert!(log.borrow().is_empty());
    }
}

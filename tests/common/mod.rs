#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard};

use archix::pacman::read_local_db;
use archix::paths::Paths;
use tempfile::TempDir;

/// The snapshot the test image is pinned to, so locking at it changes nothing but what a test declares.
pub const IMAGE_SNAPSHOT: &str = "2026-10-10";
static SYSTEM: Mutex<()> = Mutex::new(());

/// Packages tests install and remove; they never count as part of the image's own declared set.
fn test_package(name: &str) -> bool {
    matches!(name, "tree" | "lemon" | "jq") || name.starts_with("archix-test-")
}

pub fn lock_system() -> MutexGuard<'static, ()> {
    SYSTEM.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub struct Setup {
    pub dir: TempDir,
    pub paths: Paths,
}

impl Setup {
    /// A config declaring everything explicitly installed in the image plus `extra`, locked at the image's snapshot.
    pub fn new(extra: &[&str], body: &str) -> Setup {
        let paths = Paths::system();
        let mut packages: Vec<String> = read_local_db(&paths.local_db())
            .unwrap()
            .into_values()
            .filter(|pkg| pkg.explicit)
            .map(|pkg| pkg.name)
            .filter(|name| !test_package(name))
            .collect();
        packages.extend(extra.iter().map(|p| p.to_string()));
        let quoted: Vec<String> = packages.iter().map(|p| format!("{p:?}")).collect();
        let dir = TempDir::new().unwrap();
        let init = format!("return {{ packages = {{ {} }}, {body} }}", quoted.join(", "));
        fs::write(dir.path().join("init.lua"), init).unwrap();
        let setup = Setup { dir, paths };
        setup.ok(&["update", "--to", IMAGE_SNAPSHOT, "--noconfirm"]);
        setup
    }

    pub fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_archix"))
            .args(["--config", self.dir.path().to_str().unwrap(), "--host", "box"])
            .args(args)
            .output()
            .unwrap()
    }

    pub fn answering(&self, input: &str, args: &[&str]) -> String {
        let mut child = Command::new(env!("CARGO_BIN_EXE_archix"))
            .args(["--config", self.dir.path().to_str().unwrap(), "--host", "box"])
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        let output = child.wait_with_output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "archix {args:?} failed:\n{text}");
        text
    }

    pub fn lock(&self) -> archix::lock::HostLock {
        archix::lock::Lockfile::load(&self.dir.path().join("archix.lock"))
            .unwrap()
            .hosts["box"]
            .clone()
    }

    pub fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "archix {args:?} failed:\n{text}");
        text
    }

    pub fn fails(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(!output.status.success(), "archix {args:?} should have failed");
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    pub fn set_body(&self, extra: &[&str], body: &str) {
        let replacement = Setup::new(extra, body);
        fs::copy(
            replacement.dir.path().join("init.lua"),
            self.dir.path().join("init.lua"),
        )
        .unwrap();
        fs::copy(
            replacement.dir.path().join("archix.lock"),
            self.dir.path().join("archix.lock"),
        )
        .unwrap();
    }
}

pub fn installed(name: &str) -> Option<String> {
    read_local_db(&Paths::system().local_db())
        .unwrap()
        .remove(name)
        .map(|pkg| pkg.version)
}

pub fn pacman(args: &[&str]) {
    let status = Command::new("pacman").args(args).arg("--noconfirm").status().unwrap();
    assert!(status.success(), "pacman {args:?}");
}

pub fn cached_file(dir: &Path, name: &str) -> PathBuf {
    fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            let file = path.file_name().unwrap().to_string_lossy().into_owned();
            file.starts_with(&format!("{name}-")) && file.ends_with(".pkg.tar.zst")
        })
        .unwrap_or_else(|| panic!("no cached {name} package in {}", dir.display()))
}

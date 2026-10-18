#![allow(dead_code)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard};

use archix::pacman::read_local_db;
use archix::paths::Paths;
use flate2::Compression;
use flate2::write::GzEncoder;
use tempfile::TempDir;

/// The snapshot the test image is pinned to, so locking at it changes nothing but what a test declares.
pub const IMAGE_SNAPSHOT: &str = "2026-10-10";
static SYSTEM: Mutex<()> = Mutex::new(());
/// Small packages the boot tests add one at a time.
pub const UKI_TEST_PACKAGES: [&str; 10] = [
    "tree", "bc", "pv", "dos2unix", "figlet", "rlwrap", "cowsay", "sl", "toilet", "ncdu",
];

/// Packages tests install and remove; they never count as part of the image's own declared set.
fn test_package(name: &str) -> bool {
    UKI_TEST_PACKAGES.contains(&name) || matches!(name, "tree" | "lemon" | "jq") || name.starts_with("archix-test-")
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
        Setup::with_files(extra, body, &[])
    }

    /// Like `new`, with `files` (path relative to the config, content) written before the first lock.
    pub fn with_files(extra: &[&str], body: &str, files: &[(&str, &str)]) -> Setup {
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
        for (path, content) in files {
            let path = dir.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        let setup = Setup { dir, paths };
        setup.ok(&["update", "--to", IMAGE_SNAPSHOT, "--noconfirm"]);
        setup
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.run_env(&[], args)
    }

    pub fn run_env(&self, env: &[(&str, &str)], args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_archix"))
            .args(["--config", self.dir.path().to_str().unwrap(), "--host", "box"])
            .args(args)
            .envs(env.iter().copied())
            .output()
            .unwrap()
    }

    pub fn ok_env(&self, env: &[(&str, &str)], args: &[&str]) -> String {
        let output = self.run_env(env, args);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "archix {args:?} failed:\n{text}");
        text
    }

    pub fn read(&self, path: &str) -> Option<String> {
        fs::read_to_string(self.dir.path().join(path)).ok()
    }

    pub fn write(&self, path: &str, content: &str) {
        fs::write(self.dir.path().join(path), content).unwrap();
    }

    pub fn answering_raw(&self, input: &str, args: &[&str]) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_archix"))
            .args(["--config", self.dir.path().to_str().unwrap(), "--host", "box"])
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
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

/// Lines only in `before` as "- line" and only in `after` as "+ line", or "(identical)".
pub fn line_diff(before: &str, after: &str) -> String {
    let (old, new): (Vec<&str>, Vec<&str>) = (before.lines().collect(), after.lines().collect());
    let mut out: Vec<String> = old
        .iter()
        .filter(|l| !new.contains(l))
        .map(|l| format!("- {l}"))
        .collect();
    out.extend(new.iter().filter(|l| !old.contains(l)).map(|l| format!("+ {l}")));
    if out.is_empty() && before == after {
        return "(identical)".to_string();
    }
    if out.is_empty() {
        out.push("(same lines, different bytes)".to_string());
    }
    out.join("\n")
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

/// A package published to the local test AUR.
pub struct TestPkg<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub depends: &'a [&'a str],
    pub makedepends: &'a [&'a str],
    pub provides: &'a [&'a str],
    pub build: &'a str,
    pub package: &'a str,
}

impl<'a> TestPkg<'a> {
    pub fn new(name: &'a str, version: &'a str) -> Self {
        TestPkg {
            name,
            version,
            depends: &[],
            makedepends: &[],
            provides: &[],
            build: "true",
            package: "",
        }
    }
}

/// An AUR served from local bare git repos plus a metadata dump, so AUR tests don't depend on aur.archlinux.org.
pub struct LocalAur {
    dir: TempDir,
    published: Vec<(String, String, Vec<String>)>,
}

impl LocalAur {
    pub fn new() -> LocalAur {
        LocalAur {
            dir: TempDir::new().unwrap(),
            published: Vec::new(),
        }
    }

    pub fn url(&self) -> String {
        format!("file://{}", self.dir.path().display())
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(["-c", "user.name=test", "-c", "user.email=test@example.com", "-C"])
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    pub fn publish(&mut self, pkg: TestPkg) {
        let bare = self.dir.path().join(format!("{}.git", pkg.name));
        let work = self.dir.path().join("work").join(pkg.name);
        if !bare.exists() {
            fs::create_dir_all(&bare).unwrap();
            LocalAur::git(&bare, &["init", "--quiet", "--bare", "-b", "master"]);
            fs::create_dir_all(&work).unwrap();
            LocalAur::git(&work, &["init", "--quiet", "-b", "master"]);
            LocalAur::git(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
        }
        let list = |items: &[&str]| items.iter().map(|i| format!("'{i}'")).collect::<Vec<_>>().join(" ");
        let pkgbuild = format!(
            "pkgname={name}\npkgver={version}\npkgrel=1\npkgdesc='archix test package {name}'\narch=('any')\n\
             license=('MIT')\ndepends=({depends})\nmakedepends=({makedepends})\nprovides=({provides})\n\
             build() {{\n  {build}\n}}\n\
             package() {{\n  install -d \"$pkgdir/usr/share/{name}\"\n  echo {version} > \"$pkgdir/usr/share/{name}/version\"\n  {package}\n}}\n",
            name = pkg.name,
            version = pkg.version,
            depends = list(pkg.depends),
            makedepends = list(pkg.makedepends),
            provides = list(pkg.provides),
            build = pkg.build,
            package = pkg.package,
        );
        let mut srcinfo = format!(
            "pkgbase = {name}\n\tpkgdesc = archix test package {name}\n\tpkgver = {version}\n\tpkgrel = 1\n\tarch = any\n\tlicense = MIT\n",
            name = pkg.name,
            version = pkg.version
        );
        for dep in pkg.makedepends {
            srcinfo.push_str(&format!("\tmakedepends = {dep}\n"));
        }
        for dep in pkg.depends {
            srcinfo.push_str(&format!("\tdepends = {dep}\n"));
        }
        for provided in pkg.provides {
            srcinfo.push_str(&format!("\tprovides = {provided}\n"));
        }
        srcinfo.push_str(&format!("\npkgname = {}\n", pkg.name));
        fs::write(work.join("PKGBUILD"), pkgbuild).unwrap();
        fs::write(work.join(".SRCINFO"), srcinfo).unwrap();
        LocalAur::git(&work, &["add", "."]);
        LocalAur::git(
            &work,
            &["commit", "--quiet", "-m", &format!("{} {}", pkg.name, pkg.version)],
        );
        LocalAur::git(&work, &["push", "--quiet", "origin", "master"]);

        self.published.retain(|(name, _, _)| name != pkg.name);
        self.published.push((
            pkg.name.to_string(),
            format!("{}-1", pkg.version),
            pkg.provides.iter().map(|p| p.to_string()).collect(),
        ));
        let entries: Vec<serde_json::Value> = self
            .published
            .iter()
            .map(|(name, version, provides)| {
                serde_json::json!({ "Name": name, "PackageBase": name, "Version": version, "Provides": provides, "Popularity": 0.0 })
            })
            .collect();
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(serde_json::to_string(&entries).unwrap().as_bytes())
            .unwrap();
        fs::write(
            self.dir.path().join("packages-meta-ext-v1.json.gz"),
            gz.finish().unwrap(),
        )
        .unwrap();
    }
}

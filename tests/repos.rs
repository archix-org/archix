//! Installs from a local third-party repo; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use archix::lock::AUR_REPO;
use common::*;
use tempfile::TempDir;

/// A pacman repository made with repo-add and served over file://, standing in for CachyOS or chaotic-aur.
struct LocalRepo {
    dir: TempDir,
    name: &'static str,
}

fn run(cmd: &mut Command) {
    let status = cmd.status().unwrap();
    assert!(status.success(), "{cmd:?}");
}

impl LocalRepo {
    fn new(name: &'static str) -> LocalRepo {
        if !Command::new("id")
            .arg("archix-build")
            .output()
            .unwrap()
            .status
            .success()
        {
            run(Command::new("useradd").args(["--system", "--no-create-home", "archix-build"]));
        }
        LocalRepo {
            dir: TempDir::new().unwrap(),
            name,
        }
    }

    fn server(&self) -> String {
        format!("file://{}", self.dir.path().display())
    }

    fn database(&self) -> PathBuf {
        self.dir.path().join(format!("{}.db.tar.gz", self.name))
    }

    /// Builds a tiny package with makepkg and adds it to the repo.
    fn publish(&self, name: &str, version: &str) {
        let work = TempDir::new().unwrap();
        let pkgbuild = format!(
            "pkgname={name}\npkgver={version}\npkgrel=1\narch=('any')\nlicense=('MIT')\n\
             package() {{\n  install -d \"$pkgdir/usr/share/archix-third/{name}\"\n  echo {version} > \"$pkgdir/usr/share/archix-third/{name}/version\"\n}}\n"
        );
        fs::write(work.path().join("PKGBUILD"), pkgbuild).unwrap();
        run(Command::new("chown").arg("-R").arg("archix-build:").arg(work.path()));
        run(Command::new("runuser")
            .args(["-u", "archix-build", "--", "env"])
            .arg(format!("HOME={}", work.path().display()))
            .args(["makepkg", "--noconfirm"])
            .current_dir(work.path()));
        let file = format!("{name}-{version}-1-any.pkg.tar.zst");
        fs::copy(work.path().join(&file), self.dir.path().join(&file)).unwrap();
        run(Command::new("repo-add")
            .arg("--quiet")
            .arg(self.database())
            .arg(self.dir.path().join(&file)));
    }

    /// Takes a package out of the repo entirely, files included.
    fn withdraw(&self, name: &str) {
        run(Command::new("repo-remove")
            .arg("--quiet")
            .arg(self.database())
            .arg(name));
        for entry in fs::read_dir(self.dir.path()).unwrap().flatten() {
            if entry.file_name().to_string_lossy().starts_with(&format!("{name}-")) {
                fs::remove_file(entry.path()).unwrap();
            }
        }
    }
}

/// Uninstalls a package and deletes every cached copy, so the next switch has to find it somewhere.
fn forget(name: &str) {
    pacman(&["-R", name]);
    for dir in [Path::new("/var/cache/archix/pkg"), Path::new("/var/cache/pacman/pkg")] {
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            if entry.file_name().to_string_lossy().starts_with(&format!("{name}-")) {
                fs::remove_file(entry.path()).unwrap();
            }
        }
    }
}

#[test]
#[ignore]
fn third_party_packages_install_and_fall_back_down_the_chain() {
    let _system = lock_system();
    let repo = LocalRepo::new("archix-third");
    repo.publish("archix-test-third", "1.0");
    repo.publish("tree", "9.9");
    let mut aur = LocalAur::new();
    aur.publish(TestPkg::new("archix-test-unrelated", "1.0"));
    let body = format!(
        "custom_repos = {{ {{ name = \"archix-third\", servers = {{ {:?} }}, sig_level = \"Optional TrustAll\", before_official = true }} }}, aur = {{ url = {:?} }}",
        repo.server(),
        aur.url()
    );

    let setup = Setup::new(&["archix-test-third", "tree"], &body);
    let lock = setup.lock();
    assert_eq!(lock.packages["archix-test-third"].repo, "archix-third");
    assert_eq!(
        (
            lock.packages["tree"].repo.as_str(),
            lock.packages["tree"].version.as_str()
        ),
        ("archix-third", "9.9-1"),
        "a repo listed before the official ones wins"
    );
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(installed("archix-test-third").as_deref(), Some("1.0-1"));
    assert_eq!(installed("tree").as_deref(), Some("9.9-1"));
    let conf = fs::read_to_string("/etc/pacman.conf").unwrap();
    assert!(
        conf.find("[archix-third]").unwrap() < conf.find("[core]").unwrap(),
        "{conf}"
    );

    repo.withdraw("archix-test-third");
    repo.publish("archix-test-third", "1.1");
    forget("archix-test-third");
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(
        out.contains("archix-test-third 1.0-1 is gone from archix-third"),
        "{out}"
    );
    assert_eq!(
        installed("archix-test-third").as_deref(),
        Some("1.1-1"),
        "first: the repo's current version"
    );

    repo.withdraw("archix-test-third");
    aur.publish(TestPkg::new("archix-test-third", "2.0"));
    forget("archix-test-third");
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(
        installed("archix-test-third").as_deref(),
        Some("2.0-1"),
        "then: the AUR"
    );
    assert_eq!(setup.lock().packages["archix-test-third"].repo, AUR_REPO);

    repo.withdraw("tree");
    forget("tree");
    setup.ok(&["switch", "--noconfirm"]);
    let tree = installed("tree").unwrap();
    assert_ne!(tree, "9.9-1", "last: the official repos");
    assert_ne!(setup.lock().packages["tree"].repo, "archix-third");

    let strict = body.replace("aur = {", "third_party_fallback = \"fail\", aur = {");
    setup.set_body(&["archix-test-third", "tree"], &strict);
    repo.publish("archix-test-third", "3.0");
    setup.ok(&["update", "--to", IMAGE_SNAPSHOT, "--noconfirm"]);
    repo.withdraw("archix-test-third");
    assert!(
        setup
            .fails(&["switch", "--noconfirm"])
            .contains("third_party_fallback = \"fail\"")
    );

    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert!(installed("archix-test-third").is_none());
    assert!(
        !fs::read_to_string("/etc/pacman.conf")
            .unwrap()
            .contains("[archix-third]"),
        "the repo's block goes too"
    );
}

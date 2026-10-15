//! Builds AUR packages in a clean chroot; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

mod common;

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;

use archix::generations::Store;
use archix::lock::AUR_REPO;
use common::*;
use flate2::Compression;
use flate2::write::GzEncoder;
use tempfile::TempDir;

/// A package published to the local test AUR.
struct TestPkg<'a> {
    name: &'a str,
    version: &'a str,
    depends: &'a [&'a str],
    makedepends: &'a [&'a str],
    build: &'a str,
    package: &'a str,
}

impl<'a> TestPkg<'a> {
    fn new(name: &'a str, version: &'a str) -> Self {
        TestPkg {
            name,
            version,
            depends: &[],
            makedepends: &[],
            build: "true",
            package: "",
        }
    }
}

/// An AUR served from local bare git repos plus a metadata dump, so AUR tests don't depend on aur.archlinux.org.
struct LocalAur {
    dir: TempDir,
    published: Vec<(String, String)>,
}

impl LocalAur {
    fn new() -> LocalAur {
        LocalAur {
            dir: TempDir::new().unwrap(),
            published: Vec::new(),
        }
    }

    fn url(&self) -> String {
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

    fn publish(&mut self, pkg: TestPkg) {
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
             license=('MIT')\ndepends=({depends})\nmakedepends=({makedepends})\n\
             build() {{\n  {build}\n}}\n\
             package() {{\n  install -d \"$pkgdir/usr/share/{name}\"\n  echo {version} > \"$pkgdir/usr/share/{name}/version\"\n  {package}\n}}\n",
            name = pkg.name,
            version = pkg.version,
            depends = list(pkg.depends),
            makedepends = list(pkg.makedepends),
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
        srcinfo.push_str(&format!("\npkgname = {}\n", pkg.name));
        fs::write(work.join("PKGBUILD"), pkgbuild).unwrap();
        fs::write(work.join(".SRCINFO"), srcinfo).unwrap();
        LocalAur::git(&work, &["add", "."]);
        LocalAur::git(
            &work,
            &["commit", "--quiet", "-m", &format!("{} {}", pkg.name, pkg.version)],
        );
        LocalAur::git(&work, &["push", "--quiet", "origin", "master"]);

        self.published.retain(|(name, _)| name != pkg.name);
        self.published
            .push((pkg.name.to_string(), format!("{}-1", pkg.version)));
        let entries: Vec<serde_json::Value> = self
            .published
            .iter()
            .map(|(name, version)| {
                serde_json::json!({ "Name": name, "PackageBase": name, "Version": version, "Popularity": 0.0 })
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

fn read(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_default().trim().to_string()
}

#[test]
#[ignore]
fn aur_packages_are_locked_built_reviewed_and_fall_back() {
    let _system = lock_system();
    let mut aur = LocalAur::new();
    aur.publish(TestPkg::new("archix-test-lib", "1.0"));
    aur.publish(TestPkg {
        package: "install -Dm755 /dev/stdin \"$pkgdir/usr/bin/archix-test-builddep\" <<'EOS'\n#!/bin/sh\necho built-with-builddep\nEOS",
        ..TestPkg::new("archix-test-builddep", "1.0")
    });
    aur.publish(TestPkg {
        depends: &["archix-test-lib", "jq"],
        makedepends: &["archix-test-builddep"],
        build: "archix-test-builddep > \"$srcdir/marker\"",
        package: "install -Dm644 \"$srcdir/marker\" \"$pkgdir/usr/share/archix-test-app/marker\"",
        ..TestPkg::new("archix-test-app", "2.0")
    });
    let body = format!("aur = {{ url = {:?} }}", aur.url());

    let setup = Setup::new(&["archix-test-app"], &body);
    let lock = setup.lock();
    let app = &lock.packages["archix-test-app"];
    assert_eq!(
        (app.repo.as_str(), app.version.as_str(), app.explicit),
        (AUR_REPO, "2.0-1", true)
    );
    assert!(app.commit.as_ref().is_some_and(|c| c.len() == 40));
    let lib = &lock.packages["archix-test-lib"];
    assert!(
        !lib.explicit && !lib.build_only,
        "a runtime AUR dependency is installed as a dependency"
    );
    assert!(
        lock.packages["archix-test-builddep"].build_only,
        "a make-only AUR dependency is only built"
    );
    assert_ne!(
        lock.packages["jq"].repo, AUR_REPO,
        "repo dependencies of AUR packages come from the repos"
    );

    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(installed("archix-test-app").as_deref(), Some("2.0-1"));
    assert_eq!(installed("archix-test-lib").as_deref(), Some("1.0-1"));
    assert!(installed("jq").is_some());
    assert!(
        installed("archix-test-builddep").is_none(),
        "build-only packages stay out of the system"
    );
    assert_eq!(
        read("/usr/share/archix-test-app/marker"),
        "built-with-builddep",
        "the chroot had the build tool"
    );
    let store = Store::new(&setup.paths);
    let generation = store.get(store.current().unwrap().unwrap()).unwrap();
    assert_eq!(
        generation.packages["archix-test-app"].sha256.len(),
        64,
        "the generation records the built file"
    );
    assert!(
        setup.ok(&["switch", "--noconfirm"]).contains("nothing to do"),
        "built packages are reused"
    );

    aur.publish(TestPkg::new("archix-test-lib", "1.1"));
    let declined = setup.run(&["update", "--to", IMAGE_SNAPSHOT]);
    assert!(
        !declined.status.success(),
        "with no answer, a changed build recipe is not trusted"
    );
    assert!(String::from_utf8_lossy(&declined.stdout).contains("archix-test-lib: build recipe changed"));
    assert_eq!(setup.lock().packages["archix-test-lib"].version, "1.0-1");
    let out = setup.answering("y\n", &["update", "--to", IMAGE_SNAPSHOT]);
    assert!(
        out.contains("echo 1.1") || out.contains("pkgver=1.1"),
        "the review shows the diff:\n{out}"
    );
    assert_eq!(setup.lock().packages["archix-test-lib"].version, "1.1-1");

    aur.publish(TestPkg {
        build: "false",
        ..TestPkg::new("archix-test-lib", "1.2")
    });
    setup.ok(&["update", "--to", IMAGE_SNAPSHOT, "--noconfirm"]);
    aur.publish(TestPkg::new("archix-test-lib", "1.3"));
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(out.contains("archix-test-lib 1.2-1 can't be built"), "{out}");
    assert_eq!(
        installed("archix-test-lib").as_deref(),
        Some("1.3-1"),
        "--noconfirm takes the nearest newer version"
    );
    assert_eq!(
        setup.lock().packages["archix-test-lib"].version,
        "1.3-1",
        "the lock follows"
    );

    aur.publish(TestPkg {
        build: "false",
        ..TestPkg::new("archix-test-lib", "1.4")
    });
    setup.ok(&["update", "--to", IMAGE_SNAPSHOT, "--noconfirm"]);
    setup.ok(&["switch", "--noconfirm", "--accept-nearest", "older"]);
    assert_eq!(
        installed("archix-test-lib").as_deref(),
        Some("1.3-1"),
        "the nearest older version that differs"
    );

    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert!(installed("archix-test-app").is_none() && installed("archix-test-lib").is_none());
}

#[test]
#[ignore]
fn real_aur_packages_resolve() {
    let _system = lock_system();
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("init.lua"),
        r#"return { packages = { "base", "yay-bin" } }"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_archix"))
        .args([
            "--config",
            dir.path().to_str().unwrap(),
            "--host",
            "box",
            "update",
            "--to",
            IMAGE_SNAPSHOT,
        ])
        .arg("--noconfirm")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let lock = archix::lock::Lockfile::load(&dir.path().join("archix.lock")).unwrap();
    let yay = &lock.hosts["box"].packages["yay-bin"];
    assert_eq!(yay.repo, AUR_REPO);
    assert_eq!(yay.base.as_deref(), Some("yay-bin"));
}

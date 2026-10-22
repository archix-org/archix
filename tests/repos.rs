//! Installs from a local third-party repo; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

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
        self.publish_with(name, version, "");
    }

    /// Like `publish`, with more shell for its package().
    fn publish_with(&self, name: &str, version: &str, package: &str) {
        self.publish_full(name, version, package, "", None);
    }

    /// Like `publish_with`, plus an install script, and signed with the key in `signer` when given.
    fn publish_full(&self, name: &str, version: &str, package: &str, install: &str, signer: Option<&Path>) {
        let work = TempDir::new().unwrap();
        let mut pkgbuild = format!(
            "pkgname={name}\npkgver={version}\npkgrel=1\narch=('any')\nlicense=('MIT')\n\
             package() {{\n  install -d \"$pkgdir/usr/share/archix-third/{name}\"\n  echo {version} > \"$pkgdir/usr/share/archix-third/{name}/version\"\n  {package}\n}}\n"
        );
        if !install.is_empty() {
            pkgbuild.push_str(&format!("install={name}.install\n"));
            fs::write(work.path().join(format!("{name}.install")), install).unwrap();
        }
        fs::write(work.path().join("PKGBUILD"), pkgbuild).unwrap();
        run(Command::new("chown").arg("-R").arg("archix-build:").arg(work.path()));
        run(Command::new("runuser")
            .args(["-u", "archix-build", "--", "env"])
            .arg(format!("HOME={}", work.path().display()))
            .args(["makepkg", "--noconfirm"])
            .current_dir(work.path()));
        let file = format!("{name}-{version}-1-any.pkg.tar.zst");
        fs::copy(work.path().join(&file), self.dir.path().join(&file)).unwrap();
        if let Some(signer) = signer {
            run(Command::new("gpg")
                .arg("--homedir")
                .arg(signer)
                .args(["--batch", "--detach-sign", "--no-armor", "--output"])
                .arg(self.dir.path().join(format!("{file}.sig")))
                .arg(self.dir.path().join(&file)));
        }
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

/// Uninstalls a package from the current deployment and deletes every cached copy, so the next switch has to find it somewhere.
fn forget(setup: &Setup, name: &str) {
    setup.machine.reboot(None);
    setup.machine.pacman(&["-R", name]);
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
fn a_package_signed_by_a_key_an_incoming_keyring_adds_installs_in_the_same_switch() {
    let _system = lock_system();
    // Arch's container image ships pacman's keyring without the master key populating signs with; installed systems have one.
    run(Command::new("pacman-key").arg("--init"));
    let signer = TempDir::new().unwrap();
    let gpg = |args: &[&str]| {
        Command::new("gpg")
            .arg("--homedir")
            .arg(signer.path())
            .args(["--batch", "--pinentry-mode", "loopback", "--passphrase", ""])
            .args(args)
            .output()
            .unwrap()
    };
    let made = gpg(&[
        "--quick-gen-key",
        "archix test packager <packager@archix.test>",
        "ed25519",
        "sign",
        "never",
    ]);
    assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
    let listing = String::from_utf8_lossy(&gpg(&["--with-colons", "--list-keys"]).stdout).into_owned();
    let fingerprint = listing
        .lines()
        .find(|line| line.starts_with("fpr:"))
        .and_then(|line| line.split(':').nth(9))
        .unwrap()
        .to_string();
    // Files the unprivileged makepkg user packages, as archlinux-keyring ships its packagers' keys.
    let keys = TempDir::new().unwrap();
    fs::set_permissions(keys.path(), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    fs::write(keys.path().join("archix-test.gpg"), gpg(&["--export"]).stdout).unwrap();
    fs::write(keys.path().join("archix-test-trusted"), format!("{fingerprint}:4:\n")).unwrap();
    let keyrings = LocalRepo::new("archix-keyrings");
    keyrings.publish_full(
        "archix-test-keyring",
        "1.0",
        &format!(
            "install -Dm644 -t \"$pkgdir/usr/share/pacman/keyrings\" {0}/archix-test.gpg {0}/archix-test-trusted",
            keys.path().display()
        ),
        "post_install() {\n  pacman-key --populate archix-test\n}\n",
        None,
    );
    let signed = LocalRepo::new("archix-signed");
    signed.publish_full("archix-test-signed", "1.0", "", "", Some(signer.path()));
    let body = format!(
        "custom_repos = {{ {{ name = \"archix-keyrings\", servers = {{ {:?} }}, sig_level = \"Optional TrustAll\" }}, {{ name = \"archix-signed\", servers = {{ {:?} }} }} }}",
        keyrings.server(),
        signed.server()
    );

    let setup = Setup::new(&["archix-test-keyring", "archix-test-signed"], &body);
    let out = setup.run(&["switch", "--noconfirm"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    let _ = Command::new("pacman-key").args(["--delete", &fingerprint]).output();
    assert!(out.status.success(), "{text}");
    assert_eq!(installed("archix-test-signed").as_deref(), Some("1.0-1"));
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
}

#[test]
#[ignore]
fn a_package_signed_by_an_expired_key_is_refused_at_prepare() {
    let _system = lock_system();
    run(Command::new("pacman-key").arg("--init"));
    let signer = TempDir::new().unwrap();
    let gpg = |args: &[&str]| {
        Command::new("gpg")
            .arg("--homedir")
            .arg(signer.path())
            .args(["--batch", "--pinentry-mode", "loopback", "--passphrase", ""])
            .args(args)
            .output()
            .unwrap()
    };
    let packager = "archix expired packager <expired@archix.test>";
    let made = gpg(&["--quick-gen-key", packager, "ed25519", "sign", "seconds=40"]);
    let expired = Instant::now() + Duration::from_secs(42);
    assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
    let listing = String::from_utf8_lossy(&gpg(&["--with-colons", "--list-keys"]).stdout).into_owned();
    let fingerprint = listing
        .lines()
        .find(|line| line.starts_with("fpr:"))
        .and_then(|line| line.split(':').nth(9))
        .unwrap()
        .to_string();
    let key = signer.path().join("packager.gpg");
    fs::write(&key, gpg(&["--export"]).stdout).unwrap();
    run(Command::new("pacman-key").arg("--add").arg(&key));
    run(Command::new("pacman-key").args(["--lsign-key", &fingerprint]));
    let repo = LocalRepo::new("archix-signed");
    repo.publish_full("archix-test-expired", "1.0", "", "", Some(signer.path()));
    let body = format!(
        "custom_repos = {{ {{ name = \"archix-signed\", servers = {{ {:?} }} }} }}",
        repo.server()
    );
    let setup = Setup::new(&["archix-test-expired"], &body);
    std::thread::sleep(expired.saturating_duration_since(Instant::now()));

    let out = setup.run(&["switch", "--noconfirm"]);
    let _ = Command::new("pacman-key").args(["--delete", &fingerprint]).output();
    let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{text}");
    assert!(
        text.contains("pacman refuses these packages, so nothing was changed")
            && text.contains(&format!(
                "archix-test-expired-1.0-1-any.pkg.tar.zst is signed by {packager}, whose key expired on"
            )),
        "prepare names the package and the expired key: {text}"
    );
    assert!(!text.contains("in the staging root"), "{text}");
    assert!(setup.machine.deployments().is_empty(), "{text}");
    assert_eq!(setup.machine.current(), None);
    assert!(installed("archix-test-expired").is_none());
}

#[test]
#[ignore]
fn plain_pacman_finds_the_database_of_a_repo_a_switch_declared() {
    let _system = lock_system();
    let repo = LocalRepo::new("archix-third");
    repo.publish("archix-test-third", "1.0");
    let body = format!(
        "custom_repos = {{ {{ name = \"archix-third\", servers = {{ {:?} }}, sig_level = \"Optional TrustAll\" }} }}",
        repo.server()
    );
    let _ = fs::remove_file("/var/lib/pacman/sync/archix-third.db");
    let setup = Setup::new(&[], &body);
    setup.lay_out();
    let query = Command::new("pacman")
        .arg("--config")
        .arg(system_path("/etc/pacman.conf"))
        .arg("--dbpath")
        .arg(setup.machine.root.join("var/lib/pacman"))
        .args(["-Q", "pacman"])
        .output()
        .unwrap();
    let warnings = String::from_utf8_lossy(&query.stderr).into_owned();
    assert!(
        fs::read_to_string(system_path("/etc/pacman.conf"))
            .unwrap()
            .contains("[archix-third]"),
        "the deployment's pacman.conf has the repo"
    );
    assert!(
        !warnings.contains("database file for 'archix-third' does not exist"),
        "{warnings}"
    );
}

#[test]
#[ignore]
fn a_rollback_that_rebuilds_never_takes_a_keyring_back() {
    let _system = lock_system();
    let repo = LocalRepo::new("archix-third");
    let keyring = "install -Dm644 /dev/null \"$pkgdir/usr/share/pacman/keyrings/archix-test.gpg\"";
    repo.publish_with("archix-test-keyring", "1.0", keyring);
    let body = format!(
        "custom_repos = {{ {{ name = \"archix-third\", servers = {{ {:?} }}, sig_level = \"Optional TrustAll\" }} }}, deployments = {{ keep = 1 }}",
        repo.server()
    );
    let setup = Setup::new(&["archix-test-keyring"], &body);
    setup.lay_out();
    repo.publish_with("archix-test-keyring", "2.0", keyring);
    setup.ok(&["update", "--to", IMAGE_SNAPSHOT, "--noconfirm"]);
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(installed("archix-test-keyring").as_deref(), Some("2.0-1"));
    setup.machine.reboot(None);
    setup.ok(&["gc"]);

    let out = setup.ok(&["rollback", "--noconfirm"]);
    assert!(
        out.contains("keeping archix-test-keyring 2.0-1 rather than going back to 1.0-1: an older keyring trusts keys revoked since"),
        "{out}"
    );
    assert_eq!(installed("archix-test-keyring").as_deref(), Some("2.0-1"));
}

#[test]
#[ignore]
fn a_rollback_finds_a_third_party_package_archix_adopted_after_the_repo_drops_it() {
    let _system = lock_system();
    let repo = LocalRepo::new("archix-third");
    repo.publish("archix-test-adopted", "1.0");
    let body = format!(
        "custom_repos = {{ {{ name = \"archix-third\", servers = {{ {:?} }}, sig_level = \"Optional TrustAll\" }} }}",
        repo.server()
    );
    let setup = Setup::new(&[], &body);
    setup.lay_out();
    // Installed with plain pacman, the way `archix init` finds packages on a machine.
    setup.machine.pacman(&["-Sy", "archix-test-adopted"]);
    setup.set_body(&["archix-test-adopted"], &body);
    setup.ok(&["switch", "--noconfirm"]);
    setup.set_body(&[], &body);
    setup.ok(&["switch", "--noconfirm"]);
    assert!(installed("archix-test-adopted").is_none());

    repo.withdraw("archix-test-adopted");
    for entry in fs::read_dir("/var/cache/pacman/pkg").unwrap().flatten() {
        if entry.file_name().to_string_lossy().starts_with("archix-test-adopted-") {
            fs::remove_file(entry.path()).unwrap();
        }
    }
    setup.ok(&["rollback", "--noconfirm"]);
    assert_eq!(installed("archix-test-adopted").as_deref(), Some("1.0-1"));
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
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
        "custom_repos = {{ {{ name = \"archix-third\", servers = {{ {:?} }}, sig_level = \"Optional TrustAll\", before_official = true }} }}, third_party_fallback = \"fallback\", aur = {{ url = {:?} }}",
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
    let conf = fs::read_to_string(system_path("/etc/pacman.conf")).unwrap();
    assert!(
        conf.find("[archix-third]").unwrap() < conf.find("[core]").unwrap(),
        "{conf}"
    );

    repo.withdraw("archix-test-third");
    repo.publish("archix-test-third", "1.1");
    forget(&setup, "archix-test-third");
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
    forget(&setup, "archix-test-third");
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(
        installed("archix-test-third").as_deref(),
        Some("2.0-1"),
        "then: the AUR"
    );
    assert_eq!(setup.lock().packages["archix-test-third"].repo, AUR_REPO);

    repo.withdraw("tree");
    forget(&setup, "tree");
    setup.ok(&["switch", "--noconfirm"]);
    let tree = installed("tree").unwrap();
    assert_ne!(tree, "9.9-1", "last: the official repos");
    assert_ne!(setup.lock().packages["tree"].repo, "archix-third");

    let strict = body.replace("\"fallback\"", "\"fail\"");
    setup.set_body(&["archix-test-third", "tree"], &strict);
    repo.publish("archix-test-third", "3.0");
    setup.ok(&["update", "--to", IMAGE_SNAPSHOT, "--noconfirm"]);
    repo.withdraw("archix-test-third");
    assert!(
        setup
            .fails(&["switch", "--noconfirm"])
            .contains("third_party_fallback = \"fail\"")
    );

    let ask = body.replace("third_party_fallback = \"fallback\", ", "");
    setup.set_body(&["archix-test-third", "tree"], &ask);
    repo.publish("archix-test-third", "4.0");
    setup.ok(&["update", "--to", IMAGE_SNAPSHOT, "--noconfirm"]);
    repo.withdraw("archix-test-third");
    let refused = setup.fails(&["switch", "--noconfirm"]);
    assert!(refused.contains("Run without --noconfirm"), "{refused}");
    assert_eq!(
        setup.lock().packages["archix-test-third"].version,
        "4.0-1",
        "with nobody to ask, a vanished package is never swapped and the lock never rewritten"
    );

    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert!(installed("archix-test-third").is_none());
    assert!(
        !fs::read_to_string(system_path("/etc/pacman.conf"))
            .unwrap()
            .contains("[archix-third]"),
        "the repo's block goes too"
    );
}

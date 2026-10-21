//! Installs Flatpak apps from a local repository on the live system, the container's own; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

mod common;

use std::process::Command;

use common::*;

const REPO: &str = "/tmp/archix-flatpak";

fn sh(script: &str) -> String {
    let output = Command::new("bash").arg("-ec").arg(script).output().unwrap();
    assert!(
        output.status.success(),
        "{script}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A runtime and an app that prints `hello <version>`, exported to a local repo; returns the app's new commit.
fn publish_hello(version: u32) -> String {
    if version == 1 {
        sh(&format!(
            "rm -rf {REPO} && mkdir -p {REPO}/rt/usr/bin && cd {REPO} && ln -s usr rt/files
             printf '#!/bin/sh\\n' > rt/usr/bin/placeholder
             printf '[Runtime]\\nname=org.archix.Platform\\nruntime=org.archix.Platform/x86_64/1\\nsdk=org.archix.Platform/x86_64/1\\n' > rt/metadata
             flatpak build-export --runtime --no-update-summary repo rt 1 >/dev/null
             flatpak build-update-repo repo >/dev/null
             flatpak remote-add --system --no-gpg-verify --if-not-exists build file://{REPO}/repo
             flatpak install --system -y --noninteractive build runtime/org.archix.Platform/x86_64/1 >/dev/null"
        ));
    }
    sh(&format!(
        "cd {REPO} && rm -rf app && flatpak build-init app org.archix.Hello org.archix.Platform org.archix.Platform 1 >/dev/null
         mkdir -p app/files/bin && printf '#!/bin/sh\\necho hello {version}\\n' > app/files/bin/hello && chmod +x app/files/bin/hello
         flatpak build-finish app --command=hello >/dev/null
         flatpak build-export --no-update-summary repo app stable >/dev/null
         flatpak build-update-repo repo >/dev/null"
    ));
    let info = sh("LC_ALL=C flatpak remote-info --system build org.archix.Hello");
    info.lines()
        .find_map(|line| line.trim().strip_prefix("Commit:"))
        .unwrap()
        .trim()
        .to_string()
}

fn installed_commit() -> Option<String> {
    let output = Command::new("flatpak")
        .args(["info", "--system", "--show-commit", "org.archix.Hello"])
        .output()
        .unwrap();
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Hides the container's flatpak, which plays the running system's, until dropped.
struct NoFlatpak;

impl NoFlatpak {
    fn new() -> NoFlatpak {
        sh("mount --bind /dev/null /usr/bin/flatpak");
        NoFlatpak
    }
}

impl Drop for NoFlatpak {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg("/usr/bin/flatpak").status();
    }
}

/// Marks the container's flatpak a dependency, so a machine copied from it doesn't declare it, and explicit again when dropped, as the other tests expect.
struct FlatpakAsDependency;

impl FlatpakAsDependency {
    fn new() -> FlatpakAsDependency {
        pacman(&["-S", "--needed", "flatpak"]);
        pacman(&["-D", "--asdeps", "flatpak"]);
        FlatpakAsDependency
    }
}

impl Drop for FlatpakAsDependency {
    fn drop(&mut self) {
        let _ = Command::new("pacman").args(["-D", "--asexplicit", "flatpak"]).status();
    }
}

#[test]
#[ignore]
fn apps_declared_with_flatpak_itself_install_at_the_new_deployments_first_boot() {
    let _system = lock_system();
    let _dependency = FlatpakAsDependency::new();
    let first = publish_hello(1);
    let setup = Setup::new(&[], "");
    setup.lay_out();
    assert!(
        !setup
            .machine
            .deployment(setup.machine.current().unwrap())
            .join("usr/bin/flatpak")
            .exists(),
        "the running deployment has no flatpak"
    );

    let hidden = NoFlatpak::new();
    let body = format!(
        "flatpak = {{ apps = {{ \"local:org.archix.Hello\" }}, remotes = {{ [\"local\"] = {{ url = \"file://{REPO}/repo\", gpg_verify = false }} }} }}"
    );
    setup.set_body(&[], &body);
    let output = setup.run(&["switch", "--noconfirm"]);
    let out = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{out}");
    let number = setup.machine.current().unwrap();
    assert!(
        out.contains("flatpak    install org.archix.Hello at the new deployment's first boot")
            && out.contains(&format!("deployment {number}'s first boot installs them")),
        "the switch says when the app comes: {out}"
    );
    assert!(setup.machine.deployment(number).join("usr/bin/flatpak").exists());
    drop(hidden);
    assert_eq!(installed_commit(), None, "nothing is installed before the boot");

    let unit = setup
        .machine
        .deployment(number)
        .join("etc/systemd/system/archix-flatpak.service");
    let unit = std::fs::read_to_string(&unit).unwrap_or_default();
    assert!(
        unit.contains("After=network-online.target") && unit.contains("deploy-flatpak"),
        "{unit}"
    );
    setup.machine.reboot(Some(&format!("archix-{number}.conf")));
    setup.ok(&["deploy-activate", "--root", setup.machine.root.to_str().unwrap()]);
    let out = setup.ok(&["deploy-flatpak"]);
    assert!(out.contains("org.archix.Hello"), "{out}");
    assert_eq!(installed_commit().as_deref(), Some(first.as_str()));
    assert_eq!(
        setup.lock().flatpak["org.archix.Hello"].commit,
        first,
        "the boot locks the commit it installed"
    );
    assert!(
        setup
            .ok(&["diff"])
            .contains("the system matches the lock and the config"),
        "the generation records it too"
    );
    publish_hello(2);
    setup.ok(&["deploy-flatpak"]);
    assert_eq!(
        installed_commit().as_deref(),
        Some(first.as_str()),
        "it runs once per deployment"
    );

    sh(&format!(
        "flatpak uninstall --system -y --noninteractive --all >/dev/null || true
         flatpak remote-delete --system --force local || true
         flatpak remote-delete --system --force build || true
         rm -rf {REPO}"
    ));
}

#[test]
#[ignore]
fn flatpak_apps_are_locked_to_commits_and_roll_back() {
    let _system = lock_system();
    // Flatpak apps live outside deployments, so flatpak runs from the live system, here the container's.
    pacman(&["-S", "--needed", "flatpak"]);
    let setup = Setup::new(&["flatpak"], "");
    setup.ok(&["switch", "--noconfirm"]);
    let first = publish_hello(1);

    let body = format!(
        "flatpak = {{ apps = {{ \"local:org.archix.Hello\" }}, remotes = {{ [\"local\"] = {{ url = \"file://{REPO}/repo\", gpg_verify = false }} }} }}"
    );
    setup.set_body(&[], &body);
    assert!(
        setup.lock().flatpak.is_empty(),
        "lock doesn't add remotes, so the commit waits for switch"
    );
    assert!(setup.ok(&["diff"]).contains("flatpak    install org.archix.Hello"));
    let out = setup.ok(&["switch", "--noconfirm"]);
    assert!(out.contains("flatpak    install org.archix.Hello"), "{out}");
    assert_eq!(
        setup.lock().flatpak["org.archix.Hello"].commit,
        first,
        "switch records the commit"
    );
    assert_eq!(installed_commit().as_deref(), Some(first.as_str()));

    let second = publish_hello(2);
    assert_ne!(first, second);
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(
        installed_commit().as_deref(),
        Some(first.as_str()),
        "switch keeps the locked commit, not the newest"
    );
    assert!(
        setup
            .ok(&["diff"])
            .contains("the system matches the lock and the config")
    );

    setup.ok(&["update", "--to", IMAGE_SNAPSHOT, "--noconfirm"]);
    assert_eq!(
        setup.lock().flatpak["org.archix.Hello"].commit,
        second,
        "update moves to the newest commit"
    );
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(installed_commit().as_deref(), Some(second.as_str()));

    setup.ok(&["rollback", "--noconfirm"]);
    assert_eq!(
        installed_commit().as_deref(),
        Some(first.as_str()),
        "rollback pins the old commit again"
    );

    let captured = tempfile::tempdir().unwrap();
    let config = captured.path().join("cfg");
    let out = Command::new(env!("CARGO_BIN_EXE_archix"))
        .args([
            "--config",
            config.to_str().unwrap(),
            "--host",
            "fpinit",
            "init",
            "--defaults",
        ])
        .output()
        .unwrap();
    let out = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.contains("the system matches the lock and the config"), "{out}");
    let lock = archix::lock::Lockfile::load(&config.join("archix.lock")).unwrap().hosts["fpinit"].clone();
    assert_eq!(
        lock.flatpak["org.archix.Hello"].commit, first,
        "init locks the installed commit"
    );
    let host = std::fs::read_to_string(config.join("lua/hosts/fpinit/init.lua")).unwrap();
    assert!(host.contains("\"local:org.archix.Hello\""), "{host}");

    setup.ok(&["switch", "--noconfirm"]);
    setup.set_body(&["flatpak"], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(installed_commit(), None, "apps leaving the config are uninstalled");

    sh(&format!(
        "flatpak uninstall --system -y --noninteractive --all >/dev/null || true
         flatpak remote-delete --system --force local || true
         flatpak remote-delete --system --force build || true
         rm -rf {REPO}"
    ));
}

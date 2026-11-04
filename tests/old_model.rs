//! A system archix v0.2.0 laid out as deployments, which this archix can't run on: every command refuses with what to do instead and changes nothing, the deployments' own entry points included.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::Path;
use std::process::{Command, Output};

const REFUSAL: &str =
    "this system uses archix's deployments model, which this version no longer supports; it was left unchanged";

fn archix(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_archix"))
        .args(args)
        .env("ARCHIX_TEST_ROOT", root)
        .env("ARCHIX_DETACHED", "1")
        .output()
        .unwrap()
}

fn deployments_root() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for tree in ["usr", "etc", "pacman", "boot"] {
        fs::create_dir_all(root.join("archix/deploy/1").join(tree)).unwrap();
    }
    fs::write(root.join("archix/deploy/1/etc/hostname"), "old\n").unwrap();
    symlink("deploy/1", root.join("archix/current")).unwrap();
    fs::create_dir_all(root.join("etc/archix")).unwrap();
    fs::write(root.join("etc/archix/init.lua"), "return {}\n").unwrap();
    fs::create_dir_all(root.join("var/lib/archix")).unwrap();
    dir
}

fn tree(root: &Path) -> BTreeMap<String, (u32, u64, Vec<u8>)> {
    let mut seen = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = fs::symlink_metadata(&path).unwrap();
            let body = if meta.is_file() {
                fs::read(&path).unwrap()
            } else if meta.file_type().is_symlink() {
                fs::read_link(&path).unwrap().into_os_string().into_encoded_bytes()
            } else {
                stack.push(path.clone());
                Vec::new()
            };
            let name = path.strip_prefix(root).unwrap().display().to_string();
            seen.insert(name, (meta.mode(), meta.mtime() as u64, body));
        }
    }
    seen
}

#[test]
fn every_command_on_a_deployments_system_refuses_and_changes_nothing() {
    let dir = deployments_root();
    let root = dir.path();
    let before = tree(root);
    let commands: &[&[&str]] = &[
        &["switch", "--noconfirm"],
        &["status"],
        &["init"],
        &["rollback", "1", "--noconfirm"],
        &["generations"],
        &["boot-check"],
        &["pacman-hook", "pre"],
        &["deploy-mount", "/new_root"],
        &["deploy-activate"],
        &["deploy-integrate", "--exec", "/usr/bin/archix"],
        &["pacman-guard"],
        &["hook-record"],
        &[],
    ];
    for args in commands {
        let out = archix(root, args);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "archix {args:?} should refuse\n{stderr}");
        assert!(
            stderr.contains(REFUSAL),
            "archix {args:?} should say why it refuses\n{stderr}"
        );
        assert!(
            stderr.contains("/archix/"),
            "archix {args:?} should name what it found\n{stderr}"
        );
        assert!(!stderr.contains("unrecognized subcommand"), "archix {args:?}\n{stderr}");
    }
    assert_eq!(tree(root), before, "a refused command changed the root");
}

#[test]
fn the_entry_points_only_deployments_call_refuse_even_where_the_root_isnt_visible() {
    let empty = tempfile::tempdir().unwrap();
    for args in [
        &["deploy-mount", "/sysroot"][..],
        &["deploy-integrate", "--exec", "/usr/bin/archix"],
        &["pacman-guard"],
    ] {
        let out = archix(empty.path(), args);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "archix {args:?}\n{stderr}");
        assert!(stderr.contains(REFUSAL), "archix {args:?}\n{stderr}");
    }
}

#[test]
fn help_and_version_still_answer_on_a_deployments_system() {
    let dir = deployments_root();
    for args in [&["--version"][..], &["--help"]] {
        let out = archix(dir.path(), args);
        assert!(
            out.status.success(),
            "archix {args:?}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn a_system_without_deployments_isnt_mistaken_for_one() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".archix/snapshots")).unwrap();
    fs::create_dir_all(dir.path().join("archix")).unwrap();
    let out = archix(dir.path(), &["generations"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains(REFUSAL), "{stderr}");
}

#[test]
fn deploy_integrate_in_a_v0_2_0_switchs_staging_view_fails_its_mkinitcpio_before_the_commit() {
    let dir = tempfile::tempdir().unwrap();
    let staging = dir.path().join("view");
    fs::create_dir_all(staging.join("etc")).unwrap();
    let mountinfo = dir.path().join("mountinfo");
    fs::write(
        &mountinfo,
        "812 811 0:31 /@/archix/deploy/.staging-7/.view / rw,relatime - btrfs /dev/vda2 rw\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_archix"))
        .args(["deploy-integrate", "--exec", "/usr/bin/archix"])
        .env("ARCHIX_TEST_ROOT", &staging)
        .env("ARCHIX_TEST_MOUNTINFO", &mountinfo)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "v0.2.0 writes its own integration and commits when deploy-integrate fails\n{stderr}"
    );
    assert!(stderr.contains(REFUSAL), "{stderr}");
    assert_eq!(
        fs::read_to_string(staging.join("etc/mkinitcpio.conf.d/archix-refuse-deployments.conf")).unwrap(),
        "HOOKS+=(archix-refuse-deployments)\n"
    );
    let hook = staging.join("etc/initcpio/install/archix-refuse-deployments");
    let Some(kernel) = fs::read_dir("/usr/lib/modules")
        .ok()
        .and_then(|mut dirs| dirs.next())
        .map(|d| d.unwrap().file_name())
    else {
        return;
    };
    let install = dir.path().join("install");
    fs::create_dir_all(&install).unwrap();
    fs::copy(&hook, install.join("archix-refuse-deployments")).unwrap();
    let conf = dir.path().join("mkinitcpio.conf");
    fs::write(&conf, "HOOKS=(base archix-refuse-deployments)\n").unwrap();
    let image = dir.path().join("initramfs.img");
    let out = Command::new("mkinitcpio")
        .arg("-k")
        .arg(&kernel)
        .arg("-c")
        .arg(&conf)
        .env(
            "MKINITCPIO_INSTALL",
            format!("{}:/usr/lib/initcpio/install", install.display()),
        )
        .arg("-g")
        .arg(&image)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "mkinitcpio should fail\n{stderr}");
    assert!(stderr.lines().last().unwrap_or_default().contains(REFUSAL), "{stderr}");
    assert!(!image.exists());
}

#[test]
fn deploy_integrate_outside_a_staging_view_writes_no_hook() {
    let dir = tempfile::tempdir().unwrap();
    let mountinfo = dir.path().join("mountinfo");
    fs::write(&mountinfo, "29 1 0:31 /@ / rw - btrfs /dev/vda2 rw\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_archix"))
        .args(["deploy-integrate", "--exec", "/usr/bin/archix"])
        .env("ARCHIX_TEST_ROOT", dir.path())
        .env("ARCHIX_TEST_MOUNTINFO", &mountinfo)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains(REFUSAL));
    assert!(!dir.path().join("etc").exists());
}

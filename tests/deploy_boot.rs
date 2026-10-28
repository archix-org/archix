//! The deployment initramfs hook and first-boot activation; run only inside the privileged Arch container via `tests/arch/run.sh`.

mod common;

use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use archix::system::deploy_boot;
use common::lock_system;

fn sh(program: &str, args: &[&str]) -> String {
    let out = Command::new(program).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn archix(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_archix")).args(args).output().unwrap()
}

fn text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr)
}

fn xattr_names(path: &Path) -> Vec<String> {
    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let mut buf = vec![0u8; 4096];
    let size = unsafe { libc::llistxattr(path.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    assert!(size >= 0);
    buf.truncate(size as usize);
    buf.split(|b| *b == 0)
        .filter(|name| !name.is_empty())
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect()
}

/// systemd-boot's LoaderEntrySelected for `entry`: attributes, then UTF-16LE with a NUL.
fn efivar_bytes(entry: &str) -> Vec<u8> {
    let mut bytes = vec![7, 0, 0, 0];
    for unit in entry.encode_utf16().chain([0]) {
        bytes.extend(unit.to_le_bytes());
    }
    bytes
}

/// The activation unit and the whole initramfs hook, under `root`.
fn all_boot_files(root: &Path) -> Vec<(PathBuf, String)> {
    let mut files = vec![deploy_boot::activate_file(root, Path::new("/usr/bin/archix"))];
    files.extend(deploy_boot::hook_files(root));
    files
}

/// A root filesystem as the initramfs sees it, on its own tmpfs: deployments 1 and 2, an uncommitted staging tree, current at 1, an entry record for each and no /var/lib/dkms or /var/usrlocal yet.
struct Sysroot {
    dir: tempfile::TempDir,
}

impl Sysroot {
    fn new() -> Sysroot {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sysroot");
        fs::create_dir(&root).unwrap();
        sh("mount", &["-t", "tmpfs", "sysroot", root.to_str().unwrap()]);
        for tree in ["usr", "etc", "opt", "boot", "var/lib/pacman/local"] {
            fs::create_dir_all(root.join(tree)).unwrap();
        }
        for name in ["1", "2", ".staging-3"] {
            for (tree, _) in deploy_boot::TREES {
                let dir = root.join("archix/deploy").join(name).join(tree);
                fs::create_dir_all(&dir).unwrap();
                fs::write(dir.join("which"), name).unwrap();
            }
            fs::create_dir(root.join("archix/deploy").join(name).join("usr/local")).unwrap();
        }
        std::os::unix::fs::symlink("deploy/1", root.join("archix/current")).unwrap();
        let records = root.join("var/lib/archix/boot/entries");
        fs::create_dir_all(&records).unwrap();
        for n in 1..=3 {
            fs::write(records.join(format!("archix-{n}.conf")), "title archix\n").unwrap();
        }
        sh("mount", &["-o", "remount,ro", root.to_str().unwrap()]);
        Sysroot { dir }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().join("sysroot")
    }

    fn efivar(&self, entry: &str) -> PathBuf {
        let path = self.dir.path().join("efivar");
        fs::write(&path, efivar_bytes(entry)).unwrap();
        path
    }

    fn mount(&self, efivar: &Path) -> Output {
        let run = self.dir.path().join("run");
        archix(&[
            "deploy-mount",
            "--efivar",
            efivar.to_str().unwrap(),
            "--run-dir",
            run.to_str().unwrap(),
            self.root().to_str().unwrap(),
        ])
    }

    fn which(&self, at: &str) -> String {
        fs::read_to_string(self.root().join(at).join("which")).unwrap_or_default()
    }
}

impl Drop for Sysroot {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg("-R").arg(self.root()).status();
    }
}

#[test]
#[ignore]
fn the_initramfs_mounts_the_selected_committed_deployment_with_usr_opt_and_boot_read_only() {
    let sysroot = Sysroot::new();
    let out = sysroot.mount(&sysroot.efivar("archix-2+1-2.conf"));
    assert!(out.status.success(), "{}", text(&out));
    for (_, at) in deploy_boot::TREES {
        assert_eq!(sysroot.which(at), "2", "{at} from deployment 2\n{}", text(&out));
    }
    assert_eq!(
        fs::read_to_string(sysroot.dir.path().join("run/deployment")).unwrap(),
        "2\n"
    );
    assert!(
        text(&out).contains("booting deployment 2 (the boot entry "),
        "{}",
        text(&out)
    );
    // As systemd-remount-fs does after switch_root, for a command line with `ro`.
    sh("mount", &["-o", "remount,rw", sysroot.root().to_str().unwrap()]);
    let previous = sysroot.root().join("archix/deploy/1/usr");
    let which = previous.join("which");
    let before = fs::symlink_metadata(&which).unwrap();
    for (program, args) in [
        ("chmod", vec!["-R", "700"]),
        ("chown", vec!["-R", "nobody"]),
        ("setfacl", vec!["-R", "-m", "u:nobody:rwx"]),
        ("setcap", vec!["cap_net_raw+ep"]),
        ("touch", vec![]),
    ] {
        let target = if program == "setcap" || program == "touch" {
            &which
        } else {
            &previous
        };
        let out = Command::new(program).args(&args).arg(target).output().unwrap();
        assert!(
            !out.status.success() && text(&out).contains("Read-only file system"),
            "{program} through /archix: {}",
            text(&out)
        );
    }
    let after = fs::symlink_metadata(&which).unwrap();
    assert_eq!(
        (after.mode(), after.uid(), after.ctime(), after.ctime_nsec()),
        (before.mode(), before.uid(), before.ctime(), before.ctime_nsec())
    );
    assert!(xattr_names(&which).is_empty(), "{:?}", xattr_names(&which));
    assert_eq!(fs::read_to_string(&which).unwrap(), "1");
    for at in ["etc", "var/lib/pacman/local", "var/lib/dkms"] {
        fs::write(sysroot.root().join(at).join("written"), "x").unwrap_or_else(|e| panic!("{at} is writable: {e}"));
    }
    for at in ["usr", "opt", "boot"] {
        let err = fs::write(sysroot.root().join(at).join("written"), "x").expect_err(at);
        assert_eq!(err.raw_os_error(), Some(libc::EROFS), "{at}: {err}");
        assert!(!sysroot.root().join("archix/deploy/2").join(at).join("written").exists());
    }
}

#[test]
#[ignore]
fn the_initramfs_binds_var_usrlocal_over_usr_local_but_not_for_the_fallback_uki_or_an_older_deployment() {
    let sysroot = Sysroot::new();
    let root = sysroot.root();
    let out = sysroot.mount(&sysroot.efivar("archix-2+2-1.conf"));
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        fs::read_to_string(sysroot.dir.path().join("run/usr-local")).unwrap(),
        "mounted\n"
    );
    sh("mount", &["-o", "remount,rw", root.to_str().unwrap()]);
    fs::write(root.join("usr/local/written"), "x").unwrap_or_else(|e| panic!("/usr/local is writable: {e}"));
    assert!(
        root.join("var/usrlocal/written").exists(),
        "/usr/local is /var/usrlocal"
    );
    assert!(!root.join("archix/deploy/2/usr/local/written").exists());
    let err = fs::write(root.join("usr/written"), "x").unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EROFS), "/usr stays read-only: {err}");
    let _ = Command::new("umount").arg("-R").arg(&root).status();

    for (entry, why) in [
        ("archix-1.conf", "older than the current 2"),
        ("archix-fallback-2.efi", "fallback UKI archix-fallback-2.efi"),
    ] {
        let sysroot = Sysroot::new();
        let root = sysroot.root();
        sh("mount", &["-o", "remount,rw", root.to_str().unwrap()]);
        fs::remove_file(root.join("archix/current")).unwrap();
        std::os::unix::fs::symlink("deploy/2", root.join("archix/current")).unwrap();
        fs::write(
            root.join("var/lib/archix/boot/entries/archix-fallback-2.efi"),
            "uki /EFI/Linux/archix-fallback-2.efi\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("var/usrlocal")).unwrap();
        fs::write(root.join("var/usrlocal/broken.service"), "x").unwrap();
        sh("mount", &["-o", "remount,ro", root.to_str().unwrap()]);
        let out = sysroot.mount(&sysroot.efivar(entry));
        assert!(out.status.success(), "{entry}: {}", text(&out));
        assert!(
            text(&out).contains("leaving /usr/local unmounted"),
            "{entry}: {}",
            text(&out)
        );
        let record = fs::read_to_string(sysroot.dir.path().join("run/usr-local")).unwrap();
        assert!(
            record.starts_with("skipped ") && record.contains(why),
            "{entry}: {record}"
        );
        assert_eq!(
            fs::read_dir(root.join("usr/local")).unwrap().count(),
            0,
            "{entry}: the deployment's own empty /usr/local"
        );
        let err = fs::write(root.join("usr/local/written"), "x").unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EROFS), "{entry}: {err}");
    }
}

#[test]
#[ignore]
fn the_initramfs_keeps_a_held_usr_local_out_and_records_a_released_try_on_a_read_only_root() {
    let hold = |root: &Path| root.join("var/lib/archix/boot/usr-local-hold");
    let unit = "var/usrlocal/lib/systemd/system/broken.service";
    for (state, has_unit, bound, after) in [
        ("held", true, false, "held"),
        ("held", false, true, "trying"),
        ("trial", true, true, "trying"),
        ("trying", false, false, "trying"),
        ("failed", false, false, "failed"),
    ] {
        let sysroot = Sysroot::new();
        let root = sysroot.root();
        sh("mount", &["-o", "remount,rw", root.to_str().unwrap()]);
        fs::write(hold(&root), format!("{state} a boot left it out\n")).unwrap();
        fs::create_dir_all(root.join("var/usrlocal")).unwrap();
        if has_unit {
            fs::create_dir_all(root.join(unit).parent().unwrap()).unwrap();
            fs::write(root.join(unit), "x").unwrap();
        }
        sh("mount", &["-o", "remount,ro", root.to_str().unwrap()]);
        let out = sysroot.mount(&sysroot.efivar("archix-1.conf"));
        assert!(out.status.success(), "{state}: {}", text(&out));
        let record = fs::read_to_string(sysroot.dir.path().join("run/usr-local")).unwrap();
        assert_eq!(record == "mounted\n", bound, "{state}, unit {has_unit}: {record}");
        assert!(bound || record.starts_with("held "), "{state}: {record}");
        let now = fs::read_to_string(hold(&root)).unwrap();
        assert!(
            now.starts_with(&format!("{after} ")) && now.contains("a boot left it out"),
            "{state}, unit {has_unit}: the hold is {now}"
        );
    }
}

#[test]
#[ignore]
fn the_initramfs_never_mounts_a_staging_tree_and_falls_back_to_current() {
    for entry in [Some("archix-3.conf"), Some("archix-9.conf"), None] {
        let sysroot = Sysroot::new();
        let efivar = match entry {
            Some(entry) => sysroot.efivar(entry),
            None => sysroot.dir.path().join("no-efivar"),
        };
        let out = sysroot.mount(&efivar);
        assert!(out.status.success(), "{entry:?}: {}", text(&out));
        for (_, at) in deploy_boot::TREES {
            assert_eq!(sysroot.which(at), "1", "{entry:?}: {at}\n{}", text(&out));
        }
        assert_eq!(
            fs::read_to_string(sysroot.dir.path().join("run/deployment")).unwrap(),
            "1\n"
        );
    }
}

#[test]
#[ignore]
fn the_initramfs_fails_on_a_root_without_deployments() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("usr")).unwrap();
    let out = archix(&[
        "deploy-mount",
        "--efivar",
        "/nonexistent",
        "--run-dir",
        dir.path().join("run").to_str().unwrap(),
        dir.path().to_str().unwrap(),
    ]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("/archix doesn't exist"), "{}", text(&out));
    assert!(!dir.path().join("run").exists());
}

/// A live /var, a staging overlay over it whose upper layer becomes deployment 2's var-upper, and what the system did after the commit.
fn staged_root() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let var = root.join("var");
    for sub in ["lib/app", "log", "spool", "lib/dkms"] {
        fs::create_dir_all(var.join(sub)).unwrap();
    }
    fs::write(var.join("lib/app/data"), "user state").unwrap();
    fs::write(
        var.join("lib/app/removed-by-staging"),
        "still needed by the old deployment",
    )
    .unwrap();
    fs::write(var.join("log/pacman.log"), "before staging\n").unwrap();
    fs::create_dir_all(root.join("etc")).unwrap();
    fs::write(root.join("etc/passwd"), "root:x:0:0::/root:/bin/bash\n").unwrap();
    fs::write(root.join("etc/group"), "root:x:0:root\ncups:x:209:\n").unwrap();

    let (upper, work, merged) = (
        dir.path().join("upper"),
        dir.path().join("work"),
        dir.path().join("merged"),
    );
    for d in [&upper, &work, &merged] {
        fs::create_dir(d).unwrap();
    }
    let options = format!(
        "lowerdir={},upperdir={},workdir={},metacopy=off,redirect_dir=off,index=off,xino=off",
        var.display(),
        upper.display(),
        work.display()
    );
    sh(
        "mount",
        &["-t", "overlay", "overlay", "-o", &options, merged.to_str().unwrap()],
    );
    let m = |path: &str| merged.join(path).to_str().unwrap().to_string();
    fs::write(m("lib/app/seed"), "from the package").unwrap();
    sh("chown", &["0:209", &m("lib/app/seed")]);
    sh("chmod", &["640", &m("lib/app/seed")]);
    fs::write(m("lib/app/helper"), "#!/bin/sh\n").unwrap();
    sh("setcap", &["cap_net_raw+ep", &m("lib/app/helper")]);
    fs::create_dir_all(m("spool/cups/tmp")).unwrap();
    sh("chown", &["-R", "0:209", &m("spool/cups")]);
    sh("chmod", &["1770", &m("spool/cups/tmp")]);
    fs::remove_file(m("lib/app/removed-by-staging")).unwrap();
    sh(
        "sh",
        &["-c", &format!("echo 'logged in staging' >> {}", m("log/pacman.log"))],
    );
    fs::create_dir_all(m("lib/dkms/nvidia")).unwrap();
    fs::write(m("lib/app/data"), "staging's copy").unwrap();
    for never in [
        "lib/archix/staged",
        "lib/systemd/random-seed",
        "lib/systemd/timers/stamp-x.timer",
        "log/journal/0123/system.journal",
    ] {
        fs::create_dir_all(Path::new(&m(never)).parent().unwrap()).unwrap();
        fs::write(m(never), "staging's").unwrap();
    }
    sh("umount", &[merged.to_str().unwrap()]);

    let deployment = root.join("archix/deploy/2");
    fs::create_dir_all(&deployment).unwrap();
    fs::rename(&upper, deployment.join("var-upper")).unwrap();
    let package = deployment.join("pacman/app-1-1");
    fs::create_dir_all(&package).unwrap();
    fs::write(
        package.join("files"),
        "%FILES%\nvar/\nvar/lib/\nvar/lib/app/\nvar/lib/app/helper\nvar/lib/app/seed\nvar/spool/\nvar/spool/cups/\nvar/spool/cups/tmp/\n\n",
    )
    .unwrap();
    let mtree = "#mtree\n/set type=file uid=0 gid=0 mode=644\n./var/spool/cups time=1.0 mode=710 gid=209 type=dir\n\
                 ./var/spool/cups/tmp time=1.0 mode=1770 gid=209 type=dir\n./var/lib/app/seed time=1.0 mode=640 gid=209 size=16\n";
    let mut gz = flate2::write::GzEncoder::new(
        fs::File::create(package.join("mtree")).unwrap(),
        flate2::Compression::default(),
    );
    std::io::Write::write_all(&mut gz, mtree.as_bytes()).unwrap();
    gz.finish().unwrap();
    fs::write(var.join("lib/app/seed"), "edited live after the commit").unwrap();
    sh(
        "sh",
        &[
            "-c",
            &format!(
                "echo 'logged after the commit' >> {}",
                var.join("log/pacman.log").display()
            ),
        ],
    );
    fs::create_dir_all(root.join("run/archix")).unwrap();
    fs::write(root.join("run/archix/deployment"), "2\n").unwrap();
    dir
}

#[test]
#[ignore]
fn the_first_boot_applies_a_real_overlay_upper_layer_once() {
    let dir = staged_root();
    let root = dir.path().join("root");
    let var = root.join("var");
    let upper = root.join("archix/deploy/2/var-upper/lib/app/removed-by-staging");
    assert_eq!(
        fs::symlink_metadata(&upper).map(|m| m.rdev()).ok(),
        Some(0),
        "the overlay recorded the deletion as a whiteout"
    );
    let out = archix(&["deploy-activate", "--root", root.to_str().unwrap()]);
    assert!(out.status.success(), "{}", text(&out));

    let meta = |path: &str| fs::symlink_metadata(var.join(path)).unwrap();
    assert_eq!(
        fs::read_to_string(var.join("lib/app/seed")).unwrap(),
        "from the package"
    );
    assert_eq!(
        (meta("lib/app/seed").mode() & 0o7777, meta("lib/app/seed").gid()),
        (0o640, 209)
    );
    assert_eq!(
        (meta("spool/cups/tmp").mode() & 0o7777, meta("spool/cups/tmp").gid()),
        (0o1770, 209)
    );
    assert!(
        sh("getcap", &[var.join("lib/app/helper").to_str().unwrap()]).contains("cap_net_raw=ep"),
        "file capabilities survive"
    );
    assert_eq!(
        fs::read_to_string(var.join("lib/app/data")).unwrap(),
        "user state",
        "both wrote it and no package owns it, so the live copy wins"
    );
    for never in [
        "lib/archix/staged",
        "lib/systemd/random-seed",
        "lib/systemd/timers/stamp-x.timer",
        "log/journal/0123/system.journal",
    ] {
        assert!(!var.join(never).exists(), "{never}");
    }
    assert_eq!(
        fs::read_to_string(var.join("lib/app/removed-by-staging"))
            .ok()
            .as_deref(),
        Some("still needed by the old deployment"),
        "a deletion never reaches state under /var"
    );
    let log = fs::read_to_string(var.join("log/pacman.log")).unwrap();
    assert!(
        log.contains("logged after the commit") && !log.contains("logged in staging"),
        "{log}"
    );
    assert!(
        !var.join("lib/dkms/nvidia").exists(),
        "DKMS state comes from the deployment"
    );
    for path in ["spool/cups", "spool/cups/tmp", "lib/app"] {
        let names = xattr_names(&var.join(path));
        assert!(
            !names.iter().any(|n| n.starts_with("trusted.overlay.")),
            "{path}: {names:?}"
        );
    }
    let markers = || -> Vec<String> {
        fs::read_dir(var.join("lib/archix/activated"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    };
    let first = markers();
    assert!(first.len() == 1 && first[0].starts_with("2-"), "{first:?}");
    assert_eq!(
        fs::read_to_string(var.join("lib/archix/booted-deployment")).unwrap(),
        format!("{}\n", first[0])
    );

    fs::write(var.join("lib/app/seed"), "changed since").unwrap();
    let again = archix(&["deploy-activate", "--root", root.to_str().unwrap()]);
    assert!(again.status.success(), "{}", text(&again));
    assert_eq!(
        fs::read_to_string(var.join("lib/app/seed")).unwrap(),
        "changed since",
        "only a deployment's first boot applies its upper layer"
    );

    let deploy = root.join("archix/deploy");
    fs::rename(deploy.join("2"), deploy.join("2.old")).unwrap();
    fs::create_dir(deploy.join("2")).unwrap();
    for tree in ["var-upper", "pacman"] {
        fs::rename(deploy.join("2.old").join(tree), deploy.join("2").join(tree)).unwrap();
    }
    let rebuilt = archix(&["deploy-activate", "--root", root.to_str().unwrap()]);
    assert!(rebuilt.status.success(), "{}", text(&rebuilt));
    assert_eq!(
        fs::read_to_string(var.join("lib/app/seed")).unwrap(),
        "from the package",
        "a rebuilt deployment 2 activates again"
    );
    assert_eq!(markers().len(), 2);
}

#[test]
#[ignore]
fn two_accounts_sharing_an_id_stop_activation_before_anything_is_applied() {
    let dir = staged_root();
    let root = dir.path().join("root");
    fs::write(
        root.join("etc/passwd"),
        "root:x:0:0::/root:/bin/bash\ncups:x:209:209::/:/usr/bin/nologin\nlive:x:209:209::/:/usr/bin/nologin\n",
    )
    .unwrap();
    let out = archix(&["deploy-activate", "--root", root.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(text(&out).contains("UID 209 is shared by cups, live"), "{}", text(&out));
    assert_eq!(
        fs::read_to_string(root.join("var/lib/app/seed")).unwrap(),
        "edited live after the commit"
    );
    assert!(!root.join("var/lib/archix/activated").exists());
}

fn activate(root: &Path, env: &[(&str, &str)]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_archix"))
        .args(["deploy-activate", "--root", root.to_str().unwrap()])
        .envs(env.iter().copied())
        .output()
        .unwrap()
}

#[test]
#[ignore]
fn a_backup_listed_var_file_changed_live_is_kept_and_the_packages_version_written_as_pacnew() {
    let dir = staged_root();
    let root = dir.path().join("root");
    let var = root.join("var");
    let deployment = root.join("archix/deploy/2");
    let md5 = |text: &str| archix::capture::md5_bytes(text.as_bytes());
    fs::write(
        deployment.join("pacman/app-1-1/desc"),
        "%NAME%\napp\n\n%VERSION%\n1-1\n\n",
    )
    .unwrap();
    fs::write(
        deployment.join("pacman/app-1-1/files"),
        format!(
            "%FILES%\nvar/\nvar/lib/\nvar/lib/app/\nvar/lib/app/helper\nvar/lib/app/seed\nvar/lib/app/pristine\nvar/spool/\n\
             var/spool/cups/\nvar/spool/cups/tmp/\n\n%BACKUP%\nvar/lib/app/seed\t{}\nvar/lib/app/pristine\t{}\n\n",
            md5("from the package"),
            md5("shipped pristine")
        ),
    )
    .unwrap();
    fs::write(deployment.join("var-upper/lib/app/pristine"), "shipped pristine").unwrap();
    fs::write(var.join("lib/app/pristine"), "original pristine").unwrap();
    fs::write(
        deployment.join("var-backup"),
        format!(
            "lib/app/seed\t{}\nlib/app/pristine\t{}\n",
            md5("original seed"),
            md5("original pristine")
        ),
    )
    .unwrap();
    let out = activate(&root, &[]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        fs::read_to_string(var.join("lib/app/seed")).unwrap(),
        "edited live after the commit",
        "an edit made here wins, as pacman keeps it"
    );
    assert_eq!(
        fs::read_to_string(var.join("lib/app/seed.pacnew")).unwrap(),
        "from the package"
    );
    let pacnew = fs::metadata(var.join("lib/app/seed.pacnew")).unwrap();
    assert_eq!((pacnew.mode() & 0o7777, pacnew.gid()), (0o640, 209));
    assert_eq!(
        fs::read_to_string(var.join("lib/app/pristine")).unwrap(),
        "shipped pristine",
        "an unchanged one is replaced"
    );
    assert!(!var.join("lib/app/pristine.pacnew").exists());
    assert!(
        text(&out).contains("/var/lib/app/seed was changed on this machine"),
        "{}",
        text(&out)
    );
}

/// Every entry under `var` but archix's own state, with type, mode, owner, content and link target.
fn var_state(var: &Path) -> Vec<String> {
    let out = Command::new("sh")
        .arg("-c")
        .arg("find . -path ./lib/archix -prune -o -printf '%p %y %m %U %G %s %l\\n' | sort; find . -path ./lib/archix -prune -o -type f -exec md5sum {} + | sort")
        .current_dir(var)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
#[ignore]
fn a_power_cut_at_any_step_of_the_var_apply_is_finished_by_the_next_boot() {
    let reference = staged_root();
    let out = activate(&reference.path().join("root"), &[]);
    assert!(out.status.success(), "{}", text(&out));
    let want = var_state(&reference.path().join("root/var"));
    let mut cut = 0;
    for step in 1.. {
        let dir = staged_root();
        let root = dir.path().join("root");
        let point = format!("activate-step-{step}");
        let first = activate(&root, &[("ARCHIX_TEST_CRASH", &point)]);
        if first.status.success() {
            break;
        }
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&first.status),
            Some(libc::SIGKILL),
            "{point}: {}",
            text(&first)
        );
        cut += 1;
        assert!(!root.join("var/lib/archix/activated").exists(), "{point}: marked done");
        assert!(
            root.join("var/lib/archix/activating").is_dir(),
            "{point}: no progress marker"
        );
        let again = activate(&root, &[]);
        assert!(again.status.success(), "{point}: {}", text(&again));
        assert!(
            text(&again).contains("cut off at an earlier boot"),
            "{point}: {}",
            text(&again)
        );
        assert_eq!(var_state(&root.join("var")), want, "after a cut at {point} and a boot");
        assert_eq!(fs::read_dir(root.join("var/lib/archix/activated")).unwrap().count(), 1);
        assert_eq!(fs::read_dir(root.join("var/lib/archix/activating")).unwrap().count(), 0);
    }
    assert!(cut >= 6, "only {cut} steps were cut");
}

#[test]
#[ignore]
fn a_power_cut_after_the_activation_marker_leaves_no_progress_marker_behind() {
    let reference = staged_root();
    let out = activate(&reference.path().join("root"), &[]);
    assert!(out.status.success(), "{}", text(&out));
    let want = var_state(&reference.path().join("root/var"));
    let dir = staged_root();
    let root = dir.path().join("root");
    let first = activate(&root, &[("ARCHIX_TEST_CRASH", "activate-marked")]);
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&first.status),
        Some(libc::SIGKILL),
        "{}",
        text(&first)
    );
    assert_eq!(fs::read_dir(root.join("var/lib/archix/activated")).unwrap().count(), 1);
    assert_eq!(fs::read_dir(root.join("var/lib/archix/activating")).unwrap().count(), 1);
    let again = activate(&root, &[]);
    assert!(again.status.success(), "{}", text(&again));
    assert!(!text(&again).contains("cut off at an earlier boot"), "{}", text(&again));
    assert_eq!(var_state(&root.join("var")), want);
    assert_eq!(fs::read_dir(root.join("var/lib/archix/activating")).unwrap().count(), 0);
}

#[test]
#[ignore]
fn failing_hooks_rerun_at_three_boots_then_wait_for_archix_hooks() {
    let dir = staged_root();
    let root = dir.path().join("root");
    let pending = root.join("var/lib/archix/pending-hooks");
    fs::create_dir_all(&pending).unwrap();
    fs::write(pending.join("2.json"), r#"{"hooks": {"h": {"run": "false"}}}"#).unwrap();
    let notes = || fs::read_to_string(root.join("var/lib/archix/notes")).unwrap_or_default();
    for boot in 1..=3 {
        let out = activate(&root, &[]);
        assert!(out.status.success(), "{}", text(&out));
        assert!(text(&out).contains("hook h"), "boot {boot}: {}", text(&out));
    }
    assert!(
        notes().contains("try 1 of 3") && notes().contains("try 2 of 3"),
        "{}",
        notes()
    );
    assert!(notes().contains("failed at 3 boots"), "{}", notes());
    assert!(notes().contains("archix hooks retry"), "{}", notes());
    let fourth = activate(&root, &[]);
    assert!(fourth.status.success());
    assert!(
        !text(&fourth).contains("hook h") && text(&fourth).contains("don't run at boot any more"),
        "{}",
        text(&fourth)
    );
    assert!(
        pending.join("2.json").exists(),
        "they wait for archix hooks retry or skip"
    );
}

#[test]
#[ignore]
fn a_failing_sysusers_stops_the_first_boot_until_the_third_try() {
    let dir = staged_root();
    let root = dir.path().join("root");
    // The container's /tmp is noexec, so the failing stand-in lives in /var/tmp.
    let bin = tempfile::tempdir_in("/var/tmp").unwrap();
    let sysusers = bin.path().join("systemd-sysusers");
    fs::write(&sysusers, "#!/bin/sh\necho 'cannot create users' >&2\nexit 1\n").unwrap();
    fs::set_permissions(&sysusers, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap());
    let activate = |root: &Path, env: &[(&str, &str)]| {
        let mut env = env.to_vec();
        env.push(("PATH", &path));
        activate(root, &env)
    };
    let seed = root.join("var/lib/app/seed");
    for tried in 1..=2 {
        let out = activate(&root, &[]);
        assert!(!out.status.success(), "try {tried}: {}", text(&out));
        assert!(text(&out).contains(&format!("try {tried} of 3")), "{}", text(&out));
        assert!(!root.join("var/lib/archix/activated").exists());
        assert_eq!(fs::read_to_string(&seed).unwrap(), "edited live after the commit");
    }
    let notes = fs::read_to_string(root.join("var/lib/archix/notes")).unwrap();
    assert!(
        notes.contains("systemd-sysusers failed for deployment 2 (try 2 of 3)"),
        "{notes}"
    );
    let third = activate(&root, &[]);
    assert!(third.status.success(), "{}", text(&third));
    assert_eq!(fs::read_dir(root.join("var/lib/archix/activated")).unwrap().count(), 1);
    let notes = fs::read_to_string(root.join("var/lib/archix/notes")).unwrap();
    assert!(notes.contains("failed at 3 boots of deployment 2"), "{notes}");
}

/// A root with systemd's units, a stand-in for the initrd's generated sysroot.mount, archix and the deployment files.
fn unit_root(edit: impl Fn(String) -> String) -> tempfile::TempDir {
    // The container's /tmp is mounted noexec, which systemd-analyze reports as an ExecStart it can't run.
    let dir = tempfile::tempdir_in("/var/tmp").unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("usr/lib/systemd")).unwrap();
    sh(
        "cp",
        &[
            "-a",
            "/usr/lib/systemd/system",
            root.join("usr/lib/systemd/").to_str().unwrap(),
        ],
    );
    fs::create_dir_all(root.join("usr/bin")).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_archix"), root.join("usr/bin/archix")).unwrap();
    let units = root.join("usr/lib/systemd/system");
    fs::write(
        units.join("sysroot.mount"),
        "[Mount]\nWhat=/dev/disk/by-label/root\nWhere=/sysroot\n",
    )
    .unwrap();
    for (path, content) in all_boot_files(root) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, edit(content)).unwrap();
    }
    fs::copy(
        root.join("usr/lib/archix/archix-deploy-mount.service"),
        units.join("archix-deploy-mount.service"),
    )
    .unwrap();
    fs::create_dir_all(units.join("initrd-root-fs.target.requires")).unwrap();
    std::os::unix::fs::symlink(
        "../archix-deploy-mount.service",
        units.join("initrd-root-fs.target.requires/archix-deploy-mount.service"),
    )
    .unwrap();
    let (link, target) = deploy_boot::activate_wants(root);
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(target, link).unwrap();
    sh("chmod", &["-R", "go-w", root.to_str().unwrap()]);
    dir
}

fn verify(root: &Path, units: &[&str]) -> String {
    let out = Command::new("systemd-analyze")
        .arg("verify")
        .arg(format!("--root={}", root.display()))
        .arg("--man=no")
        .args(units)
        .output()
        .unwrap();
    format!("exit {:?}\n{}", out.status.code(), text(&out))
}

#[test]
#[ignore]
fn the_units_verify_without_ordering_cycles() {
    let dir = unit_root(|unit| unit);
    let initrd = verify(
        dir.path(),
        &[
            "/usr/lib/systemd/system/archix-deploy-mount.service",
            "/usr/lib/systemd/system/initrd.target",
        ],
    );
    assert!(
        initrd.starts_with("exit Some(0)") && !initrd.contains("cycle"),
        "{initrd}"
    );
    let boot = verify(
        dir.path(),
        &[
            "/etc/systemd/system/archix-deploy-activate.service",
            "/usr/lib/systemd/system/sysinit.target",
            "/usr/lib/systemd/system/multi-user.target",
        ],
    );
    assert!(boot.starts_with("exit Some(0)") && !boot.contains("cycle"), "{boot}");
    assert!(!boot.contains("archix"), "{boot}");

    let (path, flatpak) = archix::integration::boot_files(Path::new("/usr/bin/archix"))
        .into_iter()
        .find(|(path, _)| path.ends_with("archix-flatpak.service"))
        .expect("a unit installs Flatpak apps at a deployment's first boot");
    let unit = dir.path().join(path.strip_prefix("/").unwrap());
    sh("chmod", &["-R", "u+w", dir.path().to_str().unwrap()]);
    fs::write(&unit, flatpak).unwrap();
    sh("chmod", &["-R", "go-w", dir.path().to_str().unwrap()]);
    let online = verify(
        dir.path(),
        &[path.to_str().unwrap(), "/usr/lib/systemd/system/multi-user.target"],
    );
    assert!(
        online.starts_with("exit Some(0)") && !online.contains("cycle"),
        "{online}"
    );

    // The prototype's first unit: ordered before systemd-sysusers.service, which runs before local-fs.target.
    let cyclic = unit_root(|unit| {
        unit.replace(
            "After=local-fs.target systemd-sysusers.service",
            "After=local-fs.target",
        )
        .replace(
            "Before=sysinit.target",
            "Before=systemd-sysusers.service sysinit.target",
        )
    });
    let found = verify(
        cyclic.path(),
        &[
            "/etc/systemd/system/archix-deploy-activate.service",
            "/usr/lib/systemd/system/sysinit.target",
        ],
    );
    assert!(found.contains("ordering cycle"), "the check finds a cycle: {found}");
}

/// Writes the hook and unit files under ARCHIX_VM_FILES for tests/vm/deploy-boot; does nothing without it.
#[test]
fn boot_files_for_the_vm_image() {
    let Ok(dir) = std::env::var("ARCHIX_VM_FILES") else {
        return;
    };
    for (path, content) in all_boot_files(Path::new(&dir)) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
}

fn kernel() -> String {
    fs::read_dir("/usr/lib/modules")
        .unwrap()
        .flatten()
        .find(|e| e.path().join("vmlinuz").exists())
        .unwrap()
        .file_name()
        .to_string_lossy()
        .into_owned()
}

/// Runs the busybox image's own init functions and archix-deploy hook as its init does after mounting the root, with `sysroot` where the init mounts it and `entry` as the selected boot entry.
fn run_busybox_hook(image: &Path, sysroot: Option<&Sysroot>, entry: &str) -> (Option<i32>, String) {
    let efivar = image.with_extension("efivar");
    fs::write(&efivar, efivar_bytes(entry)).unwrap();
    let selected = image.join(archix::system::boot::SELECTED_ENTRY_VAR.trim_start_matches('/'));
    let bind = sysroot.map_or(String::new(), |sysroot| {
        format!(
            "mount --bind {} {}/sysroot\n",
            sysroot.root().display(),
            image.display()
        )
    });
    let script = format!(
        "set -e\nmount -t tmpfs run {image}/run\nmount -t tmpfs sys {image}/sys\nmkdir -p {dir}\ncp {efivar} {selected}\n{bind}\
         exec chroot {image} /usr/bin/ash -c '. /init_functions; launch_interactive_shell() {{ echo SHELL; }}; \
         halt() {{ echo \"HALT $*\"; exit 3; }}; . /hooks/archix-deploy; run_latehook; echo BOOTED; \
         cat /run/archix/deployment /sysroot/usr/which; echo; touch /sysroot/usr/written 2>/dev/null && echo USR-WRITABLE || echo USR-READ-ONLY'\n",
        image = image.display(),
        dir = selected.parent().unwrap().display(),
        efivar = efivar.display(),
        selected = selected.display(),
    );
    let out = Command::new("unshare")
        .args(["-m", "--propagation", "private", "sh", "-c", &script])
        .output()
        .unwrap();
    (out.status.code(), text(&out))
}

#[test]
#[ignore]
fn mkinitcpio_puts_the_hook_into_busybox_and_systemd_initramfs_images() {
    let _system = lock_system();
    // The container's /tmp is noexec, and the busybox image is run from where it's unpacked.
    let dir = tempfile::tempdir_in("/var/tmp").unwrap();
    let root = dir.path().join("root");
    for (path, content) in &deploy_boot::hook_files(&root) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    let hook = root.join("etc/initcpio/hooks/archix-deploy");
    sh("bash", &["-n", hook.to_str().unwrap()]);
    // The install hook copies these from where the package and archix put them.
    let staticbin = Path::new(archix::integration::STATIC_BINARY);
    let unit = Path::new("/usr/lib/archix/archix-deploy-mount.service");
    let placed: Vec<&Path> = [staticbin, unit].into_iter().filter(|path| !path.exists()).collect();
    fs::create_dir_all(staticbin.parent().unwrap()).unwrap();
    if placed.contains(&staticbin) {
        fs::copy(env!("CARGO_BIN_EXE_archix"), staticbin).unwrap();
    }
    if placed.contains(&unit) {
        fs::copy(root.join("usr/lib/archix/archix-deploy-mount.service"), unit).unwrap();
    }
    let kernel = kernel();
    let build = |hooks: &str| {
        let conf = dir.path().join(format!("{hooks}.conf"));
        fs::write(
            &conf,
            format!("MODULES=()\nBINARIES=()\nFILES=()\nHOOKS=({hooks} archix-deploy)\nCOMPRESSION=cat\n"),
        )
        .unwrap();
        let image = dir.path().join(format!("{hooks}.img"));
        let out = Command::new("mkinitcpio")
            .args([
                "-c",
                conf.to_str().unwrap(),
                "-k",
                &kernel,
                "-g",
                image.to_str().unwrap(),
            ])
            .arg("-D")
            .arg(root.join("etc/initcpio"))
            .args(["-D", "/usr/lib/initcpio"])
            .output()
            .unwrap();
        let listing = sh("lsinitcpio", &[image.to_str().unwrap()]);
        let unpacked = dir.path().join(hooks.replace(' ', "-"));
        fs::create_dir(&unpacked).unwrap();
        let extracted = Command::new("lsinitcpio")
            .arg("-x")
            .arg(&image)
            .current_dir(&unpacked)
            .status()
            .unwrap();
        assert!(extracted.success());
        let config = fs::read_to_string(unpacked.join("config")).unwrap_or_default();
        let _ = fs::remove_file(&image);
        (text(&out), out.status.success(), listing, config, unpacked)
    };
    let busybox = build("base udev");
    let systemd = build("base systemd");
    let _ = fs::remove_dir_all(&systemd.4);
    for path in placed {
        let _ = fs::remove_file(path);
    }

    let (log, ok, listing, config, unpacked) = busybox;
    assert!(ok, "{log}");
    assert!(listing.lines().any(|l| l == "usr/bin/archix"), "{listing}");
    assert!(listing.lines().any(|l| l == "hooks/archix-deploy"), "{listing}");
    assert!(
        config
            .lines()
            .any(|l| l.starts_with("LATEHOOKS=") && l.contains("archix-deploy")),
        "the busybox init runs the hook after mounting the root: {config}"
    );
    let sysroot = Sysroot::new();
    let (code, out) = run_busybox_hook(&unpacked, Some(&sysroot), "archix-2.conf");
    assert_eq!(code, Some(0), "{out}");
    assert!(
        out.contains("archix: booting deployment 2 (the boot entry archix-2.conf)"),
        "{out}"
    );
    assert!(out.contains("BOOTED\n2\n2\nUSR-READ-ONLY"), "{out}");
    let (code, out) = run_busybox_hook(&unpacked, None, "archix-2.conf");
    assert_eq!(code, Some(3), "{out}");
    assert!(
        out.contains("ERROR: archix couldn't mount") && out.contains("SHELL") && out.contains("HALT -f"),
        "{out}"
    );
    assert!(!out.contains("BOOTED"), "nothing boots half-mounted: {out}");

    let (log, ok, listing, _, _) = systemd;
    assert!(ok, "{log}");
    assert!(listing.lines().any(|l| l == "usr/bin/archix"), "{listing}");
    assert!(
        listing
            .lines()
            .any(|l| l == "usr/lib/systemd/system/archix-deploy-mount.service"),
        "{listing}"
    );
    assert!(
        listing
            .lines()
            .any(|l| l == "usr/lib/systemd/system/initrd-root-fs.target.requires/archix-deploy-mount.service"),
        "{listing}"
    );
}

#[test]
fn a_mount_or_activation_that_fails_on_a_counted_try_reboots_and_otherwise_stops() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    fs::create_dir_all(root.join("run/archix")).unwrap();
    fs::write(root.join("run/archix/deployment"), "5\n").unwrap();
    let efivar = dir.path().join("efivar");
    let run_dir = dir.path().join("run");
    let attempt = |args: &[&str], counted: &str| {
        let out = Command::new(env!("CARGO_BIN_EXE_archix"))
            .args(args)
            .env("ARCHIX_TEST_COUNTED_BOOT", counted)
            .output()
            .unwrap();
        assert!(!out.status.success(), "{args:?} should fail: {}", text(&out));
        text(&out)
    };
    let mount = [
        "deploy-mount",
        root.to_str().unwrap(),
        "--efivar",
        efivar.to_str().unwrap(),
        "--run-dir",
        run_dir.to_str().unwrap(),
    ];
    let activate = ["deploy-activate", "--root", root.to_str().unwrap()];
    for args in [&mount[..], &activate[..]] {
        let out = attempt(args, "1");
        assert!(
            out.contains("counted boot tries; rebooting in 10 seconds")
                && out.contains("rebooting (skipped in a test)"),
            "{args:?} on a counted try:\n{out}"
        );
        let out = attempt(args, "0");
        assert!(!out.contains("rebooting"), "{args:?} on an uncounted boot:\n{out}");
    }
}

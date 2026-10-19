//! Reads the real system; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use common::*;

fn archix(config: &str, args: &[&str]) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_archix"))
        .args(["--config", config, "--host", "initbox"])
        .args(args)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), text)
}

const HAND_WRITTEN: &str = "/etc/sysctl.d/50-init-test.conf";

fn write_hand_written() {
    fs::create_dir_all("/etc/sysctl.d").unwrap();
    fs::write(HAND_WRITTEN, "vm.swappiness = 10\n").unwrap();
    fs::set_permissions(HAND_WRITTEN, fs::Permissions::from_mode(0o644)).unwrap();
}

/// A partial upgrade: lemon from a later snapshot than the rest of the image.
fn install_newer_lemon() {
    let file = std::env::temp_dir().join("lemon-3.54.0-1-x86_64.pkg.tar.zst");
    if !file.exists() {
        let url = "https://archive.archlinux.org/repos/2026/10/14/core/os/x86_64/lemon-3.54.0-1-x86_64.pkg.tar.zst";
        assert!(
            archix::archive::Archive::new(archix::archive::ARCHIVE_URL)
                .fetch(url, &file)
                .unwrap()
        );
    }
    pacman(&["-U", file.to_str().unwrap()]);
}

#[test]
#[ignore]
fn init_describes_the_machine_so_nothing_needs_to_change() {
    let _system = lock_system();
    write_hand_written();
    install_newer_lemon();
    let user = Command::new("useradd").args(["-m", "dotuser"]).status().unwrap();
    assert!(user.success());
    fs::write("/home/dotuser/.bashrc", "alias ll='ls -l'\n").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("cfg");
    let config = config.to_str().unwrap();

    let (ok, out) = archix(config, &["init", "--defaults"]);
    fs::remove_file(HAND_WRITTEN).unwrap();
    assert!(ok, "init failed:\n{out}");
    let lock = archix::lock::Lockfile::load(&dir.path().join("cfg/archix.lock"))
        .unwrap()
        .hosts["initbox"]
        .clone();
    assert_eq!(
        lock.date.to_string(),
        IMAGE_SNAPSHOT,
        "one newer package doesn't move the snapshot"
    );
    assert_eq!(lock.pinned, ["lemon"]);
    assert_eq!(lock.packages["lemon"].version, "3.54.0-1");
    assert!(
        out.contains("the system matches the lock and the config"),
        "init's own diff isn't empty:\n{out}"
    );
    let host = fs::read_to_string(format!("{config}/lua/hosts/initbox/init.lua")).unwrap();
    assert!(
        host.contains(
            "[\"/etc/sysctl.d/50-init-test.conf\"] = { source = \"files/initbox/etc/sysctl.d/50-init-test.conf\" }"
        ),
        "hand-written config files are captured:\n{host}"
    );
    assert_eq!(
        fs::read_to_string(format!("{config}/files/initbox/etc/sysctl.d/50-init-test.conf")).unwrap(),
        "vm.swappiness = 10\n"
    );
    assert!(host.contains("dotuser = "), "users are declared:\n{host}");
    assert!(!host.contains("home = {"), "--defaults never imports dotfiles:\n{host}");
    assert!(
        !fs::symlink_metadata("/home/dotuser/.bashrc")
            .unwrap()
            .file_type()
            .is_symlink()
            && !std::path::Path::new(&format!("{config}/home")).exists()
    );
    let packages = archix::installed::read(std::path::Path::new(&format!(
        "{config}/lua/hosts/initbox/installed.lua"
    )))
    .unwrap();
    assert!(packages.iter().any(|p| p == "base-devel"), "{packages:?}");

    write_hand_written();
    let (ok, diff) = archix(config, &["diff"]);
    fs::remove_file(HAND_WRITTEN).unwrap();
    assert!(ok, "{diff}");
    assert!(diff.contains("the system matches the lock and the config"), "{diff}");

    let (ok, again) = archix(config, &["init", "--defaults"]);
    assert!(!ok && again.contains("already has an init.lua"), "{again}");
    pacman(&["-R", "lemon"]);
    assert!(
        Command::new("userdel")
            .args(["-r", "dotuser"])
            .status()
            .unwrap()
            .success()
    );
}

const HANDMADE: &str = "archix-test-handmade";

/// A package built by hand and installed with pacman -U, which no repo or the AUR has.
fn install_handmade(work: &std::path::Path) {
    let root = work.join("pkg");
    fs::create_dir_all(root.join("usr/bin")).unwrap();
    fs::create_dir_all(root.join("etc")).unwrap();
    let script = root.join("usr/bin").join(HANDMADE);
    fs::write(&script, "#!/bin/sh\necho handmade\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(root.join("etc").join(format!("{HANDMADE}.conf")), "setting = 1\n").unwrap();
    fs::write(
        root.join(".PKGINFO"),
        format!(
            "pkgname = {HANDMADE}\npkgbase = {HANDMADE}\npkgver = 1.2-3\npkgdesc = made by hand\nurl = https://example.org\n\
             builddate = 1700000000\npackager = test\nsize = 40\narch = any\nlicense = MIT\nbackup = etc/{HANDMADE}.conf\ndepend = bash\n"
        ),
    )
    .unwrap();
    fs::write(
        root.join(".INSTALL"),
        format!("post_install() {{\n  touch /var/tmp/{HANDMADE}-installed\n}}\n"),
    )
    .unwrap();
    let file = work.join(format!("{HANDMADE}-1.2-3-any.pkg.tar.zst"));
    let made = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "bsdtar -czf .MTREE --format=mtree --options='!all,use-set,type,uid,gid,mode,time,size,sha256,link' .PKGINFO .INSTALL usr etc \
             && bsdtar -cf - .MTREE .PKGINFO .INSTALL usr etc | zstd -q > {}",
            file.display()
        ))
        .current_dir(&root)
        .status()
        .unwrap();
    assert!(made.success());
    pacman(&["-U", file.to_str().unwrap()]);
}

#[test]
#[ignore]
fn init_rebuilds_a_picked_package_no_repo_has_and_keeps_its_files_out_of_the_config() {
    let _system = lock_system();
    let work = tempfile::tempdir().unwrap();
    install_handmade(work.path());
    let conf = format!("/etc/{HANDMADE}.conf");
    fs::write(&conf, "setting = 2\n").unwrap();
    // pacman -U from a path keeps no copy; one in pacman's cache gives init the config file as shipped.
    let built = work.path().join(format!("{HANDMADE}-1.2-3-any.pkg.tar.zst"));
    fs::copy(
        &built,
        format!("/var/cache/pacman/pkg/{HANDMADE}-1.2-3-any.pkg.tar.zst"),
    )
    .unwrap();

    let unpicked = work.path().join("unpicked");
    let (unpicked_ok, unpicked_out) = archix(unpicked.to_str().unwrap(), &["init", "--defaults"]);
    let unpicked_host = fs::read_to_string(unpicked.join("lua/hosts/initbox/init.lua")).unwrap_or_default();

    let config = work.path().join("cfg");
    let (ok, out) = archix(config.to_str().unwrap(), &["init", "--defaults", "--repack", HANDMADE]);
    let recipe = config.join("pkgbuilds").join(HANDMADE);
    let mut in_recipe: Vec<String> = fs::read_dir(&recipe)
        .map(|dir| {
            dir.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    in_recipe.sort();
    let pkgbuild = fs::read_to_string(recipe.join("PKGBUILD")).unwrap_or_default();
    let install = fs::read_to_string(recipe.join(format!("{HANDMADE}.install"))).unwrap_or_default();
    let declared = fs::read_to_string(config.join("lua/hosts/initbox/installed.lua")).unwrap_or_default();

    // As on another switch that has to build it: the package isn't installed, and the recipe holds no files.
    pacman(&["-Rdd", HANDMADE]);
    let _ = fs::remove_file(&conf);
    let _ = fs::remove_file(format!("/var/tmp/{HANDMADE}-installed"));
    let setup = Setup::with_files(
        &[HANDMADE],
        "",
        &[
            (&format!("pkgbuilds/{HANDMADE}/PKGBUILD"), &pkgbuild),
            (&format!("pkgbuilds/{HANDMADE}/{HANDMADE}.install"), &install),
        ],
    );
    let switched = setup.run(&["switch", "--noconfirm"]);
    let script = fs::read(format!("/usr/bin/{HANDMADE}")).unwrap_or_default();
    let mode = fs::metadata(format!("/usr/bin/{HANDMADE}"))
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(0);
    let shipped_conf = fs::read_to_string(&conf).unwrap_or_default();
    let scriptlet_ran = std::path::Path::new(&format!("/var/tmp/{HANDMADE}-installed")).exists();
    let push = work.path().join("push");
    let pushed = Command::new(env!("CARGO_BIN_EXE_archix"))
        .args(["cache", "push", push.to_str().unwrap()])
        .output()
        .unwrap();
    let mut in_cache = Vec::new();
    let mut dirs = vec![push.clone()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            if entry.file_type().unwrap().is_dir() {
                dirs.push(entry.path());
            }
            in_cache.push(entry.path().display().to_string());
        }
    }
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    let _ = fs::remove_file(format!("/var/cache/pacman/pkg/{HANDMADE}-1.2-3-any.pkg.tar.zst"));

    assert!(unpicked_ok, "{unpicked_out}");
    assert!(
        unpicked_host.contains(&format!("\"{HANDMADE}\"")) && !unpicked.join("pkgbuilds").exists(),
        "--defaults leaves it in ignored_packages and rebuilds nothing:\n{unpicked_host}"
    );
    assert!(unpicked_out.contains("--repack NAME"), "{unpicked_out}");
    assert!(ok, "init failed:\n{out}");
    assert_eq!(
        in_recipe,
        ["PKGBUILD".to_string(), format!("{HANDMADE}.install")],
        "only the recipe goes into the config"
    );
    assert!(
        pkgbuild.contains("# archix: private") && pkgbuild.contains(&format!("backup=('etc/{HANDMADE}.conf')")),
        "{pkgbuild}"
    );
    assert!(declared.contains(&format!("\"{HANDMADE}\"")), "{declared}");
    assert!(
        switched.status.success(),
        "{}",
        String::from_utf8_lossy(&switched.stderr)
    );
    assert_eq!(script, b"#!/bin/sh\necho handmade\n");
    assert_eq!(mode, 0o755);
    assert_eq!(
        shipped_conf, "setting = 1\n",
        "the config file as the package shipped it, not the edited one"
    );
    assert!(scriptlet_ran, "the install scriptlet comes along");
    assert!(pushed.status.success(), "{}", String::from_utf8_lossy(&pushed.stderr));
    assert!(
        !in_cache.iter().any(|path| path.contains(HANDMADE)),
        "its build stays out of package caches: {in_cache:?}"
    );
}

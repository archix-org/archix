//! Reads the real system; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
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
    // init asks the real AUR which foreign packages it has; a fresh empty list in its cache answers none, offline.
    let dump = archix::aur::index::dump_path(Path::new("/var/cache/archix"), archix::aur::DEFAULT_URL);
    let _dump = RestoreFile::new(&dump);
    write_aur_dump(&dump, &[]);
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
    let script = fs::read(system_path(&format!("/usr/bin/{HANDMADE}"))).unwrap_or_default();
    let mode = fs::metadata(system_path(&format!("/usr/bin/{HANDMADE}")))
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(0);
    let shipped_conf = fs::read_to_string(system_path(&conf)).unwrap_or_default();
    // The scriptlet ran in the new deployment, whose /var changes wait in var-upper for its first boot.
    let scriptlet_ran = setup
        .machine
        .deployment(setup.machine.current().unwrap_or_default())
        .join(format!("var-upper/tmp/{HANDMADE}-installed"))
        .exists();
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

/// Runs a command on the container, which must succeed.
fn sh(command: &str) {
    let status = Command::new("sh").arg("-c").arg(command).status().unwrap();
    assert!(status.success(), "{command}");
}

/// Writes a file on the container, creating its directory.
fn put(path: &str, text: &str) {
    fs::create_dir_all(std::path::Path::new(path).parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

const UNITS: &str = "/usr/lib/systemd/system/archix-init-handmade";

const FSTAB: &str = "# <file system> <dir> <type> <options> <dump> <pass>\n\
UUID=archix-init-root / ext4 rw,relatime 0 1\n\
UUID=ABCD-1234 /efi vfat rw,fmask=0077,dmask=0077 0 2\n\
UUID=archix-init-home /home ext4 rw,relatime 0 2\n\
LABEL=archix-media /mnt/media ext4 nofail,noatime 0 0\n\
/dev/mapper/secret /mnt/secret ext4 defaults,nofail 0 0\n\
/srv/archix-a /srv/archix-b none bind 0 0\n\
UUID=archix-init-var /var ext4 defaults 0 2\n\
/swapfile none swap defaults,pri=5 0 0\n";

#[test]
#[ignore]
fn init_takes_over_what_was_set_up_by_hand_and_names_everything_it_leaves_out() {
    let _system = lock_system();
    let _restore: Vec<RestoreFile> = [
        "/etc/vconsole.conf",
        "/etc/locale.gen",
        "/etc/locale.conf",
        "/etc/fstab",
        "/etc/crypttab",
        "/etc/systemd/zram-generator.conf",
    ]
    .into_iter()
    .map(|path| RestoreFile::new(std::path::Path::new(path)))
    .collect();
    put("/etc/fstab", FSTAB);
    put(
        "/etc/crypttab",
        "secret UUID=archix-init-luks /etc/archix-init.key luks,discard\n",
    );
    put(
        "/etc/systemd/zram-generator.conf",
        "[zram0]\nzram-size = ram / 2\ncompression-algorithm = zstd\n",
    );
    let default_target = fs::read_link("/etc/systemd/system/default.target").ok();
    put(
        &format!("{UNITS}.service"),
        "[Service]\nType=oneshot\nExecStart=/bin/true\n",
    );
    put(
        &format!("{UNITS}.timer"),
        "[Timer]\nOnCalendar=daily\n\n[Install]\nWantedBy=timers.target\n",
    );
    put(
        &format!("{UNITS}.target"),
        "[Unit]\nDescription=handmade\n\n[Install]\nWantedBy=multi-user.target\n",
    );
    put(
        "/usr/lib/systemd/system/archix-init-masked.service",
        "[Service]\nExecStart=/bin/true\n",
    );
    sh("systemctl daemon-reload \
        && systemctl enable archix-init-handmade.timer archix-init-handmade.target \
        && systemctl mask archix-init-masked.service && systemctl set-default multi-user.target");
    sh(
        "groupadd -r -g 941 archix-init-grp && useradd -r -M -u 940 -d /var/lib/archix-init-sys -s /usr/bin/nologin archix-init-sys",
    );
    put("/etc/archix-init-app/config.yml", "port: 1\n");
    put("/etc/archix-init-app/config.yml.pacnew", "port: 2\n");
    put(
        "/etc/wireguard/archix-init.conf",
        "[Interface]\nAddress = 10.0.0.1/24\n",
    );
    put(
        "/etc/vconsole.conf",
        "# Written by systemd-localed(8)\nKEYMAP=us\nXKBLAYOUT=us\n",
    );
    put("/etc/locale.gen", "# en_US.UTF-8 UTF-8\nen_US.UTF-8 UTF-8  \n");
    put("/etc/locale.conf", "LANG=en_US.UTF-8\n");
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("cfg");
    let config = config.to_str().unwrap();

    let (ok, out) = archix(config, &["init", "--defaults"]);
    let host = fs::read_to_string(format!("{config}/lua/hosts/initbox/init.lua"))
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let copied = fs::read_to_string(format!(
        "{config}/files/initbox/etc/systemd/system/archix-init-handmade.timer"
    ))
    .unwrap_or_default();

    sh(
        "systemctl disable archix-init-handmade.timer archix-init-handmade.target \
        && systemctl unmask archix-init-masked.service",
    );
    match &default_target {
        Some(target) => sh(&format!(
            "ln -sfn {} /etc/systemd/system/default.target",
            target.display()
        )),
        None => fs::remove_file("/etc/systemd/system/default.target").unwrap(),
    }
    for unit in ["service", "timer", "target"] {
        fs::remove_file(format!("{UNITS}.{unit}")).unwrap();
    }
    fs::remove_file("/usr/lib/systemd/system/archix-init-masked.service").unwrap();
    sh("systemctl daemon-reload && userdel archix-init-sys && groupdel archix-init-grp");
    fs::remove_dir_all("/etc/archix-init-app").unwrap();
    fs::remove_file("/etc/wireguard/archix-init.conf").unwrap();

    assert!(ok, "init failed:\n{out}");
    assert!(
        host.contains("[\"archix-init-handmade.timer\"] = true")
            && host.contains("[\"archix-init-handmade.target\"] = true"),
        "a hand-installed unit and a target enabled against its preset keep their enablement:\n{host}\n{out}"
    );
    assert!(
        host.contains("[\"/etc/systemd/system/archix-init-handmade.service\"]")
            && copied == "[Timer]\nOnCalendar=daily\n\n[Install]\nWantedBy=timers.target\n",
        "unit files no package installed are copied to /etc/systemd/system:\n{host}"
    );
    assert!(!out.contains("left out, since no package"), "{out}");
    assert!(
        host.contains("masked_units = {")
            && host.contains("\"archix-init-masked.service\"")
            && host.contains("default_target = \"multi-user.target\""),
        "{host}"
    );
    assert!(
        host.contains("[\"archix-init-sys\"] = { uid = 940, system = true,")
            && host.contains("[\"archix-init-grp\"] = { gid = 941, system = true }"),
        "hand-made system accounts keep their ids:\n{host}"
    );
    assert!(
        host.contains("locales = { \"en_US.UTF-8\" }") && !host.contains("keymap ="),
        "{host}"
    );
    assert!(
        host.contains("[\"/etc/vconsole.conf\"]")
            && out.contains("keymap: /etc/vconsole.conf it sets more than KEYMAP"),
        "a vconsole.conf the keymap option can't say is kept as a file, and init says so:\n{out}"
    );
    assert!(
        out.contains("\n      /etc/archix-init-app/config.yml\n") && !out.contains(".pacnew"),
        "every unmanaged file is named, leftovers aren't:\n{out}"
    );
    assert!(
        out.contains("left out as secrets") && out.contains("/etc/wireguard/ (1 file)"),
        "{out}"
    );
    let checks = Checks::default();
    for path in ["/etc/fstab", "/etc/crypttab", "/etc/systemd/zram-generator.conf"] {
        checks.check(
            !host.contains(&format!("[\"{path}\"]")),
            format!("{path} is taken into options, not kept as a file:\n{host}"),
        );
    }
    checks.check(
        host.contains("file_systems = {") && host.contains("swap = {") && host.contains("zram = {"),
        format!("init writes file_systems, swap and zram:\n{host}"),
    );
    checks.check(
        out.contains("/var is left out") && out.contains("pacman database"),
        format!("init says why it leaves the /var line out:\n{out}"),
    );
    let evaluation = archix::eval::evaluate(&archix::eval::Input {
        config_dir: config.into(),
        hostname: "initbox".into(),
        ..Default::default()
    })
    .unwrap();
    let rendered = archix::system::render(&evaluation.config, std::path::Path::new(config)).unwrap();
    let lines: Vec<String> = rendered.state.fstab.iter().map(|line| line.line()).collect();
    checks.eq(
        lines,
        vec![
            "UUID=archix-init-home /home ext4 rw,relatime 0 2".to_string(),
            "LABEL=archix-media /mnt/media ext4 noatime,nofail,x-systemd.device-timeout=30s 0 0".to_string(),
            "/dev/mapper/secret /mnt/secret ext4 nofail 0 0".to_string(),
            "/srv/archix-a /srv/archix-b none bind 0 0".to_string(),
            "/swapfile none swap pri=5 0 0".to_string(),
        ],
        "the captured config renders the fstab's lines but the root's, the ESP's and /var's again",
    );
    let crypttab = rendered
        .state
        .files
        .get("/etc/crypttab")
        .map(|file| String::from_utf8_lossy(&rendered.blobs[&file.sha256]).into_owned())
        .unwrap_or_default();
    checks.check(
        crypttab.contains("secret UUID=archix-init-luks /etc/archix-init.key luks,discard,nofail\n"),
        format!("crypttab's line comes back from encrypted: {crypttab}"),
    );
    checks.finish();
}

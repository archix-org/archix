//! Downloads files and AppImages for real; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

mod common;

use std::fs;
use std::path::Path;
use std::process::Command;

use common::*;

fn source(name: &str, content: &str) -> (String, String) {
    let dir = Path::new("/var/tmp/archix-fetch-src");
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, content).unwrap();
    (
        format!("file://{}", path.display()),
        archix::fetch::sha256(&path).unwrap(),
    )
}

#[test]
#[ignore]
fn fetched_files_and_appimages_are_locked_by_hash_and_roll_back() {
    let _system = lock_system();
    let (app_url, app_sha) = source("hello.AppImage", "#!/bin/sh\necho hello from an appimage\n");
    let (conf_url, conf_sha) = source("fetched.conf", "fetched = yes\n");

    let setup = Setup::new(&[], "");
    let snippet = setup.ok(&["prefetch", &conf_url]);
    assert!(
        snippet.contains(&format!(
            "archix.fetchurl {{ url = \"{conf_url}\", sha256 = \"{conf_sha}\" }}"
        )),
        "{snippet}"
    );

    let wrong = format!(
        "files = {{ [\"/etc/archix-fetched.conf\"] = archix.fetchurl {{ url = \"{conf_url}\", sha256 = \"{}\" }} }}",
        "0".repeat(64)
    );
    setup.set_body(&[], &wrong);
    let err = setup.fails(&["switch", "--noconfirm"]);
    assert!(err.contains(&format!("has sha256 {conf_sha}")), "{err}");
    assert!(!system_path("/etc/archix-fetched.conf").exists());

    let body = format!(
        "appimages = {{ hello = {{ url = \"{app_url}\", sha256 = \"{app_sha}\", categories = {{ \"Utility\" }} }} }},
         files = {{ [\"/etc/archix-fetched.conf\"] = archix.fetchurl {{ url = \"{conf_url}\", sha256 = \"{conf_sha}\" }} }}"
    );
    setup.set_body(&[], &body);
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(
        fs::read_to_string(system_path("/etc/archix-fetched.conf")).unwrap(),
        "fetched = yes\n"
    );
    let machine = &setup.machine;
    let with_app = machine.current().unwrap();
    let tree = machine.deployment(with_app);
    let command = fs::read_link(tree.join("usr/bin/hello")).unwrap();
    assert_eq!(command, Path::new("/opt/archix/appimages/hello.AppImage"));
    let hello = Command::new("sh")
        .arg(tree.join(command.strip_prefix("/").unwrap()))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&hello.stdout), "hello from an appimage\n");
    assert!(
        fs::read_to_string(tree.join("usr/share/applications/archix-hello.desktop"))
            .unwrap()
            .contains("Categories=Utility;"),
        "the menu entry is in the deployment, beside the AppImage in its /opt"
    );
    let shared = archix::system::usr_local::dir(&machine.root);
    assert!(
        !shared.join("bin/hello").exists() && !shared.join("share/applications/archix-hello.desktop").exists(),
        "nothing of an AppImage goes into the shared /usr/local, which no rollback undoes"
    );
    assert!(installed("fuse2").is_some(), "appimages declare fuse2");

    fs::remove_dir_all("/var/tmp/archix-fetch-src").unwrap();
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    let without = machine.deployment(machine.current().unwrap());
    assert!(
        !without.join("usr/bin/hello").exists()
            && !without.join("usr/share/applications/archix-hello.desktop").exists(),
        "an AppImage that left the config takes its command and menu entry with it"
    );
    setup.ok(&["rollback", "--noconfirm"]);
    assert_eq!(
        fs::read_to_string(system_path("/etc/archix-fetched.conf")).unwrap(),
        "fetched = yes\n",
        "rolling back boots the deployment with the fetched files, without the source"
    );
    assert!(system_path("/opt/archix/appimages/hello.AppImage").exists());
    assert!(
        fs::read_link(system_path("/usr/bin/hello")).is_ok(),
        "and its command with it"
    );

    setup.ok(&["switch", "--noconfirm"]);
    assert!(!system_path("/etc/archix-fetched.conf").exists());

    let before = machine.current();
    let (app_url, app_sha) = source("hello.AppImage", "#!/bin/sh\necho hello from an appimage\n");
    setup.set_body(
        &[],
        &format!("appimages = {{ bash = {{ url = \"{app_url}\", sha256 = \"{app_sha}\" }} }}"),
    );
    let err = setup.fails(&["switch", "--noconfirm"]);
    assert!(err.contains("/usr/bin/bash is bash's"), "{err}");
    assert_eq!(machine.current(), before, "the refused switch changed nothing");
    fs::remove_dir_all("/var/tmp/archix-fetch-src").unwrap();
}

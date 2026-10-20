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
    let command = fs::read_link(system_path("/usr/local/bin/hello")).unwrap();
    let hello = Command::new("sh")
        .arg(system_path(&command.to_string_lossy()))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&hello.stdout), "hello from an appimage\n");
    assert!(
        fs::read_to_string(system_path("/usr/local/share/applications/archix-hello.desktop"))
            .unwrap()
            .contains("Categories=Utility;")
    );
    assert!(installed("fuse2").is_some(), "appimages declare fuse2");

    fs::remove_dir_all("/var/tmp/archix-fetch-src").unwrap();
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert!(!system_path("/usr/local/bin/hello").exists());
    setup.ok(&["rollback", "--noconfirm"]);
    assert_eq!(
        fs::read_to_string(system_path("/etc/archix-fetched.conf")).unwrap(),
        "fetched = yes\n",
        "rolling back boots the deployment with the fetched files, without the source"
    );
    assert!(system_path("/opt/archix/appimages/hello.AppImage").exists());

    setup.ok(&["switch", "--noconfirm"]);
    assert!(!system_path("/etc/archix-fetched.conf").exists());
}

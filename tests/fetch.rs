//! Downloads files and AppImages for real; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

mod common;

use std::fs;
use std::path::Path;
use std::process::Command;

use common::*;

fn source(name: &str, content: &str) -> (String, String) {
    let dir = Path::new("/tmp/archix-fetch-src");
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
    assert!(!Path::new("/etc/archix-fetched.conf").exists());

    let body = format!(
        "appimages = {{ hello = {{ url = \"{app_url}\", sha256 = \"{app_sha}\", categories = {{ \"Utility\" }} }} }},
         files = {{ [\"/etc/archix-fetched.conf\"] = archix.fetchurl {{ url = \"{conf_url}\", sha256 = \"{conf_sha}\" }} }}"
    );
    setup.set_body(&[], &body);
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(
        fs::read_to_string("/etc/archix-fetched.conf").unwrap(),
        "fetched = yes\n"
    );
    let hello = Command::new("/usr/local/bin/hello").output().unwrap();
    assert_eq!(String::from_utf8_lossy(&hello.stdout), "hello from an appimage\n");
    assert!(
        fs::read_to_string("/usr/local/share/applications/archix-hello.desktop")
            .unwrap()
            .contains("Categories=Utility;")
    );
    assert!(installed("fuse2").is_some(), "appimages declare fuse2");

    fs::remove_dir_all("/tmp/archix-fetch-src").unwrap();
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    assert!(!Path::new("/usr/local/bin/hello").exists());
    setup.ok(&["rollback", "--noconfirm"]);
    assert_eq!(
        fs::read_to_string("/etc/archix-fetched.conf").unwrap(),
        "fetched = yes\n",
        "rolling back restores fetched files from the store, without the source"
    );
    assert!(Path::new("/opt/archix/appimages/hello.AppImage").exists());

    setup.ok(&["switch", "--noconfirm"]);
    assert!(!Path::new("/etc/archix-fetched.conf").exists());
}

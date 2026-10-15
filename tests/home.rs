//! Writes a real user's dotfiles; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

mod common;

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use common::*;

fn owner(path: &str) -> u32 {
    fs::symlink_metadata(path).unwrap().uid()
}

#[test]
#[ignore]
fn home_files_and_programs_belong_to_the_user() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    fs::create_dir_all(setup.dir.path().join("config/nvim")).unwrap();
    fs::write(setup.dir.path().join("config/nvim/init.lua"), "-- edited live\n").unwrap();
    let body = r##"users = { homeuser = {} },
        home = {
          user = "homeuser",
          files = { [".config/nvim"] = { source = "config/nvim", live = true }, ["bin/hello"] = { text = "#!/bin/sh\necho hi\n", executable = true } },
          systemd = { timers = { tick = { Unit = { Description = "tick" }, Timer = { OnCalendar = "daily" }, Install = { WantedBy = { "timers.target" } } } } },
        },
        programs = { kitty = { enable = true, package = archix.null }, git = { enable = true, package = archix.null, settings = { user = { name = "Home User" } } } }"##;
    setup.set_body(&[], body);
    setup.ok(&["switch", "--noconfirm"]);

    let uid = fs::metadata("/home/homeuser").unwrap().uid();
    assert!(read_text("/home/homeuser/.config/kitty/kitty.conf").contains("shell_integration no-rc"));
    assert!(read_text("/home/homeuser/.config/git/config").contains("name = \"Home User\""));
    assert_eq!(owner("/home/homeuser/.config/git/config"), uid);
    assert_eq!(
        owner("/home/homeuser/.config/git"),
        uid,
        "directories archix creates in a home belong to the user"
    );
    let nvim = fs::read_link("/home/homeuser/.config/nvim").unwrap();
    assert!(
        nvim.starts_with(setup.dir.path().canonicalize().unwrap()),
        "live files link into the config repo"
    );
    assert_eq!(read_text("/home/homeuser/.config/nvim/init.lua"), "-- edited live\n");
    assert_eq!(fs::metadata("/home/homeuser/bin/hello").unwrap().mode() & 0o777, 0o755);
    assert!(Path::new("/home/homeuser/.config/systemd/user/timers.target.wants/tick.timer").exists());

    setup.set_body(&[], "users = { homeuser = {} }");
    setup.ok(&["switch", "--noconfirm"]);
    assert!(!Path::new("/home/homeuser/.config/kitty/kitty.conf").exists());
    assert!(fs::symlink_metadata("/home/homeuser/.config/nvim").is_err());
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
}

fn read_text(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// Replaces `<home>/.config/app` with a live link and takes it away again; `home` on another filesystem exercises copying.
fn live_link_over_a_directory(home: &str) {
    let user = format!("users = {{ homeuser = {{ home = \"{home}\" }} }}");
    let setup = Setup::new(&[], &user);
    setup.ok(&["switch", "--noconfirm"]);
    fs::create_dir_all(format!("{home}/.config/app")).unwrap();
    fs::write(format!("{home}/.config/app/old.conf"), "old\n").unwrap();
    fs::create_dir_all(setup.dir.path().join("dot/app")).unwrap();
    fs::write(setup.dir.path().join("dot/app/new.conf"), "new\n").unwrap();

    let linked = format!(
        "{user}, home = {{ user = \"homeuser\", files = {{ [\".config/app\"] = {{ source = \"dot/app\", live = true }} }} }}"
    );
    setup.set_body(&[], &linked);
    setup.ok(&["switch", "--noconfirm"]);
    assert!(
        fs::symlink_metadata(format!("{home}/.config/app"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(read_text(&format!("{home}/.config/app/new.conf")), "new\n");

    setup.set_body(&[], &user);
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(
        read_text(&format!("{home}/.config/app/old.conf")),
        "old\n",
        "the original directory comes back"
    );
    setup.set_body(&[], "");
    setup.ok(&["switch", "--noconfirm"]);
    let _ = fs::remove_dir_all(home);
}

#[test]
#[ignore]
fn a_live_link_replaces_a_directory_and_gives_it_back() {
    let _system = lock_system();
    live_link_over_a_directory("/home/homeuser");
    live_link_over_a_directory("/tmp/archix-other-fs/homeuser");
}

//! Writes a real user's dotfiles on a test machine; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

mod common;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;

use common::*;

/// Where an absolute path of the machine is from here.
fn at(path: &str) -> PathBuf {
    system_path(path)
}

fn owner(path: &str) -> u32 {
    fs::symlink_metadata(at(path)).unwrap().uid()
}

fn read_text(path: &str) -> String {
    fs::read_to_string(at(path)).unwrap_or_default()
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

    let uid = fs::metadata(at("/home/homeuser")).unwrap().uid();
    assert!(read_text("/home/homeuser/.config/kitty/kitty.conf").contains("shell_integration no-rc"));
    assert!(read_text("/home/homeuser/.config/git/config").contains("name = \"Home User\""));
    assert_eq!(owner("/home/homeuser/.config/git/config"), uid);
    assert_eq!(
        owner("/home/homeuser/.config/git"),
        uid,
        "directories archix creates in a home belong to the user"
    );
    let nvim = fs::read_link(at("/home/homeuser/.config/nvim")).unwrap();
    assert!(
        nvim.starts_with(setup.dir.path().canonicalize().unwrap()),
        "live files link into the config repo"
    );
    assert_eq!(fs::read_to_string(nvim.join("init.lua")).unwrap(), "-- edited live\n");
    assert_eq!(
        fs::metadata(at("/home/homeuser/bin/hello")).unwrap().mode() & 0o777,
        0o755
    );
    assert!(at("/home/homeuser/.config/systemd/user/timers.target.wants/tick.timer").is_symlink());

    setup.set_body(&[], "users = { homeuser = {} }");
    setup.ok(&["switch", "--noconfirm"]);
    assert!(!at("/home/homeuser/.config/kitty/kitty.conf").exists());
    assert!(fs::symlink_metadata(at("/home/homeuser/.config/nvim")).is_err());
}

/// Replaces `<home>/.config/app` with a live link and takes it away again.
fn live_link_over_a_directory(home: &str) {
    let user = format!("users = {{ homeuser = {{ home = \"{home}\" }} }}");
    let setup = Setup::new(&[], &user);
    setup.ok(&["switch", "--noconfirm"]);
    fs::create_dir_all(at(&format!("{home}/.config/app"))).unwrap();
    fs::write(at(&format!("{home}/.config/app/old.conf")), "old\n").unwrap();
    fs::create_dir_all(setup.dir.path().join("dot/app")).unwrap();
    fs::write(setup.dir.path().join("dot/app/new.conf"), "new\n").unwrap();

    let linked = format!(
        "{user}, home = {{ user = \"homeuser\", files = {{ [\".config/app\"] = {{ source = \"dot/app\", live = true }} }} }}"
    );
    setup.set_body(&[], &linked);
    setup.ok(&["switch", "--noconfirm"]);
    let link = at(&format!("{home}/.config/app"));
    assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    assert_eq!(
        fs::read_to_string(fs::read_link(&link).unwrap().join("new.conf")).unwrap(),
        "new\n"
    );

    setup.set_body(&[], &user);
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(
        read_text(&format!("{home}/.config/app/old.conf")),
        "old\n",
        "the original directory comes back"
    );
}

#[test]
#[ignore]
fn a_live_link_replaces_a_directory_and_gives_it_back() {
    let _system = lock_system();
    live_link_over_a_directory("/home/homeuser");
    live_link_over_a_directory("/srv/homes/homeuser");
}

#[test]
#[ignore]
fn merged_keys_and_read_only_files() {
    let _system = lock_system();
    let setup = Setup::new(&[], "");
    let body = r##"users = { homeuser = {} },
        mutable_files = false,
        home = {
          user = "homeuser",
          files = {
            [".config/kdeglobals"] = { merge = { format = "kconfig", values = { General = { ColorScheme = "Breeze" } } } },
            [".config/app/settings.json"] = { merge = { format = "json", values = { bar = { position = "top" } } } },
            [".zshrc"] = { text = "echo managed" },
            [".config/notes.conf"] = { text = "free", mutable = true },
          },
        }"##;
    setup.set_body(&[], body);
    setup.ok(&["switch", "--noconfirm"]);
    fs::write(
        at("/home/homeuser/.config/kdeglobals"),
        "[General]\nColorScheme=Old\nfont=Noto\n\n[KDE]\nSingleClick=false\n",
    )
    .unwrap();
    fs::write(
        at("/home/homeuser/.config/app/settings.json"),
        "{\"bar\":{\"position\":\"bottom\",\"height\":30}}",
    )
    .unwrap();
    setup.ok(&["switch", "--noconfirm"]);

    assert_eq!(
        read_text("/home/homeuser/.config/kdeglobals"),
        "[General]\nColorScheme[$i]=Breeze\nfont=Noto\n\n[KDE]\nSingleClick=false\n",
        "only the declared key changes, locked with [$i] since files are read-only"
    );
    let json: serde_json::Value = serde_json::from_str(&read_text("/home/homeuser/.config/app/settings.json")).unwrap();
    assert_eq!(json, serde_json::json!({ "bar": { "position": "top", "height": 30 } }));
    assert_eq!(fs::metadata(at("/home/homeuser/.zshrc")).unwrap().mode() & 0o777, 0o444);
    assert_eq!(
        fs::metadata(at("/home/homeuser/.config/notes.conf")).unwrap().mode() & 0o777,
        0o644
    );

    let mut kde = read_text("/home/homeuser/.config/kdeglobals");
    kde.push_str("[Icons]\nTheme=Papirus\n");
    fs::write(at("/home/homeuser/.config/kdeglobals"), kde).unwrap();
    fs::write(
        at("/home/homeuser/.config/app/settings.json"),
        "{\n    \"bar\": {\"height\": 31, \"position\": \"top\"}\n}\n",
    )
    .unwrap();
    assert!(
        setup.ok(&["diff"]).contains("matches the lock and the config"),
        "an app's own keys and formatting aren't drift"
    );

    fs::set_permissions(at("/home/homeuser/.zshrc"), fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(at("/home/homeuser/.zshrc"), "echo edited").unwrap();
    setup.ok(&["switch", "--noconfirm"]);
    assert_eq!(read_text("/home/homeuser/.zshrc"), "echo managed");
    assert_eq!(fs::metadata(at("/home/homeuser/.zshrc")).unwrap().mode() & 0o777, 0o444);

    setup.set_body(&[], "users = { homeuser = {} }");
    setup.ok(&["switch", "--noconfirm"]);
    assert!(
        read_text("/home/homeuser/.config/kdeglobals").contains("ColorScheme[$i]=Breeze"),
        "a merged file leaving the config keeps its keys"
    );
}

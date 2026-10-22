//! Writes a real user's dotfiles on a test machine; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

mod common;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, lchown, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

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
    let (uid, gid) = ids(&at(home));
    give(&at(&format!("{home}/.config")), uid, gid);
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

/// Gives a tree in a home to its user, as files the user made themselves would be.
fn give(path: &Path, uid: u32, gid: u32) {
    let status = Command::new("chown")
        .arg("-hR")
        .arg(format!("{uid}:{gid}"))
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
}

/// A program for ARCHIX_TEST_MARK, which archix runs at its named steps, that runs `script` at `step` only.
fn mark_hook(root: &Path, name: &str, step: &str, script: &str) -> PathBuf {
    let path = root.join("tmp").join(name);
    write_executable(&path, &format!("#!/bin/sh\n[ \"$1\" = {step} ] || exit 0\n{script}\n"));
    path
}

fn ids(path: &Path) -> (u32, u32) {
    let meta = fs::symlink_metadata(path).unwrap();
    (meta.uid(), meta.gid())
}

fn output_text(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr)
}

#[test]
#[ignore]
fn a_users_links_never_make_archix_write_or_move_files_outside_the_home() {
    let _system = lock_system();
    let setup = Setup::new(&[], "users = { homeuser = {} }");
    setup.ok(&["switch", "--noconfirm"]);
    let root = setup.machine.root.clone();
    let home = at("/home/homeuser");
    let (uid, gid) = ids(&home);
    let victim = root.join("srv/victim");
    fs::write(&victim, "secret\n").unwrap();
    fs::set_permissions(&victim, fs::Permissions::from_mode(0o600)).unwrap();
    let victim_dir = root.join("srv/victim-dir");
    fs::create_dir_all(&victim_dir).unwrap();
    let config = home.join(".config");
    fs::create_dir_all(config.join("gone")).unwrap();
    fs::write(config.join("gone/old.conf"), "mine\n").unwrap();
    symlink(&victim_dir, config.join("linked")).unwrap();
    give(&config, uid, gid);
    let plant = format!(
        "ln -s {} {}/.kdeglobals.archix.$PPID",
        victim.display(),
        config.display()
    );
    let hook = mark_hook(&root, "plant.sh", "merge-read", &plant);
    let files = r#"[".config/kdeglobals"] = { merge = { format = "kconfig", values = { General = { ColorScheme = "Breeze" } } } },
        [".config/linked/app.conf"] = { text = "x\n" },
        [".config/gone/old.conf"] = { text = "managed\n" }"#;
    setup.set_body(
        &[],
        &format!(r#"users = {{ homeuser = {{}} }}, home = {{ user = "homeuser", files = {{ {files} }} }}"#),
    );
    let out = setup.run_env(
        &[("ARCHIX_TEST_MARK", hook.to_str().unwrap())],
        &["switch", "--noconfirm"],
    );
    let text = output_text(&out);
    let untouched = |why: &str, text: &str| {
        let meta = fs::metadata(&victim).unwrap();
        assert_eq!(
            (
                fs::read_to_string(&victim).unwrap().as_str(),
                meta.uid(),
                meta.mode() & 0o7777
            ),
            ("secret\n", 0, 0o600),
            "{why}:\n{text}"
        );
        let entries: Vec<_> = fs::read_dir(&victim_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert!(entries.is_empty(), "{why}: {entries:?}\n{text}");
    };
    untouched(
        "a link at archix's temporary name or on a home file's path leads out of the home",
        &text,
    );
    assert!(
        text.contains(".config/linked/app.conf") && text.contains("Permission denied"),
        "the file behind the user's link out of the home fails as it would for the user:\n{text}"
    );
    let kde = config.join("kdeglobals");
    assert!(fs::symlink_metadata(&kde).unwrap().is_file(), "{text}");
    assert!(
        fs::read_to_string(&kde).unwrap().contains("ColorScheme=Breeze"),
        "{text}"
    );
    assert_eq!(ids(&kde), (uid, gid), "a new merged file belongs to the user");
    assert_eq!(fs::read_to_string(config.join("gone/old.conf")).unwrap(), "managed\n");
    assert_eq!(ids(&config.join("gone/old.conf")), (uid, gid));

    fs::remove_dir_all(config.join("gone")).unwrap();
    symlink(&victim_dir, config.join("gone")).unwrap();
    lchown(config.join("gone"), Some(uid), Some(gid)).unwrap();
    setup.set_body(&[], "users = { homeuser = {} }");
    let out = setup.run(&["switch", "--noconfirm"]);
    untouched(
        "restoring the original of a file whose directory became a link out of the home",
        &output_text(&out),
    );
}

#[test]
#[ignore]
fn an_app_writing_a_merged_file_during_the_switch_keeps_its_write() {
    let _system = lock_system();
    let setup = Setup::new(&[], "users = { homeuser = {} }");
    setup.ok(&["switch", "--noconfirm"]);
    let root = setup.machine.root.clone();
    let home = at("/home/homeuser");
    let (uid, gid) = ids(&home);
    let kde = home.join(".config/kdeglobals");
    fs::create_dir_all(kde.parent().unwrap()).unwrap();
    fs::write(&kde, "[General]\nColorScheme=Old\n").unwrap();
    give(&home.join(".config"), uid, gid);
    let f = kde.display();
    let save = |theme: &str| {
        format!("printf '[General]\\nColorScheme=Old\\n\\n[Icons]\\nTheme={theme}\\n' > {f}.new && mv {f}.new {f}")
    };
    let once = format!("[ -e {f}.saved ] && exit 0; touch {f}.saved; {}", save("Papirus"));
    let hook = mark_hook(&root, "app.sh", "merge-read", &once);
    setup.set_body(
        &[],
        r#"users = { homeuser = {} }, home = { user = "homeuser", files = { [".config/kdeglobals"] = { merge = { format = "kconfig", values = { General = { ColorScheme = "Breeze" } } } } } }"#,
    );
    setup.ok_env(
        &[("ARCHIX_TEST_MARK", hook.to_str().unwrap())],
        &["switch", "--noconfirm"],
    );
    let text = fs::read_to_string(&kde).unwrap();
    assert!(
        text.contains("ColorScheme=Breeze") && text.contains("Theme=Papirus"),
        "an app saving between archix's read and its rename keeps its write, with archix's key set on top:\n{text}"
    );

    fs::write(&kde, "[General]\nColorScheme=Old\n").unwrap();
    let holder = format!(
        "(sleep 3; {}; rm -f {f}.lock) > /dev/null 2>&1 &\n\
         printf '%s\\nkwriteconfig6\\n%s\\n' $! \"$(cat /proc/sys/kernel/hostname)\" > {f}.lock",
        save("Breath")
    );
    let hook = mark_hook(&root, "holder.sh", "merge-start", &holder);
    setup.ok_env(
        &[("ARCHIX_TEST_MARK", hook.to_str().unwrap())],
        &["switch", "--noconfirm"],
    );
    let lock = PathBuf::from(format!("{f}.lock"));
    let deadline = Instant::now() + Duration::from_secs(15);
    while lock.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    let text = fs::read_to_string(&kde).unwrap();
    assert!(
        text.contains("ColorScheme=Breeze") && text.contains("Theme=Breath"),
        "archix waits for KDE's lock on the file, then sets its key on what the app saved:\n{text}"
    );
    assert!(!lock.exists(), "archix leaves no lock behind");
    assert_eq!(ids(&kde), (uid, gid));
}

//! archix's pacman hooks under real pacman runs on test machines: the snapshot before a plain transaction and its space estimate, the step after it, strict mode and the warnings about masked hooks; run only inside the systemd-booted Arch container via `tests/arch/run.sh`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use archix::snapshot::{Kind, Store};
use common::*;
use tempfile::TempDir;

/// systemd-boot 262's LoaderFeatures, so entries get boot counters.
const COUNTING: (&str, &str) = ("ARCHIX_TEST_LOADER_FEATURES", "4194303");
const OWN: (&str, &str) = ("ARCHIX_TRANSACTION", "1");
const MIB: u64 = 1 << 20;

fn sh(script: &str) -> String {
    let output = Command::new("bash").args(["-ec", script]).output().unwrap();
    assert!(
        output.status.success(),
        "{script}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// pacman on `machine` with `env`, through `script` when `typed` is given, so it and the hooks have a terminal: each answer of `typed`, split at newlines, is typed once the output shows a prompt after the last answer (pacman throws away what was typed before it asks).
fn pacman(machine: &Machine, env: &[(&str, &str)], args: &[&str], typed: Option<&str>) -> Output {
    let mut argv: Vec<String> = ["pacman", "--root"].map(str::to_string).to_vec();
    argv.push(machine.root.display().to_string());
    argv.push("--dbpath".into());
    argv.push(machine.root.join("var/lib/pacman").display().to_string());
    if !args.contains(&"--config") {
        argv.push("--config".into());
        argv.push(machine.root.join("etc/pacman.conf").display().to_string());
    }
    argv.extend(args.iter().map(|arg| arg.to_string()));
    let mut env = env.to_vec();
    env.push(("ARCHIX_TEST_PLAIN_ESP", "1"));
    let Some(typed) = typed else {
        return Command::new(&argv[0])
            .args(&argv[1..])
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .output()
            .unwrap();
    };
    let log = tempfile::NamedTempFile::new().unwrap();
    let quoted: Vec<String> = argv.iter().map(|arg| format!("'{arg}'")).collect();
    let mut child = Command::new("script")
        .args(["-qec", &quoted.join(" "), "/dev/null"])
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(log.reopen().unwrap())
        .stderr(log.reopen().unwrap())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut answers = typed.lines();
    let mut seen = 0;
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        let shown = fs::read(log.path()).unwrap_or_default();
        let fresh = String::from_utf8_lossy(&shown[seen.min(shown.len())..]).into_owned();
        if fresh.contains("[Y/n]") || fresh.contains("[a] ") {
            seen = shown.len();
            if let Some(answer) = answers.next() {
                std::io::Write::write_all(&mut stdin, format!("{answer}\n").as_bytes()).unwrap();
            }
        }
        if started.elapsed() > std::time::Duration::from_secs(600) {
            let _ = child.kill();
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    drop(stdin);
    Output {
        status,
        stdout: fs::read(log.path()).unwrap(),
        stderr: Vec::new(),
    }
}

/// pacman's database on the root under /var/rollback, bound over /var/lib/pacman with a permanent lock underneath, as `archix snapshot setup` lays it out.
fn bind_pacman_db(root: &Path) {
    sh(&format!(
        "r={}; mkdir -p $r/var/rollback && mv $r/var/lib/pacman $r/var/rollback/pacman && mkdir $r/var/lib/pacman && : > $r/var/lib/pacman/db.lck && mount --bind $r/var/rollback/pacman $r/var/lib/pacman",
        root.display()
    ));
}

fn snapshots(machine: &Machine) -> Vec<archix::snapshot::Record> {
    Store::open(&machine.root).list().unwrap()
}

fn on_btrfs(size: &str, body: &str) -> Setup {
    let machine = Machine::on_btrfs_of(size);
    machine.btrfs.as_ref().unwrap().mount_archix();
    bind_pacman_db(&machine.root);
    Setup::on(machine, &[], body, &[])
}

/// What `archix boot-complete` notes after a boot of the current entry that counted as good.
fn bless(setup: &Setup) {
    let run = tempfile::tempdir().unwrap();
    setup.ok_env(
        &[
            ("ARCHIX_TEST_RUN", run.path().to_str().unwrap()),
            ("ARCHIX_TEST_ENTRY", "archix-current.conf"),
        ],
        &["boot-complete"],
    );
}

#[test]
#[ignore]
fn a_plain_pacman_run_takes_a_snapshot_of_the_root_before_it_with_pacmans_lock_in_it() {
    let _system = lock_system();
    let checks = Checks::default();
    let packages = Packages::new();
    packages.build("archix-test-synced", "");
    let setup = on_btrfs("2G", "rollback = { enable = true }, drift = \"keep\"");
    let machine = &setup.machine;
    setup.ok(&["switch", "--noconfirm"]);
    let before = snapshots(machine).len();
    let conf = packages.conf(machine);
    let conf = conf.to_str().unwrap();
    let synced = pacman(
        machine,
        &[],
        &["--config", conf, "-Sy", "--noconfirm", "archix-test-synced"],
        None,
    );
    checks.check(synced.status.success(), format!("pacman -S: {}", text(&synced)));
    let after = snapshots(machine);
    checks.eq(after.len(), before + 1, "one snapshot for the transaction");
    let record = after.last().unwrap();
    checks.eq(record.kind, Kind::Pacman, "of kind pacman");
    checks.eq(
        record.label.as_str(),
        "before pacman transaction",
        "labelled by what came after",
    );
    checks.check(
        text(&synced).contains(&format!(
            "archix: snapshot {} holds the system as it was before this transaction",
            record.number
        )),
        format!("pacman's output says so: {}", text(&synced)),
    );
    let snapshot = Store::open(&machine.root).path(record.number);
    let local = snapshot.join("var/rollback/pacman/local");
    checks.check(local.is_dir(), "the snapshot holds pacman's database through the bind");
    checks.check(
        !archix::pacman::read_local_db(&local)
            .unwrap()
            .contains_key("archix-test-synced"),
        "as it was before the transaction",
    );
    checks.check(
        snapshot.join("var/rollback/pacman/db.lck").exists(),
        "with the lock pacman held through its pre-transaction hooks, which a keep takes out of its clone",
    );
    checks.check(
        !machine.root.join("var/rollback/pacman/db.lck").exists(),
        "while the live database's lock is gone once pacman is done",
    );
    checks.check(
        !snapshot.join("usr/share/archix-test-synced").exists()
            && machine.root.join("usr/share/archix-test-synced/version").is_file(),
        "the package's files are on the root, not in the snapshot",
    );

    let own = pacman(machine, &[OWN], &["-R", "--noconfirm", "archix-test-synced"], None);
    checks.check(
        own.status.success(),
        format!("archix's own transaction: {}", text(&own)),
    );
    checks.eq(
        snapshots(machine).len(),
        before + 1,
        "archix's own transactions take none",
    );

    let recorded = machine.root.join(".archix/root.json");
    let real = fs::read_to_string(&recorded).ok();
    fs::write(
        &recorded,
        r#"{"format": 1, "uuid": "00000000-0000-0000-0000-000000000000"}"#,
    )
    .unwrap();
    let foreign = pacman(
        machine,
        &[],
        &["--config", conf, "-S", "--noconfirm", "archix-test-synced"],
        None,
    );
    checks.check(
        foreign.status.success(),
        format!("a foreign root never blocks pacman: {}", text(&foreign)),
    );
    checks.check(
        text(&foreign).contains("archix: warning: no snapshot before this transaction: the root subvolume"),
        format!("it warns in pacman's output: {}", text(&foreign)),
    );
    checks.eq(snapshots(machine).len(), before + 1, "and takes no snapshot");
    match real {
        Some(real) => fs::write(&recorded, real).unwrap(),
        None => fs::remove_file(&recorded).unwrap(),
    }
    pacman(machine, &[OWN], &["-R", "--noconfirm", "archix-test-synced"], None);
    checks.finish();
}

/// Free space on the machine's btrfs, measured as archix measures it.
fn free(machine: &Machine) -> u64 {
    sh(&format!("btrfs filesystem sync {}", machine.root.display()));
    archix::system::btrfs::space(&machine.root).unwrap().bytes
}

/// Makes the btrfs have about `target` bytes free with a file at its top level, outside every snapshot.
fn leave_free(machine: &Machine, target: u64) {
    let top = &machine.btrfs.as_ref().unwrap().top;
    let filler = top.join("filler");
    let _ = fs::remove_file(&filler);
    let now = free(machine);
    if now > target {
        sh(&format!("fallocate -l {} {}", now - target, filler.display()));
    }
    let left = free(machine);
    assert!(
        left.abs_diff(target) < 160 * MIB,
        "wanted {} MiB free, got {} MiB",
        target >> 20,
        left >> 20
    );
}

/// A snapshot that holds a file the root no longer has, which pruning it frees, then a newer one, since the newest never goes.
fn pin_data(setup: &Setup, small: &Path, size: u64) {
    let machine = &setup.machine;
    let _ = fs::remove_file(machine.btrfs.as_ref().unwrap().top.join("filler"));
    let blob = machine.root.join("archix-pinned");
    sh(&format!("fallocate -l {size} {}", blob.display()));
    let small = small.to_str().unwrap();
    let before: Vec<u32> = snapshots(machine).iter().map(|record| record.number).collect();
    let mut said = String::new();
    for _ in 0..2 {
        let out = pacman(machine, &[], &["-U", "--noconfirm", small], None);
        assert!(out.status.success(), "{}", text(&out));
        said.push_str(&text(&out));
        let _ = fs::remove_file(&blob);
        pacman(machine, &[OWN], &["-R", "--noconfirm", "archix-test-small"], None);
    }
    let new = snapshots(machine)
        .iter()
        .filter(|record| !before.contains(&record.number))
        .count();
    assert_eq!(new, 2, "both snapshots pinning the data stay: {said}");
}

fn installed_on(machine: &Machine, name: &str) -> bool {
    machine.installed(name).is_some()
}

#[test]
#[ignore]
fn the_snapshot_hook_short_of_space_aborts_prunes_or_still_tries_as_asked_or_as_rollback_low_space_says() {
    let _system = lock_system();
    let checks = Checks::default();
    let packages = Packages::new();
    let big = packages.build(
        "archix-test-big",
        "head -c 512M /dev/zero > \"$pkgdir/usr/share/archix-test-big/blob\"",
    );
    let small = packages.build("archix-test-small", "");
    let body = |low_space: &str| {
        format!("rollback = {{ enable = true, min_free = 50, entries = 0{low_space} }}, drift = \"keep\"")
    };
    let setup = on_btrfs("6G", &body(""));
    let machine = &setup.machine;
    // Inside the root, so the hook, chrooted there, reads the file pacman -U names.
    let inside = |file: &Path| {
        let copy = machine.root.join("var/tmp").join(file.file_name().unwrap());
        fs::copy(file, &copy).unwrap();
        copy
    };
    let (big, small) = (inside(&big), inside(&small));
    let big = big.to_str().unwrap();
    setup.ok(&["switch", "--noconfirm"]);
    bless(&setup);
    let gib = 1u64 << 30;
    let total = 6 * gib;
    // After the 512 MiB package and its 64 MiB margin: under the 3 GiB floor but over the 1 GiB keep reserve, or under both.
    let (still_try, tight, roomy) = (total / 2, gib + 400 * MIB, total * 3 / 4);
    let state = machine.root.join(".archix/boot/state.json");
    let reset = |checks: &Checks, what: &str| {
        if installed_on(machine, "archix-test-big") {
            pacman(machine, &[OWN], &["-R", "--noconfirm", "archix-test-big"], None);
        }
        let keep = archix::system::menu::load(&machine.root.join(".archix/boot"))
            .unwrap()
            .slot;
        for record in snapshots(machine) {
            if Some(record.number) != keep && record.kind == Kind::Pacman && record.label == "before pacman transaction"
            {
                let out = setup.run(&["snapshot", "delete", &record.number.to_string()]);
                checks.check(
                    out.status.success(),
                    format!("{what}: deleting snapshot {}: {}", record.number, text(&out)),
                );
            }
        }
    };

    leave_free(machine, roomy);
    let fine = pacman(machine, &[], &["-U", "--noconfirm", small.to_str().unwrap()], None);
    checks.check(
        fine.status.success() && !text(&fine).contains("short"),
        format!("with room to spare nothing is asked: {}", text(&fine)),
    );
    let slot = snapshots(machine).last().unwrap().number;
    pacman(machine, &[OWN], &["-R", "--noconfirm", "archix-test-small"], None);

    leave_free(machine, tight);
    let count = snapshots(machine).len();
    let aborted = pacman(machine, &[], &["-U", big], Some("y\na\n"));
    let out = text(&aborted);
    checks.check(
        !aborted.status.success() && !installed_on(machine, "archix-test-big"),
        format!("abort stops the transaction: {out}"),
    );
    checks.check(
        out.contains("this transaction needs about 576 MiB"),
        format!("the warning names the estimate, the package's whole installed size and the margin: {out}"),
    );
    checks.check(
        out.contains("[a]bort, or [p]rune old snapshots?") && !out.contains("[s]till try"),
        format!("under the keep reserve, still trying isn't offered: {out}"),
    );
    checks.eq(snapshots(machine).len(), count, "an abort takes no snapshot");
    let status = setup.ok(&["status"]);
    checks.check(
        status.contains("pacman hook,") && status.contains("aborted the transaction"),
        format!("status shows what the hook did: {status}"),
    );

    leave_free(machine, still_try);
    let tried = pacman(machine, &[], &["-U", big], Some("y\ns\n"));
    let out = text(&tried);
    checks.check(
        out.contains("[s]till try (snapshot and install anyway)?"),
        format!("over the keep reserve, still trying is offered: {out}"),
    );
    checks.check(
        tried.status.success() && installed_on(machine, "archix-test-big"),
        format!("and installs: {out}"),
    );
    let after: Vec<u32> = snapshots(machine).iter().map(|record| record.number).collect();
    let pruned: Vec<u32> = out
        .lines()
        .find_map(|line| line.strip_prefix("archix: deleted snapshots "))
        .map(|numbers| numbers.split(", ").map(|n| n.trim().parse().unwrap()).collect())
        .unwrap_or_default();
    checks.check(
        after.iter().any(|n| *n > slot) && after.contains(&slot),
        format!("with its snapshot, and the slot stays: {after:?}"),
    );
    checks.check(
        !pruned.is_empty() && after.len() == count + 1 - pruned.len(),
        format!("the transaction left the root under min_free, so the step after it pruned what no entry pins: {pruned:?}, left {after:?}"),
    );
    reset(&checks, "after still try");

    leave_free(machine, still_try);
    let refused = pacman(machine, &[], &["-U", big], Some("y\nx\n"));
    checks.check(
        !refused.status.success() && !installed_on(machine, "archix-test-big"),
        format!("any other answer aborts: {}", text(&refused)),
    );

    setup.set_body(&[], &body(", low_space = \"abort\""));
    setup.ok(&["switch", "--noconfirm"]);
    reset(&checks, "after the switch to abort");
    leave_free(machine, still_try);
    let out = pacman(machine, &[], &["-U", "--noconfirm", big], None);
    checks.check(
        !out.status.success() && !installed_on(machine, "archix-test-big"),
        format!("with no terminal, low_space = abort aborts: {}", text(&out)),
    );

    setup.set_body(&[], &body(", low_space = \"skip\""));
    setup.ok(&["switch", "--noconfirm"]);
    reset(&checks, "after the switch to skip");
    leave_free(machine, tight);
    let count = snapshots(machine).len();
    let out = pacman(machine, &[], &["-U", "--noconfirm", big], None);
    checks.check(
        out.status.success()
            && installed_on(machine, "archix-test-big")
            && text(&out).contains("no snapshot before this transaction (rollback.low_space = \"skip\")"),
        format!("low_space = skip installs without a snapshot: {}", text(&out)),
    );
    checks.eq(snapshots(machine).len(), count, "skip takes no snapshot");

    setup.set_body(&[], &body(""));
    setup.ok(&["switch", "--noconfirm"]);
    reset(&checks, "after the switch to the default");
    pin_data(&setup, &small, 3 * gib / 2);
    let saved = fs::read_to_string(&state).unwrap();
    let mut menu: serde_json::Value = serde_json::from_str(&saved).unwrap();
    menu["slot"] = serde_json::Value::Null;
    menu["armed"] = serde_json::Value::Bool(false);
    fs::write(&state, menu.to_string()).unwrap();
    leave_free(machine, tight);
    let out = pacman(machine, &[], &["-U", "--noconfirm", big], None);
    checks.check(
        !out.status.success() && text(&out).contains("needs a safe recovery point to stay"),
        format!(
            "with no terminal the default prunes only while a safe recovery point stays, and there's none: {}",
            text(&out)
        ),
    );
    checks.check(!installed_on(machine, "archix-test-big"), "so it aborts");
    fs::write(&state, saved).unwrap();

    leave_free(machine, tight);
    let held: Vec<u32> = snapshots(machine).iter().map(|record| record.number).collect();
    let out = pacman(machine, &[], &["-U", "--noconfirm", big], None);
    let after: Vec<u32> = snapshots(machine).iter().map(|record| record.number).collect();
    checks.check(
        out.status.success() && installed_on(machine, "archix-test-big"),
        format!("with the slot back, the default prunes and installs: {}", text(&out)),
    );
    checks.check(
        after.contains(&slot) && held.iter().any(|n| !after.contains(n)),
        format!(
            "pruning dropped a snapshot and kept the slot {slot}: {held:?} -> {after:?}; {}",
            text(&out)
        ),
    );
    reset(&checks, "after the default prune");

    pin_data(&setup, &small, 3 * gib / 2);
    leave_free(machine, tight);
    let out = pacman(machine, &[], &["-U", big], Some("y\np\n"));
    checks.check(
        out.status.success()
            && installed_on(machine, "archix-test-big")
            && text(&out).contains("to make room; snapshot"),
        format!("prune on the terminal makes room and installs: {}", text(&out)),
    );
    let status = setup.ok(&["status"]);
    checks.check(
        status.contains("pruned for a pacman transaction"),
        format!("status shows the prune: {status}"),
    );
    reset(&checks, "at the end");
    let _ = fs::remove_file(machine.btrfs.as_ref().unwrap().top.join("filler"));
    checks.finish();
}

#[test]
#[ignore]
fn after_a_plain_transaction_the_current_entry_gets_its_tries_again_and_a_changed_initramfs_a_new_image() {
    let _system = lock_system();
    let checks = Checks::default();
    let packages = Packages::new();
    let small = packages.build("archix-test-small", "");
    let initcpio = packages.build(
        "archix-test-initcpio",
        "install -Dm644 /dev/stdin \"$pkgdir/usr/lib/initcpio/archix-test-blob\" <<< blob; install -Dm644 /dev/stdin \"$pkgdir/etc/mkinitcpio.conf.d/archix-test.conf\" <<< 'FILES+=(/usr/lib/initcpio/archix-test-blob)'",
    );
    let setup = on_btrfs("3G", "rollback = { enable = true }, drift = \"keep\"");
    let machine = &setup.machine;
    setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    bless(&setup);
    let entries = machine.esp().join("loader/entries");
    let blessed = || {
        fs::rename(
            entries.join("archix-current+2.conf"),
            entries.join("archix-current.conf"),
        )
        .unwrap()
    };
    blessed();
    let image = |id: &str| -> Option<String> {
        machine
            .entries()
            .into_iter()
            .find(|file| file == id || file.starts_with(&format!("{}+", id.trim_end_matches(".conf"))))
            .and_then(|file| fs::read_to_string(entries.join(file)).ok())
            .and_then(|text| {
                text.lines()
                    .find_map(|line| line.strip_prefix("uki ").map(str::to_string))
            })
    };
    let first = image("archix-current.conf");

    let own = pacman(
        machine,
        &[COUNTING, OWN],
        &["-U", "--noconfirm", small.to_str().unwrap()],
        None,
    );
    checks.check(own.status.success(), text(&own));
    checks.check(
        machine.entries().contains(&"archix-current.conf".to_string()),
        format!(
            "archix's own transactions leave the entry alone: {:?}",
            machine.entries()
        ),
    );
    pacman(machine, &[OWN], &["-R", "--noconfirm", "archix-test-small"], None);

    let out = pacman(
        machine,
        &[COUNTING],
        &["-U", "--noconfirm", small.to_str().unwrap()],
        None,
    );
    checks.check(out.status.success(), text(&out));
    checks.check(
        machine.entries().contains(&"archix-current+2.conf".to_string()),
        format!(
            "a plain transaction that leaves the kernel alone still gives the current entry its two tries: {:?}; {}",
            machine.entries(),
            text(&out)
        ),
    );
    checks.check(
        !text(&out).contains("building the boot images"),
        format!("without rebuilding the image: {}", text(&out)),
    );
    checks.eq(image("archix-current.conf"), first.clone(), "which stays the same");
    let slot = snapshots(machine).last().unwrap().number;
    checks.check(
        machine.entries().contains(&format!("archix-snapshot-{slot}+1.conf")),
        format!(
            "its snapshot, taken after a good boot, is the slot and gets an entry: {:?}",
            machine.entries()
        ),
    );
    let slot_entry = fs::read_to_string(entries.join(format!("archix-snapshot-{slot}+1.conf"))).unwrap_or_default();
    checks.check(
        slot_entry.contains("sort-key archix-1"),
        format!("sorted right after the current entry: {slot_entry}"),
    );

    blessed();
    let out = pacman(
        machine,
        &[COUNTING],
        &["-U", "--noconfirm", initcpio.to_str().unwrap()],
        None,
    );
    checks.check(
        out.status.success() && text(&out).contains("the kernel or initramfs changed; building the boot images"),
        format!(
            "a package with files the initramfs is built from rebuilds the image: {}",
            text(&out)
        ),
    );
    let second = image("archix-current.conf");
    checks.check(
        second.is_some() && second != first,
        format!("a new image: {first:?} -> {second:?}"),
    );
    let before = snapshots(machine).last().unwrap().number;
    checks.eq(
        image(&format!("archix-snapshot-{before}.conf")),
        first.clone(),
        "the snapshot before it boots the old image from an entry of its own",
    );
    checks.check(
        machine.entries().contains(&"archix-current+2.conf".to_string()),
        format!("with tries again: {:?}", machine.entries()),
    );
    pacman(
        machine,
        &[OWN],
        &["-R", "--noconfirm", "archix-test-initcpio", "archix-test-small"],
        None,
    );
    checks.finish();
}

#[test]
#[ignore]
fn a_transaction_cut_off_before_the_step_after_it_leaves_the_current_entry_counted_and_the_slot_bootable() {
    let _system = lock_system();
    let checks = Checks::default();
    let packages = Packages::new();
    let cut = packages.build_scripted(
        "archix-test-cut",
        "1",
        "",
        Some("pre_install() {\n  kill -9 $PPID\n}\n"),
    );
    let setup = on_btrfs("3G", "rollback = { enable = true }, drift = \"keep\"");
    let machine = &setup.machine;
    setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    bless(&setup);
    let entries = machine.esp().join("loader/entries");
    fs::rename(
        entries.join("archix-current+2.conf"),
        entries.join("archix-current.conf"),
    )
    .unwrap();
    let out = pacman(
        machine,
        &[COUNTING],
        &["-U", "--noconfirm", cut.to_str().unwrap()],
        None,
    );
    checks.check(
        !out.status.success() && !text(&out).contains("tries again"),
        format!(
            "the scriptlet kills pacman mid-transaction, before the step after it: {}",
            text(&out)
        ),
    );
    checks.check(
        machine.entries().contains(&"archix-current+2.conf".to_string()),
        format!(
            "the current entry already has its two tries from before the transaction, so a boot it broke falls back: {:?}; {}",
            machine.entries(),
            text(&out)
        ),
    );
    let slot = snapshots(machine).last().unwrap().number;
    checks.check(
        machine.entries().contains(&format!("archix-snapshot-{slot}+1.conf")),
        format!(
            "and the snapshot before it, the slot, already has its entry: {:?}",
            machine.entries()
        ),
    );
    let marked = archix::pacman::hooks::unfinished(&setup.paths.state);
    checks.check(
        marked.as_ref().is_some_and(|unfinished| unfinished.slot == Some(slot)),
        format!("the transaction is marked as unfinished, naming the slot: {marked:?}"),
    );
    let check = setup.run_env(
        &[
            COUNTING,
            ("ARCHIX_TEST_COUNTED_BOOT", "1"),
            ("ARCHIX_TEST_ENTRY", "archix-current+2.conf"),
        ],
        &["health-check"],
    );
    checks.check(
        !check.status.success()
            && text(&check).contains("was cut off before the step after it finished")
            && text(&check).contains(&format!("`archix rollback {slot}` keeps it")),
        format!(
            "so the boot after it fails its health check, naming the slot: {}",
            text(&check)
        ),
    );
    checks.check(
        machine
            .entries()
            .iter()
            .any(|entry| entry.starts_with("archix-current+0")),
        format!(
            "and its entry is marked bad, so the next boot falls back: {:?}",
            machine.entries()
        ),
    );
    for lock in ["var/lib/pacman/db.lck", "var/rollback/pacman/db.lck"] {
        let _ = fs::remove_file(machine.root.join(lock));
    }
    let next = packages.build("archix-test-after-cut", "");
    let out = pacman(
        machine,
        &[COUNTING],
        &["-U", "--noconfirm", next.to_str().unwrap()],
        None,
    );
    checks.check(
        out.status.success() && archix::pacman::hooks::unfinished(&setup.paths.state).is_none(),
        format!("a transaction that finishes drops the mark: {}", text(&out)),
    );
    pacman(
        machine,
        &[OWN],
        &["-R", "--noconfirm", "archix-test-cut", "archix-test-after-cut"],
        None,
    );
    checks.finish();
}

/// archix as a package owning the test binary, as the AUR package owns /usr/bin/archix.
fn archix_package(packages: &Packages) -> PathBuf {
    let binary = env!("CARGO_BIN_EXE_archix");
    packages.build("archix", &format!("install -Dm755 {binary} \"$pkgdir{binary}\""))
}

/// The ways to install `file` the spec names: plain pacman, an AUR helper (makepkg's build installed with pacman -U, as yay and paru do) and a tool whose own hook runs in the transaction.
fn front_ends(machine: &Machine, packages: &Packages, n: &str, env: &[(&str, &str)]) -> Vec<(String, Output)> {
    let plain = packages.build(&format!("archix-test-plain{n}"), "");
    let helper = packages.build(&format!("archix-test-helper{n}"), "");
    let hooked = packages.build(&format!("archix-test-hooked{n}"), "");
    let hooks = machine.root.join("etc/pacman.d/hooks");
    fs::create_dir_all(&hooks).unwrap();
    fs::write(
        hooks.join("zz-archix-test-third-party.hook"),
        "[Trigger]\nOperation = Install\nType = Package\nTarget = archix-test-hooked*\n\n[Action]\nDescription = a third-party tool's hook\nWhen = PreTransaction\nExec = /bin/sh -c 'echo ran >> /var/tmp/archix-test-third-party'\n",
    )
    .unwrap();
    let mut env = env.to_vec();
    let hookdir = hooks.display().to_string();
    let with_hookdir = ["--hookdir", hookdir.as_str()];
    let run = |file: &Path, extra: &[&str], env: &[(&str, &str)]| {
        let mut args: Vec<&str> = extra.to_vec();
        args.extend(["-U", "--noconfirm", file.to_str().unwrap()]);
        pacman(machine, env, &args, None)
    };
    let plain = run(&plain, &[], &env);
    env.push(("SUDO_USER", "archix-build"));
    let helper = run(&helper, &[], &env);
    env.pop();
    let hooked = run(&hooked, &with_hookdir, &env);
    vec![
        ("plain pacman".to_string(), plain),
        ("an AUR helper".to_string(), helper),
        ("a tool with its own pacman hook".to_string(), hooked),
    ]
}

fn remove_test_packages(machine: &Machine) {
    let names: Vec<String> = archix::pacman::read_local_db(&machine.local_db())
        .unwrap()
        .into_keys()
        .filter(|name| name.starts_with("archix-test-"))
        .collect();
    if !names.is_empty() {
        let mut args = vec!["-R", "--noconfirm"];
        args.extend(names.iter().map(String::as_str));
        pacman(machine, &[OWN], &args, None);
    }
    let _ = fs::remove_file(machine.root.join("var/tmp/archix-test-third-party"));
}

#[test]
#[ignore]
fn strict_mode_blocks_every_front_end_until_its_turned_off_or_archix_its_state_or_its_config_is_gone() {
    let _system = lock_system();
    let checks = Checks::default();
    let packages = Packages::new();
    let mut setup = Setup::new(&[], "drift = \"keep\", ignored_packages = { \"archix\" }");
    setup.move_config_to_etc();
    let machine = &setup.machine;
    let archix = archix_package(&packages);
    let install_archix = || {
        let out = pacman(
            machine,
            &[OWN],
            &["-U", "--noconfirm", "--overwrite", "*", archix.to_str().unwrap()],
            None,
        );
        assert!(out.status.success(), "installing archix as a package: {}", text(&out));
    };
    install_archix();
    let config = setup.config.clone();
    let saved = TempDir::new().unwrap();
    sh(&format!("cp -a {}/. {}", config.display(), saved.path().display()));
    let strict = |on: bool| {
        let init = fs::read_to_string(config.join("init.lua")).unwrap();
        let init = init.replace("strict = true, ", "");
        let init = match on {
            true => init.replacen("return { ", "return { strict = true, ", 1),
            false => init,
        };
        fs::write(config.join("init.lua"), init).unwrap();
        setup.ok(&["switch", "--noconfirm"]);
    };
    let ran_third_party = || machine.root.join("var/tmp/archix-test-third-party").exists();
    let mut round = 0;
    let mut expect = |checks: &Checks, allowed: bool, when: &str| {
        round += 1;
        for (how, out) in front_ends(machine, &packages, &round.to_string(), &[]) {
            let said = text(&out);
            match allowed {
                true => checks.check(out.status.success(), format!("{when}: {how} installs: {said}")),
                false => checks.check(
                    !out.status.success() && said.contains("strict mode is on, so packages change only through archix"),
                    format!("{when}: strict mode blocks {how}: {said}"),
                ),
            };
        }
        checks.eq(
            ran_third_party(),
            allowed,
            format!("{when}: the third-party hook ran only if allowed"),
        );
        remove_test_packages(machine);
    };

    strict(true);
    checks.eq(
        fs::read_to_string("/var/lib/archix/strict").unwrap_or_default(),
        "/etc/archix".to_string(),
        "the flag names the config as the hooks see it",
    );
    expect(&checks, false, "on");
    let own = pacman(
        machine,
        &[OWN],
        &[
            "-U",
            "--noconfirm",
            packages.build("archix-test-own", "").to_str().unwrap(),
        ],
        None,
    );
    checks.check(
        own.status.success(),
        format!("archix's own transactions pass: {}", text(&own)),
    );
    remove_test_packages(machine);

    strict(false);
    checks.check(
        !Path::new("/var/lib/archix/strict").exists(),
        "turning it off drops the flag",
    );
    expect(&checks, true, "after turning strict off");

    strict(true);
    let binary = machine.root.join(env!("CARGO_BIN_EXE_archix").trim_start_matches('/'));
    let restore_binary = || {
        sh(&format!(
            "install -Dm755 {} {}",
            env!("CARGO_BIN_EXE_archix"),
            binary.display()
        ))
    };
    let removed = pacman(machine, &[], &["-R", "--noconfirm", "archix"], None);
    checks.check(
        removed.status.success(),
        format!("pacman -R archix passes while strict is on: {}", text(&removed)),
    );
    checks.check(!binary.exists(), "and removes the binary");
    checks.check(
        !machine
            .root
            .join("usr/share/libalpm/hooks/01-archix-snapshot.hook")
            .exists(),
        "the step after a transaction that removed archix takes its hooks away",
    );
    expect(&checks, true, "after pacman -R archix");
    restore_binary();
    install_archix();
    strict(true);
    checks.check(
        machine.root.join("run/archix/removing").exists(),
        "the removal's marker is still there",
    );
    expect(
        &checks,
        false,
        "on again after reinstalling archix, the marker of a pacman that's gone ignored",
    );

    fs::remove_file(&binary).unwrap();
    expect(&checks, true, "after deleting the binary");
    restore_binary();
    strict(true);

    sh("cp -a /var/lib/archix /var/tmp/archix-state-saved && rm -rf /var/lib/archix/*");
    expect(&checks, true, "after deleting archix's state");
    sh(
        "rm -rf /var/lib/archix/* && cp -a /var/tmp/archix-state-saved/. /var/lib/archix/ && rm -rf /var/tmp/archix-state-saved",
    );
    expect(&checks, false, "with the state back");

    sh(&format!("rm -rf {}", config.display()));
    expect(&checks, true, "after deleting the config");
    sh(&format!(
        "mkdir -p {c} && cp -a {}/. {c}",
        saved.path().display(),
        c = config.display()
    ));
    expect(&checks, false, "with the config back");

    sh(&format!(
        "cp -a /var/lib/archix /var/tmp/archix-state-saved && rm -rf /var/lib/archix/* {} {}",
        config.display(),
        binary.display()
    ));
    expect(&checks, true, "after deleting all three");
    sh(
        "rm -rf /var/lib/archix/* && cp -a /var/tmp/archix-state-saved/. /var/lib/archix/ && rm -rf /var/tmp/archix-state-saved",
    );
    sh(&format!(
        "mkdir -p {c} && cp -a {}/. {c}",
        saved.path().display(),
        c = config.display()
    ));
    restore_binary();

    strict(false);
    let pending = machine.root.join("run/archix/keep-pending");
    fs::create_dir_all(pending.parent().unwrap()).unwrap();
    fs::write(&pending, "3\n").unwrap();
    let out = pacman(
        machine,
        &[],
        &[
            "-U",
            "--noconfirm",
            packages.build("archix-test-pending", "").to_str().unwrap(),
        ],
        None,
    );
    checks.check(
        !out.status.success()
            && text(&out).contains("rollback to snapshot 3 is pending; reboot, or `archix rollback --cancel`"),
        format!("a pending keep refuses pacman with strict mode off: {}", text(&out)),
    );
    let switched = setup.run(&["switch", "--noconfirm"]);
    checks.check(
        !switched.status.success() && text(&switched).contains("rollback to snapshot 3 is pending"),
        format!("and archix switch: {}", text(&switched)),
    );
    fs::remove_file(&pending).unwrap();
    remove_test_packages(machine);
    let _ = pacman(machine, &[OWN], &["-R", "--noconfirm", "archix"], None);
    restore_binary();
    checks.finish();
}

#[test]
#[ignore]
fn a_masked_or_shadowed_hook_is_reported_by_status_and_at_boot() {
    let _system = lock_system();
    let checks = Checks::default();
    let setup = Setup::new(&[], "");
    setup.first_switch();
    let machine = &setup.machine;
    checks.check(
        !setup.ok(&["status"]).contains("archix's pacman hook"),
        "nothing to report after a switch",
    );
    let etc = machine.root.join("etc/pacman.d/hooks");
    fs::create_dir_all(&etc).unwrap();
    std::os::unix::fs::symlink("/dev/null", etc.join("01-archix-snapshot.hook")).unwrap();
    fs::write(etc.join("95-archix-post.hook"), "[Trigger]\n").unwrap();
    let status = setup.ok(&["status"]);
    checks.check(
        status.contains("warning: archix's pacman hook 01-archix-snapshot.hook is masked by /etc/pacman.d/hooks/01-archix-snapshot.hook, so plain pacman runs take no snapshot"),
        format!("status names a mask: {status}"),
    );
    checks.check(
        status.contains("95-archix-post.hook is shadowed by /etc/pacman.d/hooks/95-archix-post.hook"),
        format!("and a shadow: {status}"),
    );
    let boot = setup.ok(&["boot-check"]);
    checks.check(
        boot.contains("archix: archix's pacman hook 01-archix-snapshot.hook is masked"),
        format!("so does every boot: {boot}"),
    );
    let notice = setup.ok(&["notice"]);
    checks.check(
        notice.contains("01-archix-snapshot.hook is masked"),
        format!("and the login notice: {notice}"),
    );
    fs::remove_dir_all(&etc).unwrap();
    let system = machine.root.join("usr/share/libalpm/hooks/00-archix-strict.hook");
    fs::remove_file(&system).unwrap();
    std::os::unix::fs::symlink("/dev/null", &system).unwrap();
    let status = setup.ok(&["status"]);
    checks.check(
        status.contains("00-archix-strict.hook is a link to /dev/null"),
        format!("a masked hook in place: {status}"),
    );
    setup.set_body(&["tree"], "");
    setup.ok(&["switch", "--noconfirm"]);
    checks.check(
        fs::read_to_string(&system).is_ok_and(|text| text.contains("AbortOnFail")),
        "the next switch writes it again rather than through the link",
    );
    checks.check(
        !setup.ok(&["status"]).contains("archix's pacman hook"),
        "and the warning goes",
    );
    checks.finish();
}

#[test]
#[ignore]
fn a_switch_cut_off_part_way_is_marked_unfinished_after_giving_the_current_entry_its_tries_and_the_slot_its_entry() {
    let _system = lock_system();
    let checks = Checks::default();
    let body = "rollback = { enable = true }, drift = \"keep\"";
    let setup = on_btrfs("3G", body);
    let machine = &setup.machine;
    setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    bless(&setup);
    let entries = machine.esp().join("loader/entries");
    fs::rename(
        entries.join("archix-current+2.conf"),
        entries.join("archix-current.conf"),
    )
    .unwrap();
    setup.set_body(
        &[],
        &format!("{body}, files = {{ [\"/etc/archix-cut-test\"] = {{ text = \"one\" }} }}"),
    );
    let out = setup.run_env(
        &[COUNTING, ("ARCHIX_TEST_CRASH", "switch-files")],
        &["switch", "--noconfirm"],
    );
    checks.check(
        !out.status.success() && machine.root.join("etc/archix-cut-test").exists(),
        format!("the switch dies once it has written the config's files: {}", text(&out)),
    );
    checks.check(
        machine.entries().contains(&"archix-current+2.conf".to_string()),
        format!(
            "the current entry got its two tries before the switch changed anything: {:?}",
            machine.entries()
        ),
    );
    let slot = snapshots(machine).last().unwrap().number;
    checks.check(
        machine.entries().contains(&format!("archix-snapshot-{slot}+1.conf")),
        format!(
            "and the switch's snapshot, the slot, has its entry: {:?}",
            machine.entries()
        ),
    );
    let marked = archix::pacman::hooks::unfinished(&setup.paths.state);
    checks.check(
        marked
            .as_ref()
            .is_some_and(|unfinished| unfinished.slot == Some(slot) && unfinished.switch.is_some()),
        format!("the switch is marked as unfinished, naming the slot: {marked:?}"),
    );
    let check = setup.run_env(
        &[
            COUNTING,
            ("ARCHIX_TEST_COUNTED_BOOT", "1"),
            ("ARCHIX_TEST_ENTRY", "archix-current+2.conf"),
        ],
        &["health-check"],
    );
    checks.check(
        !check.status.success()
            && text(&check).contains("failed its health check switch")
            && text(&check).contains("was cut off before it finished")
            && text(&check).contains(&format!("`archix rollback {slot}` keeps it")),
        format!(
            "so the boot after it fails its health check, naming the slot: {}",
            text(&check)
        ),
    );
    let out = setup.run_env(&[COUNTING], &["switch", "--noconfirm"]);
    checks.check(
        out.status.success() && archix::pacman::hooks::unfinished(&setup.paths.state).is_none(),
        format!("a switch that finishes drops the mark: {}", text(&out)),
    );
    checks.finish();
}

#[test]
#[ignore]
fn a_switch_that_changes_nothing_before_failing_leaves_no_mark() {
    let _system = lock_system();
    let checks = Checks::default();
    let body = "rollback = { enable = true }, drift = \"keep\"";
    let setup = on_btrfs("3G", body);
    setup.ok_env(&[COUNTING], &["switch", "--noconfirm"]);
    bless(&setup);
    setup.set_body(
        &[],
        &format!("{body}, files = {{ [\"/etc/archix-cut-test\"] = {{ text = \"two\" }} }}"),
    );
    let out = setup.run_env(
        &[COUNTING, ("ARCHIX_TEST_FAIL", "switch-apply")],
        &["switch", "--noconfirm"],
    );
    checks.check(
        !out.status.success() && text(&out).contains("before anything was changed"),
        format!("the apply fails before its first step: {}", text(&out)),
    );
    let marked = archix::pacman::hooks::unfinished(&setup.paths.state);
    checks.check(marked.is_none(), format!("so nothing is marked unfinished: {marked:?}"));
    checks.finish();
}

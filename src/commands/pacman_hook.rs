use std::fs;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::process::ExitCode;

use archix::generations::Generation;
use archix::pacman::hooks::{self, Invocation, LowSpace, Pending, Verdict};
use archix::paths::Paths;
use archix::snapshot::{self, Kind, Policy, Store};
use archix::system;

/// Which side of the transaction the hook runs on.
#[derive(Clone, Copy, Debug, PartialEq, clap::ValueEnum)]
pub enum When {
    Pre,
    Post,
    /// Run by the step after a transaction that removed archix, from the copy the removal hook made.
    Remove,
}

/// What to do when the transaction leaves the root short of space.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Choice {
    Abort,
    Prune,
    StillTry,
    Skip,
}

/// Runs a hook step with the transaction's targets on stdin; archix's own transactions pass straight through. Only an abort for space fails, so nothing else here can stop pacman.
pub fn run(when: When) -> ExitCode {
    if std::env::var_os(archix::packages::TRANSACTION_ENV).is_some() {
        return ExitCode::SUCCESS;
    }
    let targets: Vec<String> = io::stdin()
        .lock()
        .lines()
        .map_while(Result::ok)
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();
    let paths = Paths::system();
    let pacman = std::os::unix::process::parent_id();
    match when {
        When::Remove => {
            remove(&paths);
            ExitCode::SUCCESS
        }
        When::Pre => pre(&paths, pacman, &targets),
        When::Post => {
            post(&paths, pacman, &targets);
            if let Err(e) = hooks::finished(&paths.state) {
                println!("archix: warning: {e:#}; the next boot won't count as good until a transaction finishes");
            }
            ExitCode::SUCCESS
        }
    }
}

/// After pacman removed archix: takes its integration off and hands the ESP to plain systemd-boot, saying what it did; never fails, as pacman's transaction is done.
fn remove(paths: &Paths) {
    let removed = system::removal::remove_integration(paths);
    println!(
        "archix: archix was removed; took off {} of its files (units, the initcpio hook, pacman hooks)",
        removed.len()
    );
    if system::live::Root::new(&paths.root).running() {
        let _ = std::process::Command::new("systemctl").arg("daemon-reload").status();
    }
    let esp = fs::read_to_string(paths.state.join("boot").join("esp"))
        .ok()
        .map(|esp| esp.trim().to_string())
        .filter(|esp| !esp.is_empty());
    let plan = current(paths).and_then(|generation| generation.settings.boot);
    let main = plan
        .as_ref()
        .map_or_else(|| "linux".to_string(), |plan| plan.kernel.clone());
    let loader = plan
        .as_ref()
        .map_or(system::boot::Loader::SystemdBoot, |plan| plan.loader);
    // Only a systemd-boot machine is handed to plain systemd-boot; on GRUB or Limine nothing of that loader's is touched.
    if let Some(esp) = esp.as_deref().filter(|_| loader != system::boot::Loader::SystemdBoot) {
        match system::removal::leave_for_other_loader(&paths.root, Path::new(esp), loader) {
            Ok(lines) => lines.iter().for_each(|line| println!("archix: {line}")),
            Err(e) => println!("archix: warning: couldn't take archix's own entries off the ESP: {e:#}"),
        }
    } else if let Some(esp) = esp {
        let esp = Path::new(&esp);
        let enforcing = system::secureboot::Machine::under(&paths.root)
            .state()
            .is_some_and(|state| state.enabled);
        let keys = system::boot::sbctl_has_keys();
        if enforcing && !keys {
            println!(
                "archix: warning: Secure Boot is on and sbctl has no keys to sign plain systemd-boot's UKIs with, so archix's boot entries stay as they are; they boot this kernel until the next kernel update. Sign a UKI for systemd-boot (mkinitcpio's preset with default_uki, then sbctl sign) and drop archix's entries from {}/loader/entries",
                esp.display()
            );
        } else {
            match system::removal::hand_over(paths, esp, &main, keys) {
                Ok(lines) => {
                    for line in lines {
                        println!("archix: {line}");
                    }
                }
                Err(e) => println!(
                    "archix: warning: couldn't hand the ESP to plain systemd-boot ({e:#}), so archix's boot entries stay as they are; they boot this kernel until the next kernel update"
                ),
            }
        }
    }
    let store = Store::open(&paths.root);
    let snapshots = store.dir();
    println!(
        "archix: archix's snapshots stay in {} as plain btrfs subvolumes; delete them with `sudo btrfs subvolume delete {}/*/root` once you don't need them",
        snapshots.display(),
        snapshots.display()
    );
    let _ = fs::remove_file(paths.root.join(system::removal::COPY.trim_start_matches('/')));
}

fn current(paths: &Paths) -> Option<Generation> {
    let store = archix::generations::Store::new(paths);
    store.get(store.current().ok()??).ok()
}

/// The policy when rollbacks are on and the root is laid out for them, else none.
fn policy(paths: &Paths, current: &Generation) -> Option<Policy> {
    let policy = current.settings.rollback.clone()?;
    (snapshot::unsupported(&paths.root).is_none() && super::snapshot::archix_mounted(&paths.root)).then_some(policy)
}

fn pre(paths: &Paths, pacman: u32, targets: &[String]) -> ExitCode {
    let invocation = Invocation::parse(&fs::read(format!("/proc/{pacman}/cmdline")).unwrap_or_default());
    let triggers = hooks::initramfs_triggers(&paths.root);
    let pending = Pending {
        pacman,
        initramfs: hooks::touches_initramfs(&paths.local_db(), targets, &triggers, false),
    };
    if let Err(e) = hooks::save_pending(&paths.root, &pending) {
        println!("archix: couldn't note this transaction for the step after it: {e:#}");
    }
    if system::initrd::trying().is_some() {
        return ExitCode::SUCCESS;
    }
    let Some(current) = current(paths) else {
        return ExitCode::SUCCESS;
    };
    if current.settings.rollback.is_none() {
        return ExitCode::SUCCESS;
    }
    if let Some(why) = snapshot::foreign_root(&paths.root) {
        println!("archix: warning: no snapshot before this transaction: {why}");
        return ExitCode::SUCCESS;
    }
    let Some(policy) = policy(paths, &current) else {
        return ExitCode::SUCCESS;
    };
    let estimate = hooks::estimate(&paths.pacman_db, targets, &invocation);
    let needed = estimate + hooks::margin(estimate);
    let mut take = true;
    if let Some(space) = system::btrfs::space(&paths.root)
        && let Verdict::Short { still_try } = hooks::verdict(space, needed, policy.min_free)
    {
        let left = hooks::after(space, needed);
        let warning = format!(
            "archix: warning: this transaction needs about {} MiB with the snapshot that keeps what it replaces, and the root has {} MiB free; after it {:.0}% would be left, below rollback.min_free = {}%{}",
            needed >> 20,
            space.bytes >> 20,
            left.percent,
            policy.min_free,
            match still_try {
                true => String::new(),
                false => format!(" and the {} MiB kept for a rollback", snapshot::KEEP_RESERVE >> 20),
            }
        );
        let asked = ask(&warning, still_try, invocation.noconfirm);
        if asked.is_none() {
            println!("{warning}");
        }
        let choice = asked.unwrap_or(match policy.low_space {
            LowSpace::Abort => Choice::Abort,
            LowSpace::Prune => Choice::Prune,
            LowSpace::Skip => Choice::Skip,
        });
        match choice {
            Choice::Abort => {
                return abort(
                    paths,
                    "aborted the transaction: the root is short of space for it and its snapshot",
                );
            }
            Choice::StillTry => hooks::log(
                &paths.state,
                &format!(
                    "a pacman transaction went ahead with its snapshot {} MiB short of rollback.min_free",
                    needed >> 20
                ),
            ),
            Choice::Skip => {
                hooks::log(
                    &paths.state,
                    "a pacman transaction ran without a snapshot, as rollback.low_space = \"skip\" says when the root is short of space",
                );
                println!("archix: no snapshot before this transaction (rollback.low_space = \"skip\")");
                take = false;
            }
            Choice::Prune => {
                let Some(safe) = safe_point(paths, &current) else {
                    return abort(
                        paths,
                        "aborted the transaction: the root is short of space, and pruning snapshots needs a safe recovery point to stay (a snapshot of a system that booted fine, with its boot entry), which there isn't",
                    );
                };
                let pinned = system::menu::pinned(paths, policy.entries);
                let freed = snapshot::prune_root(paths, &policy, &current.packages, &pinned, || {
                    system::btrfs::space(&paths.root).map(|space| hooks::after(space, needed))
                });
                let fits = system::btrfs::space(&paths.root).is_some_and(|space| {
                    hooks::verdict(space, needed, policy.min_free) != Verdict::Short { still_try: false }
                });
                let numbers: Vec<String> = freed.deleted.iter().map(u32::to_string).collect();
                let what = match numbers.is_empty() {
                    true => "no snapshot could go".to_string(),
                    false => format!("deleted snapshots {}", numbers.join(", ")),
                };
                if !fits {
                    return abort(
                        paths,
                        &format!(
                            "aborted the transaction: the root is short of space even after pruning ({what}; {safe} stays)"
                        ),
                    );
                }
                hooks::log(
                    &paths.state,
                    &format!("pruned for a pacman transaction short of space: {what}; {safe} stays"),
                );
                println!("archix: {what} to make room; {safe} stays");
            }
        }
    }
    if take {
        snapshot_before(paths, &current);
    }
    count_before(paths, pacman, &current, &policy);
    ExitCode::SUCCESS
}

/// Gives the current entry its tries and the slot its entry before the transaction changes anything, so a boot that a transaction cut off half way breaks still falls back, then marks the transaction as running, so such a boot doesn't count as good.
fn count_before(paths: &Paths, pacman: u32, current: &Generation, policy: &Policy) {
    let Some(plan) = current.settings.boot.as_ref() else {
        return;
    };
    if plan.loader == system::boot::Loader::Limine {
        return;
    }
    if let Err(e) = system::menu::refresh(paths, plan, Some(policy.entries), true) {
        println!(
            "archix: warning: the boot menu wasn't written before this transaction, so a boot it breaks may not fall back: {e:#}"
        );
        return;
    }
    let unfinished = hooks::Unfinished {
        pacman,
        started: archix::generations::now(),
        slot: system::menu::load(&system::menu::dir(paths))
            .ok()
            .and_then(|state| state.slot),
        switch: None,
    };
    if let Err(e) = hooks::mark_unfinished(&paths.state, &unfinished) {
        println!(
            "archix: warning: this transaction isn't marked as running, so a power cut during it may leave a torn system that counts as good: {e:#}"
        );
    }
}

fn abort(paths: &Paths, why: &str) -> ExitCode {
    hooks::log(&paths.state, why);
    println!(
        "archix: {why}; nothing was changed. Free space, delete snapshots with `archix snapshot delete N`, or set rollback.low_space"
    );
    ExitCode::FAILURE
}

/// Shows `warning` and asks on the terminal, when there is one and pacman wasn't told --noconfirm.
fn ask(warning: &str, still_try: bool, noconfirm: bool) -> Option<Choice> {
    if noconfirm {
        return None;
    }
    let tty = fs::OpenOptions::new().read(true).write(true).open("/dev/tty").ok()?;
    let mut out = tty.try_clone().ok()?;
    let prompt = match still_try {
        true => "archix: [a]bort, [p]rune old snapshots, or [s]till try (snapshot and install anyway)? [a] ",
        false => "archix: [a]bort, or [p]rune old snapshots? [a] ",
    };
    write!(out, "{warning}\n{prompt}").ok()?;
    out.flush().ok()?;
    let mut answer = String::new();
    io::BufReader::new(tty).read_line(&mut answer).ok()?;
    Some(match answer.trim().to_lowercase().as_str() {
        "p" | "prune" => Choice::Prune,
        "s" | "still" | "still try" if still_try => Choice::StillTry,
        _ => Choice::Abort,
    })
}

/// A snapshot that holds a system which booted fine and has its boot entry, so a fallback always has somewhere to go: the slot, or, after a boot that counted as good, the snapshot this transaction takes, which becomes the slot.
fn safe_point(paths: &Paths, current: &Generation) -> Option<String> {
    let plan = current.settings.boot.as_ref()?;
    if plan.loader == system::boot::Loader::Limine {
        return None;
    }
    let state = system::menu::load(&system::menu::dir(paths)).ok()?;
    state.current.main.as_ref()?;
    if state.armed {
        return Some("this transaction's snapshot, the fallback slot of a system that booted fine,".to_string());
    }
    let slot = state.slot?;
    let exists = Store::open(&paths.root).get(slot).ok()?.is_some();
    (exists && state.snapshots.contains_key(&slot) && !state.failed.contains(&slot))
        .then(|| format!("snapshot {slot}, the fallback slot,"))
}

fn snapshot_before(paths: &Paths, current: &Generation) {
    let taken = Store::open(&paths.root)
        .create(
            &paths.root,
            Kind::Pacman,
            super::snapshot::contents(Some(current), "before pacman transaction".to_string(), None),
        )
        .and_then(|record| system::menu::snapshot_taken(paths, record.number).map(|()| record));
    match taken {
        Ok(record) => println!(
            "archix: snapshot {} holds the system as it was before this transaction",
            record.number
        ),
        Err(e) => println!("archix: warning: no snapshot before this transaction: {e:#}"),
    }
}

fn post(paths: &Paths, pacman: u32, targets: &[String]) {
    // pacman doesn't flush what it extracted, so a power cut while the images build would lose it.
    archix::durable::sync_filesystems(&[&paths.root]);
    match archix::integration::record(paths, targets.to_vec()) {
        Ok(Some(change)) => println!(
            "archix: recorded {} changed outside archix; `archix switch` handles them as the config's drift policy says",
            super::count(change.packages.len(), "package", "packages")
        ),
        Ok(None) => {}
        Err(e) => println!("archix: could not record this transaction: {e:#}"),
    }
    let pending = hooks::take_pending(&paths.root, pacman);
    if system::initrd::trying().is_some() || snapshot::foreign_root(&paths.root).is_some() {
        return;
    }
    let Some(current) = current(paths) else { return };
    let Some(plan) = current.settings.boot.clone() else {
        return;
    };
    if plan.loader == system::boot::Loader::Limine {
        return;
    }
    let triggers = hooks::initramfs_triggers(&paths.root);
    let initramfs = pending.as_ref().is_some_and(|pending| pending.initramfs)
        || hooks::touches_initramfs(&paths.local_db(), targets, &triggers, pending.is_none());
    let root = system::live::Root::new(&paths.root);
    let mut built = true;
    if initramfs {
        println!("archix: the kernel or initramfs changed; building the boot images");
        let result = system::boot::build(paths, &plan, &root).and_then(|images| {
            let dir = system::menu::dir(paths);
            let mut state = system::menu::load(&dir)?;
            state.current = images;
            system::menu::save(&dir, &state)?;
            archix::testhooks::mark("boot-image-recorded");
            Ok(())
        });
        if let Err(e) = result {
            built = false;
            println!(
                "archix: warning: the boot images weren't built, so the boot menu still starts the old ones: {e:#}"
            );
        }
    }
    let policy = policy(paths, &current);
    if let Some(policy) = &policy {
        let pinned = system::menu::pinned(paths, policy.entries);
        let freed = snapshot::prune_root(paths, policy, &current.packages, &pinned, || {
            system::btrfs::space(&paths.root)
        });
        if !freed.deleted.is_empty() {
            let numbers: Vec<String> = freed.deleted.iter().map(u32::to_string).collect();
            println!("archix: deleted snapshots {}", numbers.join(", "));
        }
        for warning in freed.warnings {
            println!("archix: warning: {warning}");
        }
    }
    let cap = policy.as_ref().map(|policy| policy.entries);
    match system::esp::clean_esp(Path::new(&plan.esp)).and_then(|()| system::menu::refresh(paths, &plan, cap, true)) {
        Ok(()) if plan.tries > 0 => println!(
            "archix: the boot menu is up to date, and the current system's entry has {} tries again{}",
            plan.tries,
            match built {
                true => "",
                false => ", though with its old image",
            }
        ),
        Ok(()) => {}
        Err(e) => println!("archix: warning: the boot menu wasn't written: {e:#}"),
    }
    if initramfs && let Err(e) = system::rescue::refresh(paths, &plan, &root) {
        println!("archix: warning: the rescue entry wasn't refreshed: {e:#}");
    }
}

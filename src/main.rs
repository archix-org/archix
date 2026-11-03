use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;

mod commands;

use archix::eval::{self, Evaluation, Input};
use archix::options::{self, Def};

#[derive(Parser)]
#[command(name = "archix", version, about = "Declarative, lockable Arch Linux configuration")]
struct Cli {
    /// Config directory containing init.lua
    #[arg(long, global = true, env = "ARCHIX_CONFIG", default_value = "/etc/archix")]
    config: PathBuf,
    /// Evaluate as this host instead of the machine's hostname
    #[arg(long, global = true)]
    host: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Write a config describing this machine as it is, locked to exactly what's installed
    Init {
        /// Take every question's default without asking
        #[arg(long)]
        defaults: bool,
        /// Rebuild this package, which no repo or the AUR has, as a recipe from its installed files; its files stay out of the config and package caches
        #[arg(long, value_name = "PACKAGE")]
        repack: Vec<String>,
    },
    /// Print the evaluated configuration as JSON
    Eval,
    /// List every declared option
    Options {
        #[arg(long)]
        json: bool,
    },
    /// Resolve the declared packages into archix.lock, keeping the locked snapshot date and AUR commits
    Lock {
        /// Don't ask before trusting new AUR build recipes
        #[arg(long)]
        noconfirm: bool,
    },
    /// Move archix.lock to the newest archive snapshot (or --to a date) and re-resolve
    Update {
        /// Snapshot date to move to, as YYYY-MM-DD
        #[arg(long)]
        to: Option<archix::date::Date>,
        /// Don't ask before trusting new AUR build recipes
        #[arg(long)]
        noconfirm: bool,
    },
    /// Show what a switch would change to make this machine match the lock, and what it carries that the config doesn't declare
    Diff {
        /// List everything carried but not declared, not only the first lines of each kind
        #[arg(long)]
        all: bool,
    },
    /// Apply the lock and the config to the running system, in place
    Switch {
        /// Don't ask for confirmation
        #[arg(long)]
        noconfirm: bool,
        /// Go back across versions that may not read newer data (postgresql majors and the like) without asking
        #[arg(long)]
        allow_unsafe_downgrade: bool,
        /// When a locked AUR version can't be built, take the nearest newer or older one without asking
        #[arg(long, value_enum)]
        accept_nearest: Option<commands::Nearest>,
        /// Commit even though secrets don't decrypt on this machine yet: those are left out, and what uses them fails until a switch after `archix secrets rekey`
        #[arg(long)]
        without_secrets: bool,
        /// Downgrade declared packages even though that breaks packages archix doesn't track, by their dependencies or the libraries they load
        #[arg(long)]
        allow_partial: bool,
    },
    /// Gone: a switch that fails partway says what it applied
    #[command(hide = true)]
    Recover {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        rest: Vec<String>,
    },
    /// Show the lock, the current and booted generations, pending changes and what the machine carries that the config doesn't declare
    Status {
        /// List everything carried but not declared, not only the first lines of each kind
        #[arg(long)]
        all: bool,
    },
    /// Apply an earlier generation's record in place (the previous one by default): its packages from archix's cache and the archive, its files, users and services; the config and archix.lock stay as they are
    Rollback {
        /// Generation number; `archix generations` lists them
        generation: Option<u32>,
        /// Don't ask for confirmation
        #[arg(long)]
        noconfirm: bool,
        /// Go back across versions that may not read newer data (postgresql majors and the like) without asking
        #[arg(long)]
        allow_unsafe_downgrade: bool,
    },
    /// List generations, the current one marked with *, then the snapshots by the state each holds
    Generations,
    /// Delete old generation records and cached files no kept generation or snapshot needs
    Gc,
    /// Declare packages in installed.lua and install them (pacman-style: archix -S <packages>)
    Install {
        #[arg(required = true)]
        packages: Vec<String>,
        /// Directory under the config's lua/ whose installed.lua to edit, instead of lua/hosts/<host>/
        #[arg(long)]
        path: Option<PathBuf>,
        /// Don't ask for confirmation
        #[arg(long)]
        noconfirm: bool,
        #[arg(skip)]
        upgrade_first: bool,
    },
    /// Remove packages from installed.lua and uninstall them (pacman-style: archix -R <packages>)
    #[command(short_flag = 'R')]
    Remove {
        #[arg(required = true)]
        packages: Vec<String>,
        /// Directory under the config's lua/ whose installed.lua to edit
        #[arg(long)]
        path: Option<PathBuf>,
        /// Don't ask for confirmation
        #[arg(long)]
        noconfirm: bool,
    },
    /// Search the repositories and the AUR (pacman-style: archix -Ss <terms>)
    Search {
        #[arg(required = true)]
        terms: Vec<String>,
    },
    /// Move archix.lock to the newest snapshot and AUR packages to their newest recipes, then switch (also: archix, archix -Syu)
    Upgrade {
        /// Don't ask for confirmation
        #[arg(long)]
        noconfirm: bool,
    },
    /// pacman-style operations: -S <packages>, -Ss <terms>, -Syu [packages]
    #[command(short_flag = 'S', hide = true)]
    Sync {
        #[arg(short = 's')]
        search: bool,
        #[arg(short = 'y', action = clap::ArgAction::Count)]
        refresh: u8,
        #[arg(short = 'u', action = clap::ArgAction::Count)]
        upgrade: u8,
        #[arg(long)]
        noconfirm: bool,
        #[arg(long)]
        needed: bool,
        targets: Vec<String>,
    },
    /// Download a file and print the archix.fetchurl { url, sha256 } line to paste into the config
    Prefetch { url: String },
    /// Repair a machine from the rescue boot entry, which doesn't mount the root; with no command, the rescue prompt
    Rescue {
        #[command(flatten)]
        options: commands::rescue::Options,
        #[command(subcommand)]
        action: Option<commands::rescue::Action>,
    },
    /// Run at boot by archix-boot.service: the boot's own checks (the hibernation guard, failed entries, the try notice, a root archix didn't produce)
    #[command(hide = true)]
    BootCheck,
    /// Run by archix-boot-complete.service once systemd-boot has counted the boot as good: arm the fallback slot and note the failed units
    #[command(hide = true)]
    BootComplete,
    /// Run in the initramfs: mount the snapshot the started entry names over the root, read-only with a throwaway layer
    #[command(hide = true)]
    BootMount { root: PathBuf },
    /// Run every minute of a counted boot by archix-unblessed.timer: reboot it once it's stuck past boot.unblessed_timeout with nobody logged in
    #[command(hide = true)]
    Unblessed,
    /// Print the try-mode notice, if this boot is one; with --desktop, send it as a desktop notification
    #[command(hide = true)]
    Notice {
        #[arg(long)]
        desktop: bool,
    },
    /// Adopt a root archix didn't produce (a snapper or limine-snapper-sync rollback, a restore): check its layout, carry accounts over from the root it replaced when that can be found, and record it
    Resync,
    /// Run by systemd-bless-boot.service, before its own rename: count the booted entry as good, synced under its new name
    #[command(hide = true)]
    Bless,
    /// Run by archix-health-check.service before boot-complete.target: check a counted boot, and mark its entry bad if it fails
    #[command(hide = true)]
    HealthCheck,
    /// Run by archix's pacman hooks around a transaction archix didn't start: `pre` snapshots the root, `post` rebuilds the boot image if the kernel or initramfs changed, resets boot counting and records the packages
    #[command(hide = true)]
    PacmanHook {
        #[arg(value_enum)]
        when: commands::pacman_hook::When,
    },
    /// Run at boot before a swap file declared with a size is swapped on: make it unless it's there with that size
    #[command(hide = true)]
    Swapfile { path: PathBuf, bytes: u64 },
    /// Run by emergency.service before sulogin where the config declares required mounts: say which didn't mount and what to do
    #[command(hide = true)]
    EmergencyHint {
        #[arg(long)]
        no_rescue_entry: bool,
    },
    /// Set up and check Secure Boot
    #[command(subcommand)]
    SecureBoot(SecureBoot),
    /// Share package files and AUR builds with other machines through a package cache
    #[command(subcommand)]
    Cache(Cache),
    /// Declared secrets: this machine's key, editing, setting, re-encrypting and checking them
    #[command(subcommand)]
    Secrets(Secrets),
    /// The root's btrfs snapshots: taking, listing and deleting them, and laying the root out for rollbacks
    #[command(subcommand)]
    Snapshot(SnapshotAction),
    /// Repair from a live USB: mount the root on a device as it boots, @archix, @varlib and the binds from /var/rollback included, and arch-chroot into it
    Chroot {
        /// The root's btrfs device, such as /dev/nvme0n1p2 or an opened /dev/mapper/root
        device: String,
        /// The command to run inside, a shell by default
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Run at boot by archix-split-var-lib.service, before /var/lib is mounted: move it into its own subvolume
    #[command(hide = true)]
    SplitVarLib,
    /// Run by a root command under a transient uid: evaluate a captured config tree and print its plan
    #[command(hide = true)]
    EvaluateIsolated {
        #[arg(long)]
        dir: PathBuf,
        #[arg(long)]
        host: String,
    },
    /// Run by root as a directory's owner: write stdin to NAME in DIR without following a symlink
    #[command(hide = true)]
    WriteFile { dir: PathBuf, name: std::ffi::OsString },
    /// `archix <terms>`, like `yay <terms>`: search, pick from a numbered list, install the picks
    #[command(external_subcommand)]
    Pick(Vec<String>),
}

#[derive(Subcommand)]
enum Secrets {
    /// Make this machine's age identity in /var/lib/archix/secrets and print the key for secrets.machine_key
    Init {
        /// Make a new identity even though one exists; the old one is retired, kept while a kept generation needs it
        #[arg(long)]
        replace: bool,
    },
    /// Print this machine's age recipient
    Pubkey,
    /// Decrypt a secret into an editor on a private ramfs and encrypt what you save to every recipient the config names
    Edit {
        name: String,
        /// Decrypt with this machine's own key (root) instead of yours
        #[arg(long)]
        machine: bool,
    },
    /// Encrypt a value from --file or stdin as a secret, for piping from a password manager
    Set {
        name: String,
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Re-encrypt secrets (all by default) to the recipients every host's config names now, as after adding a host or an admin
    Rekey {
        names: Vec<String>,
        /// Decrypt with this machine's own key and its retired ones (root) instead of yours
        #[arg(long)]
        machine: bool,
    },
    /// Show for each secret who it should be encrypted to against who it was, whether this machine decrypts it, and what uses it
    Check,
    /// Make a new machine identity; the old one is retired until no kept generation needs it (gc drops it then)
    RotateMachineKey {
        /// The old key leaked: delete it at once instead of retiring it
        #[arg(long)]
        compromised: bool,
    },
    /// Run at boot by archix-secrets.service: decrypt the config's secrets into /run/archix/secrets
    #[command(hide = true)]
    Activate {
        #[arg(long, default_value = "/")]
        root: PathBuf,
    },
}

#[derive(Subcommand)]
enum SnapshotAction {
    /// Take a read-only snapshot of the root now; prune never drops it
    Create {
        /// A note to keep with it, shown by `archix snapshot list`
        #[arg(long)]
        note: Option<String>,
    },
    /// List the snapshots, oldest first, with what took each
    List,
    /// Delete a snapshot
    Delete { number: u32 },
    /// Lay the root out for rollbacks: /.archix and /var/lib as subvolumes of their own, with pacman's database, DKMS's and archix's state kept on the root; /var/lib moves at the next boot
    Setup,
}

#[derive(Subcommand)]
enum Cache {
    /// Copy this machine's package files, signatures and AUR build records into a directory other machines list in package_caches
    Push {
        /// The cache directory, such as a NAS mount; serve it over HTTP to share it beyond the network share
        dir: PathBuf,
    },
}

#[derive(Subcommand)]
enum SecureBoot {
    /// Create sbctl keys if needed and enroll them, with Microsoft's, while the firmware is in setup mode
    Setup {
        /// Leave Microsoft's keys out; GPU option ROMs and Windows may then fail to start, and some boards won't boot at all
        #[arg(long)]
        no_microsoft: bool,
    },
    /// Show whether Secure Boot is on and keys are enrolled
    Check {
        /// Also check what Easy Anti-Cheat Linux v2 requires of the boot
        #[arg(long)]
        anticheat: bool,
    },
    /// Experimental, tested on OVMF only: add the boot images archix signed and no longer uses to dbx, signed with your KEK, so the firmware refuses them
    Revoke {
        /// List them without writing dbx
        #[arg(long)]
        dry_run: bool,
        /// Don't ask before writing dbx
        #[arg(long)]
        noconfirm: bool,
    },
    /// Replace systemd's PCR policy signing key in /etc and bind the TPM2 slot of each LUKS volume given to the new one; the next switch builds UKIs whose policies it signs, and UKIs built before no longer unlock those volumes
    RotatePcrKey {
        /// LUKS2 volumes to re-enroll, such as /dev/nvme0n1p2; systemd-cryptenroll asks for each one's passphrase or recovery key, or reads $PASSWORD
        devices: Vec<PathBuf>,
    },
}

/// Turns pacman-style `-S` operations into the native command they mean.
fn from_sync(command: Command) -> Result<Command> {
    let Command::Sync {
        search,
        refresh,
        upgrade,
        noconfirm,
        targets,
        ..
    } = command
    else {
        return Ok(command);
    };
    Ok(match (search, upgrade > 0, targets.is_empty()) {
        (true, _, false) => Command::Search { terms: targets },
        (true, _, true) => anyhow::bail!("-Ss needs search terms"),
        (false, true, true) => Command::Upgrade { noconfirm },
        (false, upgrade_first, false) => Command::Install {
            packages: targets,
            path: None,
            noconfirm,
            upgrade_first,
        },
        (false, false, true) if refresh > 0 => {
            anyhow::bail!(
                "archix doesn't sync databases on their own; use -Syu to upgrade, or `archix update` to move the lock"
            )
        }
        (false, false, true) => anyhow::bail!("-S needs packages to install"),
    })
}

#[derive(Serialize)]
struct OptionDoc {
    path: String,
    r#type: String,
    default: Option<archix::value::Value>,
    description: Option<String>,
    declared_in: String,
}

fn main() -> ExitCode {
    // Rust ignores SIGPIPE by default, which turns `archix diff | head` into a panic instead of a quiet exit.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    // archix reads pacman's and systemctl's messages, so they must be untranslated; the user's character set stays.
    unsafe {
        if let Some(all) = std::env::var_os("LC_ALL") {
            std::env::set_var("LANG", all);
            std::env::remove_var("LC_ALL");
        }
        std::env::set_var("LC_MESSAGES", "C");
        std::env::remove_var("LANGUAGE");
    }
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            let denied = e.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied)
                    || cause.to_string().contains("Permission denied")
            });
            if denied && !is_root() {
                eprintln!("hint: that needs root; run it with sudo: {}", with_sudo());
            }
            ExitCode::FAILURE
        }
    }
}

fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// The command line as typed, with sudo in front.
fn with_sudo() -> String {
    let mut args = std::env::args();
    let program = args
        .next()
        .and_then(|arg0| {
            PathBuf::from(arg0)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "archix".to_string());
    std::iter::once(format!("sudo {program}"))
        .chain(args)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Commands that change the system, checked before anything (like installed.lua) is touched.
fn needs_root(command: &Command) -> Option<&'static str> {
    match command {
        Command::Install { .. } => Some("install"),
        Command::Remove { .. } => Some("remove"),
        Command::Switch { .. } => Some("switch"),
        Command::Upgrade { .. } => Some("upgrade"),
        Command::Rollback { .. } => Some("rollback"),
        Command::Gc => Some("gc"),
        Command::Snapshot(SnapshotAction::Create { .. }) => Some("snapshot create"),
        Command::Snapshot(SnapshotAction::Delete { .. }) => Some("snapshot delete"),
        Command::Snapshot(SnapshotAction::Setup) => Some("snapshot setup"),
        Command::Resync => Some("resync"),
        Command::Secrets(Secrets::Init { .. }) => Some("secrets init"),
        Command::Secrets(Secrets::RotateMachineKey { .. }) => Some("secrets rotate-machine-key"),
        Command::Secrets(Secrets::Edit { machine: true, .. }) => Some("secrets edit --machine"),
        Command::Secrets(Secrets::Rekey { machine: true, .. }) => Some("secrets rekey --machine"),
        Command::Init { .. } => Some("init"),
        Command::SecureBoot(SecureBoot::Setup { .. }) => Some("secure-boot setup"),
        Command::SecureBoot(SecureBoot::Revoke { .. }) => Some("secure-boot revoke"),
        Command::SecureBoot(SecureBoot::RotatePcrKey { .. }) => Some("secure-boot rotate-pcr-key"),
        _ => None,
    }
}

const DETACHED: &str = "ARCHIX_DETACHED";

/// Run from a login session, archix moves to a scope of its own, as NixOS runs its switch with systemd-run, so logind ending the session when an ssh connection drops doesn't stop it; it stays on the same terminal.
fn leave_login_session() {
    use std::os::unix::process::CommandExt;
    if std::env::var_os(DETACHED).is_some() || !std::path::Path::new("/run/systemd/system").exists() {
        return;
    }
    let in_session = fs::read_to_string("/proc/self/cgroup").is_ok_and(|cgroups| {
        cgroups
            .lines()
            .any(|line| line.contains("/session-") && line.ends_with(".scope"))
    });
    let Ok(exe) = std::env::current_exe() else { return };
    if !in_session {
        return;
    }
    let error = std::process::Command::new("systemd-run")
        .args(["--scope", "--quiet", "--collect"])
        .arg(format!("--unit=archix-{}", std::process::id()))
        .arg("--")
        .arg(exe)
        .args(std::env::args_os().skip(1))
        .env(DETACHED, "1")
        .exec();
    eprintln!("warning: couldn't move out of the login session ({error}), so ending the session could stop this run");
}

fn running_file() -> Result<fs::File> {
    let dir = archix::paths::Paths::system().state;
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("running");
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))
}

fn try_lock(file: &fs::File) -> bool {
    use std::os::fd::AsRawFd;
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// The lock that keeps two archix runs from changing the system at once; held until the process exits.
static RUNNING: std::sync::OnceLock<fs::File> = std::sync::OnceLock::new();

fn hold_exclusive() -> Result<()> {
    use std::io::Write as _;
    let path = archix::paths::Paths::system().state.join("running");
    let mut file = running_file()?;
    if !try_lock(&file) {
        let who = fs::read_to_string(&path).unwrap_or_default();
        anyhow::bail!(
            "another archix is already running ({}); wait for it to finish",
            who.trim()
        );
    }
    file.set_len(0)?;
    let args: Vec<String> = std::env::args().collect();
    write!(file, "pid {}, `{}`", std::process::id(), args.join(" "))?;
    let _ = RUNNING.set(file);
    Ok(())
}

fn run() -> Result<ExitCode> {
    // A switch by 0.1.0-dogfood that replaced archix itself left units saying `ExecStart=/usr/bin/archix (deleted) boot-check`.
    let cli = Cli::parse_from(std::env::args_os().filter(|arg| arg != "(deleted)"));
    let command = from_sync(cli.command.unwrap_or(Command::Upgrade { noconfirm: false }))?;
    // Both run before the root is fully up, so they skip archix's locks and pending-state checks.
    let command = match command {
        Command::EvaluateIsolated { dir, host } => {
            let ok = archix::eval::isolated::child(&dir, &host)?;
            return Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE });
        }
        Command::WriteFile { dir, name } => {
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut std::io::stdin(), &mut bytes)?;
            archix::captured::write_file_here(&dir, &name, &bytes)?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::Secrets(Secrets::Activate { root }) => {
            if !is_root() {
                anyhow::bail!("`archix secrets activate` runs at boot as root");
            }
            archix::secrets::activate::run(&root)?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::Swapfile { path, bytes } => {
            archix::system::fstab::make_swapfile(&path, bytes)?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::Chroot { device, command } => {
            if !is_root() {
                anyhow::bail!("`archix chroot` mounts filesystems; run it as root");
            }
            commands::snapshot::chroot(&device, &command)?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::SplitVarLib => {
            commands::snapshot::split_var_lib()?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::EmergencyHint { no_rescue_entry } => {
            archix::system::emergency::run(&archix::paths::Paths::system(), !no_rescue_entry);
            return Ok(ExitCode::SUCCESS);
        }
        Command::BootMount { root } => {
            let cmdline = archix::testhooks::kernel_cmdline();
            archix::system::initrd::mount(&root, &archix::testhooks::run_dir(), &cmdline)?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::PacmanHook { when } => {
            return Ok(commands::pacman_hook::run(when));
        }
        Command::Notice { desktop } => {
            commands::notice(desktop);
            return Ok(ExitCode::SUCCESS);
        }
        Command::Recover { .. } => anyhow::bail!(
            "`archix recover` is gone: a switch that fails partway says what it applied, and with rollbacks on a snapshot holds the system as it was before; run `archix switch` again"
        ),
        Command::Rescue { options, action } => {
            if !is_root() {
                anyhow::bail!("`archix rescue` mounts filesystems, so it needs root");
            }
            commands::rescue::run(options, action)?;
            return Ok(ExitCode::SUCCESS);
        }
        command => command,
    };
    if let Some(name) = needs_root(&command)
        && !is_root()
    {
        anyhow::bail!(
            "`archix {name}` changes the system, so it needs root; run it with sudo: {}",
            with_sudo()
        );
    }
    let changes_things = needs_root(&command).is_some()
        || matches!(
            command,
            Command::BootCheck | Command::BootComplete | Command::Bless | Command::HealthCheck | Command::Unblessed
        );
    if needs_root(&command).is_some() {
        leave_login_session();
    }
    if changes_things {
        hold_exclusive()?;
    }
    match command {
        Command::Rollback {
            generation,
            noconfirm,
            allow_unsafe_downgrade,
        } => {
            commands::rollback(generation, noconfirm, allow_unsafe_downgrade)?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::Generations => {
            commands::generations()?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::Gc => {
            commands::gc()?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::Secrets(Secrets::Init { replace }) => {
            println!(
                "{}",
                archix::secrets::tool::init(&archix::paths::Paths::system().state, replace)?
            );
            return Ok(ExitCode::SUCCESS);
        }
        Command::Secrets(Secrets::Pubkey) => {
            println!(
                "{}",
                archix::secrets::crypto::machine(&archix::paths::Paths::system().state)?.recipient
            );
            return Ok(ExitCode::SUCCESS);
        }
        Command::Secrets(Secrets::RotateMachineKey { compromised }) => {
            println!(
                "{}",
                archix::secrets::tool::rotate(&archix::paths::Paths::system().state, compromised)?
            );
            return Ok(ExitCode::SUCCESS);
        }
        Command::Cache(Cache::Push { dir }) => {
            commands::cache_push(&dir)?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::Snapshot(action) => {
            match action {
                SnapshotAction::Create { note } => commands::snapshot::create(note)?,
                SnapshotAction::List => commands::snapshot::list()?,
                SnapshotAction::Delete { number } => commands::snapshot::delete(number)?,
                SnapshotAction::Setup => commands::snapshot::setup()?,
            }
            return Ok(ExitCode::SUCCESS);
        }
        Command::Prefetch { url } => {
            commands::prefetch(&url)?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::BootCheck => {
            commands::boot_check()?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::BootComplete => {
            let paths = archix::paths::Paths::system();
            if archix::system::boot::booted() == Some(archix::system::menu::Entry::Current) {
                archix::system::emergency::forget(&paths);
            }
            archix::system::health::boot_complete(&paths)?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::Unblessed => {
            archix::system::health::unblessed()?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::Resync => {
            commands::snapshot::resync()?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::Bless => {
            archix::system::boot::bless_booted(&archix::paths::Paths::system())?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::HealthCheck => {
            let passed = archix::system::health::run_boot(&archix::paths::Paths::system())?;
            return Ok(if passed { ExitCode::SUCCESS } else { ExitCode::FAILURE });
        }
        Command::SecureBoot(SecureBoot::Setup { no_microsoft }) => {
            commands::secure_boot_setup(!no_microsoft)?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::SecureBoot(SecureBoot::Revoke { dry_run, noconfirm }) => {
            commands::secure_boot_revoke(dry_run, noconfirm)?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::SecureBoot(SecureBoot::RotatePcrKey { devices }) => {
            archix::system::boot::rotate_pcr_key(&archix::paths::Paths::system(), &devices)?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::SecureBoot(SecureBoot::Check { anticheat }) => {
            return Ok(if commands::secure_boot_check(anticheat) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            });
        }
        _ => {}
    }
    let hostname = match cli.host {
        Some(host) => host,
        None => system_hostname()?,
    };
    if let Command::Init { defaults, repack } = command {
        commands::init(&cli.config, &hostname, defaults, &repack)?;
        return Ok(ExitCode::SUCCESS);
    }
    if matches!(command, Command::Options { .. }) && !cli.config.join("init.lua").is_file() {
        let empty = std::env::temp_dir().join(format!("archix-options-{}", std::process::id()));
        fs::create_dir_all(&empty)?;
        fs::write(empty.join("init.lua"), "return {}\n")?;
        let evaluation = eval::evaluate(&Input {
            config_dir: empty.clone(),
            hostname: hostname.clone(),
            ..Default::default()
        });
        let _ = fs::remove_dir_all(&empty);
        let evaluation = evaluation.map_err(|errors| anyhow::anyhow!("{errors}"))?;
        if let Command::Options { json } = command {
            print!("{}", render_options(&evaluation, json)?);
        }
        return Ok(ExitCode::SUCCESS);
    }
    if !cli.config.join("init.lua").is_file() {
        anyhow::bail!(
            "there's no archix config at {} yet; run `sudo archix init` to write one that describes this machine, or point --config at yours",
            cli.config.display()
        );
    }
    // Nothing these commands change in the config is written until the switch has applied.
    if matches!(
        command,
        Command::Install { .. }
            | Command::Remove { .. }
            | Command::Switch { .. }
            | Command::Upgrade { .. }
            | Command::Pick(_)
    ) {
        commands::defer_config_writes();
    }
    match &command {
        Command::Install { packages, path, .. } => {
            commands::edit_installed(&cli.config, &hostname, path.as_deref(), packages, &[])?;
        }
        Command::Remove { packages, path, .. } => {
            commands::edit_installed(&cli.config, &hostname, path.as_deref(), &[], packages)?;
        }
        _ => {}
    }
    let evaluation = match commands::evaluate_config(&cli.config, &hostname, commands::deferred_overlay(&cli.config))? {
        Ok(evaluation) => evaluation,
        Err(errors) => {
            eprintln!("{errors}");
            return Ok(ExitCode::FAILURE);
        }
    };
    let tree = evaluation
        .captured
        .as_ref()
        .map_or(cli.config.clone(), |captured| captured.dir.clone());
    let target = commands::Target {
        config_dir: &cli.config,
        host: &hostname,
        config: &evaluation.config,
        tree: &tree,
        captured: evaluation.captured.as_ref(),
    };
    let state = archix::paths::Paths::system().state;
    match command {
        Command::Secrets(Secrets::Edit { name, machine }) => {
            archix::secrets::tool::edit(&cli.config, &hostname, &state, &name, machine)?
        }
        Command::Secrets(Secrets::Set { name, file }) => {
            archix::secrets::tool::set(&cli.config, &hostname, &name, file.as_deref())?
        }
        Command::Secrets(Secrets::Rekey { names, machine }) => {
            archix::secrets::tool::rekey(&cli.config, &hostname, &state, &names, machine)?
        }
        Command::Secrets(Secrets::Check) => {
            let rendered = archix::system::render_linked(&evaluation.config, &tree, &cli.config)
                .ok()
                .and_then(|r| r.state.secrets);
            if !archix::secrets::tool::check(&cli.config, &hostname, &state, rendered.as_ref())? {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Install {
            noconfirm,
            upgrade_first,
            ..
        } => {
            let result = if upgrade_first {
                commands::upgrade(&target, noconfirm)
            } else {
                commands::switch(&target, noconfirm, None, false)
            };
            result.map_err(|e| e.context("installed.lua was left as it was"))?;
        }
        Command::Remove { noconfirm, .. } => {
            commands::switch(&target, noconfirm, None, false)
                .map_err(|e| e.context("installed.lua was left as it was"))?;
        }
        Command::Search { terms } => commands::search(&target, &terms)?,
        Command::Pick(args) => {
            let noconfirm = args.iter().any(|arg| arg == "--noconfirm");
            let terms: Vec<String> = args.into_iter().filter(|arg| arg != "--noconfirm").collect();
            let picked = commands::pick(&target, &terms)?;
            if picked.is_empty() {
                return Ok(ExitCode::SUCCESS);
            }
            if unsafe { libc::geteuid() } != 0 {
                return install_with_sudo(&cli.config, &hostname, &picked, noconfirm);
            }
            commands::declare_and_switch(&target, &picked, noconfirm)?;
        }
        Command::Upgrade { noconfirm } => commands::upgrade(&target, noconfirm)?,
        Command::Eval => println!("{}", serde_json::to_string_pretty(&evaluation.config)?),
        Command::Options { json } => {
            // As root the config's own options would need an in-process evaluation, so only archix's built-in ones are listed.
            let dir = if is_root() {
                std::env::temp_dir().join(format!("archix-options-{}", std::process::id()))
            } else {
                cli.config.clone()
            };
            if is_root() {
                fs::create_dir_all(&dir)?;
                fs::write(dir.join("init.lua"), "return {}\n")?;
            }
            let options = eval::evaluate(&Input {
                config_dir: dir.clone(),
                hostname: hostname.clone(),
                ..Default::default()
            });
            if is_root() {
                let _ = fs::remove_dir_all(&dir);
            }
            print!(
                "{}",
                render_options(&options.map_err(|errors| anyhow::anyhow!("{errors}"))?, json)?
            )
        }
        Command::Lock { noconfirm } => commands::lock(&target, commands::Snapshot::Locked, noconfirm)?,
        Command::Update { to: None, noconfirm } => commands::lock(&target, commands::Snapshot::Latest, noconfirm)?,
        Command::Update {
            to: Some(date),
            noconfirm,
        } => commands::lock(&target, commands::Snapshot::On(date), noconfirm)?,
        Command::Diff { all } => commands::diff(&target, all)?,
        Command::Switch {
            noconfirm,
            accept_nearest,
            allow_unsafe_downgrade,
            without_secrets,
            allow_partial,
        } => {
            if without_secrets {
                commands::without_secrets();
            }
            if allow_partial {
                commands::allow_partial();
            }
            commands::switch(&target, noconfirm, accept_nearest, allow_unsafe_downgrade)?
        }
        Command::Status { all } => commands::status(&target, all)?,
        Command::Recover { .. }
        | Command::Init { .. }
        | Command::PacmanHook { .. }
        | Command::Rollback { .. }
        | Command::Generations
        | Command::Gc
        | Command::Prefetch { .. }
        | Command::Rescue { .. }
        | Command::BootCheck
        | Command::BootComplete
        | Command::BootMount { .. }
        | Command::Unblessed
        | Command::Notice { .. }
        | Command::Resync
        | Command::Bless
        | Command::HealthCheck
        | Command::Swapfile { .. }
        | Command::EmergencyHint { .. }
        | Command::SecureBoot(_)
        | Command::Cache(_)
        | Command::Snapshot(_)
        | Command::SplitVarLib
        | Command::EvaluateIsolated { .. }
        | Command::WriteFile { .. }
        | Command::Chroot { .. }
        | Command::Secrets(_)
        | Command::Sync { .. } => {
            unreachable!("handled before the config is evaluated")
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Installing needs root, so a search run as a user hands its picks to `sudo archix install`, the way yay calls sudo.
fn install_with_sudo(config: &std::path::Path, host: &str, packages: &[String], noconfirm: bool) -> Result<ExitCode> {
    let exe = std::env::current_exe().context("finding archix's own path")?;
    let mut cmd = std::process::Command::new("sudo");
    cmd.arg(exe)
        .arg("--config")
        .arg(config)
        .args(["--host", host, "install"]);
    if noconfirm {
        cmd.arg("--noconfirm");
    }
    let status = cmd.arg("--").args(packages).status().context("running sudo")?;
    Ok(if status.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn system_hostname() -> Result<String> {
    let name = fs::read_to_string("/proc/sys/kernel/hostname").context("reading the hostname; pass --host")?;
    Ok(name.trim().to_string())
}

fn render_options(evaluation: &Evaluation, json: bool) -> Result<String> {
    let docs: Vec<OptionDoc> = options::flatten(&evaluation.options)
        .into_iter()
        .map(|(path, decl)| OptionDoc {
            default: decl
                .default
                .clone()
                .and_then(|raw| options::check(&path, &decl.ty, Def::new(raw, decl.source.clone())).ok()),
            r#type: decl.ty.to_string(),
            description: decl.description.clone(),
            declared_in: decl.source.to_string(),
            path,
        })
        .collect();
    if json {
        return Ok(format!("{}\n", serde_json::to_string_pretty(&docs)?));
    }
    let mut out = String::new();
    for doc in docs {
        out.push_str(&format!("{}\n    type: {}\n", doc.path, doc.r#type));
        if let Some(default) = doc.default {
            out.push_str(&format!("    default: {default}\n"));
        }
        if let Some(description) = doc.description {
            out.push_str(&format!("    {description}\n"));
        }
        out.push_str(&format!("    declared in {}\n", doc.declared_in));
    }
    Ok(out)
}

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
    /// Build the next deployment from the lock and the config; it boots at the next restart
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
        /// Commit even though secrets don't decrypt on this machine yet: those are left out of the deployment, and what uses them fails until a switch after `archix secrets rekey`
        #[arg(long)]
        without_secrets: bool,
    },
    /// Gone: a switch commits a whole new deployment or nothing, so there's nothing to recover
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
    /// Make an earlier generation current (the previous one by default): its deployment if kept, else rebuilt from the package cache; it boots at the next restart
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
    /// List generations; the current one is marked with *, and those with a deployment say so
    Generations,
    /// Delete the deployments deployments.keep leaves out, old generation records and cached files no kept generation needs
    Gc,
    /// Stop managing this machine: systemd-boot starts the distro's kernels without archix, and archix's hooks and boot service come off
    Eject {
        /// Don't ask for confirmation
        #[arg(long)]
        noconfirm: bool,
        /// Put back the files archix replaced and delete the ones it created
        #[arg(long)]
        restore_originals: bool,
        /// Also delete /var/lib/archix and /var/cache/archix
        #[arg(long)]
        purge: bool,
        /// What becomes of declared secrets, which plain Arch has nothing to decrypt: drop their links, or materialize them as real files from /run/archix/secrets (from a booted system only)
        #[arg(long, value_enum)]
        secrets: Option<archix::deploy::eject::SecretsMode>,
    },
    /// From a live USB: bind a deployment's trees over the archix root mounted at ROOT, so `arch-chroot ROOT` runs it and `archix rollback` there can repair booting; run it as /usr/lib/archix/archix-static from a deployment under ROOT/archix/deploy
    MountDeployment {
        /// Where the archix root is mounted, such as /mnt (on btrfs, its root subvolume)
        root: PathBuf,
        /// The deployment to bind; current's by default
        deployment: Option<u32>,
    },
    /// Declare packages in installed.lua and install them (pacman-style: archix -S <packages>)
    Install {
        #[arg(required = true)]
        packages: Vec<String>,
        /// Directory under the config's lua/ whose installed.lua to edit, instead of lua/hosts/<host>/
        #[arg(long)]
        path: Option<PathBuf>,
        /// Install without declaring anything, in a new deployment; the next switch removes it again unless it gets declared
        #[arg(long)]
        temp: bool,
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
    /// Repair a machine from the rescue boot entry, which mounts no deployment; with no command, the rescue prompt
    Rescue {
        #[command(flatten)]
        options: commands::rescue::Options,
        #[command(subcommand)]
        action: Option<commands::rescue::Action>,
    },
    /// Run at boot by archix-boot.service: make the deployment whose boot entry was chosen current
    #[command(hide = true)]
    BootCheck {
        #[arg(long)]
        cmdline: Option<String>,
    },
    /// Run by archix-boot-complete.service once systemd-boot has counted the boot as good: make its generation the default
    #[command(hide = true)]
    BootComplete,
    /// Run by systemd-bless-boot.service, before its own rename: count the booted entry as good, synced under its new name
    #[command(hide = true)]
    Bless,
    /// Run by archix-health-check.service before boot-complete.target: check a new deployment's counted boot, and mark its entry bad if it fails
    #[command(hide = true)]
    HealthCheck,
    /// Called by archix's pacman hook to record transactions archix didn't run
    #[command(hide = true)]
    HookRecord,
    /// Called by archix's PreTransaction pacman hook: fail, pointing to archix, when plain pacman would change a deployment
    #[command(hide = true)]
    PacmanGuard,
    /// Run by archix-flatpak.service once the network is up: install the Flatpak apps a switch left for this deployment's first boot
    #[command(hide = true)]
    DeployFlatpak,
    /// Run by the initramfs: mount the deployment this boot belongs to over the root
    #[command(hide = true)]
    DeployMount {
        root: PathBuf,
        #[arg(long, default_value = archix::system::boot::SELECTED_ENTRY_VAR)]
        efivar: PathBuf,
        #[arg(long, default_value = "/run/archix")]
        run_dir: PathBuf,
    },
    /// Run early in every boot by archix-deploy-activate.service: apply a deployment's /var changes on its first boot
    #[command(hide = true)]
    DeployActivate {
        #[arg(long, default_value = "/")]
        root: PathBuf,
    },
    /// Run by archix-etc-merge.service from a commit until shutdown: carry /etc changes into the deployment that boots next; with --boot, check at boot that the last pass finished
    #[command(hide = true)]
    EtcMerge {
        #[arg(long, default_value = "/")]
        root: PathBuf,
        #[arg(long)]
        boot: bool,
    },
    /// Run by archix in a private mount namespace: delete what the root's own trees held before its first layout
    #[command(hide = true)]
    PruneHidden { root: PathBuf },
    /// Run at boot before a swap file declared with a size is swapped on: make it unless it's there with that size
    #[command(hide = true)]
    Swapfile { path: PathBuf, bytes: u64 },
    /// Run by emergency.service before sulogin in a deployment with required mounts: say which didn't mount and what to do
    #[command(hide = true)]
    EmergencyHint {
        #[arg(long)]
        no_rescue_entry: bool,
    },
    /// Run by `archix eject` in a private mount namespace: lay the deployment out as the root's own trees and hand the ESP over
    #[command(hide = true)]
    EjectHere {
        root: PathBuf,
        #[arg(long)]
        state: PathBuf,
    },
    /// Run by archix inside a new deployment's staging root: write archix's integration files there, with units that run `exec`
    #[command(hide = true)]
    DeployIntegrate {
        #[arg(long)]
        exec: PathBuf,
    },
    /// Set up and check Secure Boot
    #[command(subcommand)]
    SecureBoot(SecureBoot),
    /// Share package files and AUR builds with other machines through a package cache
    #[command(subcommand)]
    Cache(Cache),
    /// Run again, or drop, the hooks of a deployment that failed at too many boots
    #[command(subcommand)]
    Hooks(Hooks),
    /// Let boots bind the shared /usr/local again once a boot that left it out held it out
    #[command(subcommand)]
    UsrLocal(UsrLocal),
    /// Declared secrets: this machine's key, editing, setting, re-encrypting and checking them
    #[command(subcommand)]
    Secrets(Secrets),
    /// Check each kept deployment's /usr, /opt and /boot against its packages (content, mode, owner), like pacman -Qkk
    Verify {
        /// Restore what differs from the package cache
        #[arg(long)]
        repair: bool,
    },
    /// `archix <terms>`, like `yay <terms>`: search, pick from a numbered list, install the picks
    #[command(external_subcommand)]
    Pick(Vec<String>),
}

#[derive(Subcommand)]
enum Hooks {
    /// Run the waiting hooks now, on the running deployment
    Retry {
        /// The deployment whose hooks to run; the running one by default
        deployment: Option<u32>,
    },
    /// Drop the waiting hooks without running them
    Skip {
        /// The deployment whose hooks to drop; the running one by default
        deployment: Option<u32>,
    },
}

#[derive(Subcommand)]
enum Secrets {
    /// Make this machine's age identity in /var/lib/archix/secrets and print the key for secrets.machine_key
    Init {
        /// Make a new identity even though one exists; the old one is retired, kept while a deployment needs it
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
    /// Make a new machine identity; the old one is retired until no kept deployment needs it (gc drops it then)
    RotateMachineKey {
        /// The old key leaked: delete it at once instead of retiring it
        #[arg(long)]
        compromised: bool,
    },
    /// Run at boot by archix-secrets.service: decrypt the booted deployment's secrets into /run/archix/secrets
    #[command(hide = true)]
    Activate {
        #[arg(long, default_value = "/")]
        root: PathBuf,
    },
}

#[derive(Subcommand)]
enum UsrLocal {
    /// Let the next boot bind /usr/local again, once; if it doesn't reach multi-user.target, the boot after leaves it out again
    Release,
    /// Move the files under /var/usrlocal that systemd, udev or the module loader read at boot to /var/usrlocal.quarantine, then release /usr/local
    Quarantine,
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
    /// Replace systemd's PCR policy signing key in every deployment and bind the TPM2 slot of each LUKS volume given to the new one; the next switch builds UKIs whose policies it signs, and UKIs built before no longer unlock those volumes
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
            temp: false,
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
        Command::Hooks(_) => Some("hooks"),
        Command::UsrLocal(_) => Some("usr-local"),
        Command::Secrets(Secrets::Init { .. }) => Some("secrets init"),
        Command::Secrets(Secrets::RotateMachineKey { .. }) => Some("secrets rotate-machine-key"),
        Command::Secrets(Secrets::Edit { machine: true, .. }) => Some("secrets edit --machine"),
        Command::Secrets(Secrets::Rekey { machine: true, .. }) => Some("secrets rekey --machine"),
        Command::Verify { .. } => Some("verify"),
        Command::Eject { .. } => Some("eject"),
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
        Command::DeployMount { root, efivar, run_dir } => {
            if let Err(e) = archix::system::deploy_boot::mount(&root, &efivar, &run_dir) {
                if efivar == std::path::Path::new(archix::system::boot::SELECTED_ENTRY_VAR) {
                    archix::system::boot::mount_efivars();
                }
                archix::system::boot::reboot_counted_try("mounting this boot's deployment", &e, &root, true);
                return Err(e);
            }
            return Ok(ExitCode::SUCCESS);
        }
        Command::Secrets(Secrets::Activate { root }) => {
            if !is_root() {
                anyhow::bail!("`archix secrets activate` runs at boot as root");
            }
            archix::secrets::activate::run(&root)?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::DeployActivate { root } => {
            if let Err(e) = archix::system::activate::activate(&root) {
                archix::system::boot::reboot_counted_try("activating this boot's deployment", &e, &root, false);
                return Err(e);
            }
            return Ok(ExitCode::SUCCESS);
        }
        Command::PruneHidden { root } => {
            commands::prune_hidden(&root)?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::Swapfile { path, bytes } => {
            archix::system::fstab::make_swapfile(&path, bytes)?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::EmergencyHint { no_rescue_entry } => {
            archix::system::emergency::run(&archix::paths::Paths::system(), !no_rescue_entry);
            return Ok(ExitCode::SUCCESS);
        }
        Command::MountDeployment { root, deployment } => {
            commands::mount_deployment(&root, deployment)?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::EjectHere { root, state } => {
            let paths = archix::paths::Paths {
                root: root.clone(),
                state,
                ..archix::paths::Paths::system()
            };
            for line in archix::deploy::eject::here(&root, &paths)? {
                println!("{line}");
            }
            return Ok(ExitCode::SUCCESS);
        }
        Command::EtcMerge { root, boot: true } => {
            archix::etcmerge::boot(&root)?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::EtcMerge { root, boot: false } => {
            archix::etcmerge::watch::run(&root)?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::DeployIntegrate { exec } => {
            let paths = archix::paths::Paths::system();
            archix::integration::write_all(&paths, &exec, &|path| Ok(paths.root.join(path.strip_prefix("/")?)))?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::PacmanGuard => {
            if archix::integration::pacman_allowed(std::path::Path::new("/")) {
                return Ok(ExitCode::SUCCESS);
            }
            println!("{}", archix::integration::PACMAN_REFUSAL);
            return Ok(ExitCode::FAILURE);
        }
        Command::Recover { .. } => anyhow::bail!(
            "`archix recover` is gone: a switch commits a whole new deployment or leaves nothing behind, so there's nothing to recover; run `archix switch` again"
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
            Command::BootCheck { .. }
                | Command::BootComplete
                | Command::Bless
                | Command::DeployFlatpak
                | Command::HealthCheck
        );
    if needs_root(&command).is_some() {
        leave_login_session();
    }
    if changes_things {
        hold_exclusive()?;
    }
    if is_root() {
        archix::system::deploy_boot::writable_store(&archix::testhooks::root())?;
    }
    match command {
        Command::HookRecord => {
            commands::hook_record();
            return Ok(ExitCode::SUCCESS);
        }
        Command::Rollback {
            generation,
            noconfirm,
            allow_unsafe_downgrade,
        } => {
            commands::rollback(&cli.config, generation, noconfirm, allow_unsafe_downgrade)?;
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
        Command::Eject {
            noconfirm,
            restore_originals,
            purge,
            secrets,
        } => {
            commands::eject(noconfirm, restore_originals, purge, secrets)?;
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
        Command::Hooks(Hooks::Retry { deployment }) => {
            commands::hooks(true, deployment)?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::Hooks(Hooks::Skip { deployment }) => {
            commands::hooks(false, deployment)?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::UsrLocal(action) => {
            let paths = archix::paths::Paths::system();
            let said = match action {
                UsrLocal::Release => archix::system::usr_local::release(&paths)?,
                UsrLocal::Quarantine => archix::system::usr_local::quarantine(&paths)?,
            };
            println!("{said}");
            return Ok(ExitCode::SUCCESS);
        }
        Command::Verify { repair } => {
            commands::verify(repair)?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::Prefetch { url } => {
            commands::prefetch(&url)?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::BootCheck { cmdline } => {
            commands::boot_check(cmdline)?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::BootComplete => {
            if let Err(e) = archix::system::usr_local::confirm(&archix::paths::Paths::system()) {
                println!("archix: {e:#}");
            }
            if let Some(booted) = archix::system::boot::booted() {
                archix::system::emergency::forget(&archix::paths::Paths::system(), booted);
            }
            archix::system::boot::promote_booted(&archix::paths::Paths::system())?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::Bless => {
            archix::system::boot::bless_booted(&archix::paths::Paths::system())?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::DeployFlatpak => {
            commands::deploy_flatpak()?;
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
    // Nothing these commands change in the config is written until the switch commits its deployment.
    if matches!(
        command,
        Command::Install { temp: false, .. }
            | Command::Remove { .. }
            | Command::Switch { .. }
            | Command::Upgrade { .. }
            | Command::Pick(_)
    ) {
        commands::defer_config_writes();
    }
    match &command {
        Command::Install {
            packages,
            path,
            temp: false,
            ..
        } => {
            commands::edit_installed(&cli.config, &hostname, path.as_deref(), packages, &[])?;
        }
        Command::Remove { packages, path, .. } => {
            commands::edit_installed(&cli.config, &hostname, path.as_deref(), &[], packages)?;
        }
        _ => {}
    }
    let evaluation = match eval::evaluate(&Input {
        config_dir: cli.config.clone(),
        hostname: hostname.clone(),
        overlay: commands::deferred_overlay(&cli.config),
    }) {
        Ok(evaluation) => evaluation,
        Err(errors) => {
            eprintln!("{errors}");
            return Ok(ExitCode::FAILURE);
        }
    };
    let target = commands::Target {
        config_dir: &cli.config,
        host: &hostname,
        config: &evaluation.config,
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
            let rendered = archix::system::render(&evaluation.config, &cli.config)
                .ok()
                .and_then(|r| r.state.secrets);
            if !archix::secrets::tool::check(&cli.config, &hostname, &state, rendered.as_ref())? {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Install {
            packages,
            temp: true,
            noconfirm,
            ..
        } => commands::install_temporarily(&target, &packages, noconfirm)?,
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
        Command::Options { json } => print!("{}", render_options(&evaluation, json)?),
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
        } => {
            if without_secrets {
                commands::without_secrets();
            }
            commands::switch(&target, noconfirm, accept_nearest, allow_unsafe_downgrade)?
        }
        Command::Status { all } => commands::status(&target, all)?,
        Command::Recover { .. }
        | Command::Init { .. }
        | Command::HookRecord
        | Command::PacmanGuard
        | Command::DeployFlatpak
        | Command::Rollback { .. }
        | Command::Generations
        | Command::Gc
        | Command::Eject { .. }
        | Command::Prefetch { .. }
        | Command::Rescue { .. }
        | Command::BootCheck { .. }
        | Command::BootComplete
        | Command::Bless
        | Command::HealthCheck
        | Command::DeployMount { .. }
        | Command::DeployActivate { .. }
        | Command::PruneHidden { .. }
        | Command::Swapfile { .. }
        | Command::EmergencyHint { .. }
        | Command::MountDeployment { .. }
        | Command::EjectHere { .. }
        | Command::EtcMerge { .. }
        | Command::DeployIntegrate { .. }
        | Command::SecureBoot(_)
        | Command::Cache(_)
        | Command::Hooks(_)
        | Command::UsrLocal(_)
        | Command::Secrets(_)
        | Command::Verify { .. }
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

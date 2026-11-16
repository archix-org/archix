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
    /// Write a minimal config for this machine as it stands (explicit packages, enabled services, users), locked to what's installed
    Init,
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
    /// Show the plan a switch would apply, read-only: exit 0 nothing to do, 1 changes, 2 switch would refuse, 3 error, 4 incomplete
    Diff {
        /// Print the plan as JSON, with its schema version
        #[arg(long, conflicts_with = "stat")]
        json: bool,
        /// Print names only
        #[arg(long)]
        stat: bool,
        /// Write the plan and its fingerprint to this file, for `switch --plan`
        #[arg(long, value_name = "FILE")]
        save: Option<PathBuf>,
        /// Use the sync dbs already cached instead of fetching them; the header says so
        #[arg(long)]
        cached: bool,
        /// Skip `home-manager build`, so home-manager users' file changes aren't listed (exit 4)
        #[arg(long)]
        no_build: bool,
    },
    /// Apply the config to the running system; being rewritten for the rescope
    Switch {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        rest: Vec<String>,
    },
    /// Declare packages and install them; being rewritten with switch
    Install {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        rest: Vec<String>,
    },
    /// Remove packages; being rewritten with switch
    Remove {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        rest: Vec<String>,
    },
    /// Move the lock forward and switch; being rewritten with switch
    Upgrade {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        rest: Vec<String>,
    },
    /// Search the repositories and the AUR (pacman-style: archix -Ss <terms>)
    Search {
        #[arg(required = true)]
        terms: Vec<String>,
    },
    /// Download a file and print the archix.fetchurl { url, sha256 } line to paste into the config
    Prefetch { url: String },
    /// Run at boot before a swap file declared with a size is swapped on: make it unless it's there with that size
    #[command(hide = true)]
    Swapfile { path: PathBuf, bytes: u64 },
    /// Declared secrets: this machine's key, editing, setting, re-encrypting and checking them
    #[command(subcommand)]
    Secrets(Secrets),
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
        Command::Secrets(Secrets::Init { .. }) => Some("secrets init"),
        Command::Secrets(Secrets::RotateMachineKey { .. }) => Some("secrets rotate-machine-key"),
        Command::Secrets(Secrets::Edit { machine: true, .. }) => Some("secrets edit --machine"),
        Command::Secrets(Secrets::Rekey { machine: true, .. }) => Some("secrets rekey --machine"),
        Command::Init => Some("init"),
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

/// Takes the lock, or refuses while another run holds it.
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
    let mut cli = Cli::parse();
    // Git and the config's owner resolve a relative path from wherever they run, which isn't always here.
    cli.config = std::path::absolute(&cli.config).with_context(|| format!("resolving {}", cli.config.display()))?;
    let Some(command) = cli.command else {
        <Cli as clap::CommandFactory>::command().print_help()?;
        return Ok(ExitCode::from(2));
    };
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
        Command::Switch { .. } | Command::Install { .. } | Command::Remove { .. } | Command::Upgrade { .. } => {
            eprintln!("archix: switch is being rewritten for the rescope (docs/SCOPE.md)");
            return Ok(ExitCode::from(3));
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
    if needs_root(&command).is_some() {
        leave_login_session();
        hold_exclusive()?;
    }
    match command {
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
        Command::Prefetch { url } => {
            commands::prefetch(&url)?;
            return Ok(ExitCode::SUCCESS);
        }
        _ => {}
    }
    let hostname = match cli.host {
        Some(host) => host,
        None => system_hostname()?,
    };
    if let Command::Init = command {
        commands::init(&cli.config, &hostname)?;
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
    if let Command::Diff {
        json,
        stat,
        save,
        cached,
        no_build,
    } = command
    {
        let args = commands::diff::DiffArgs {
            json,
            stat,
            save,
            cached,
            no_build,
        };
        return Ok(ExitCode::from(
            commands::diff::diff(&cli.config, &hostname, &args).unwrap_or_else(|e| {
                eprintln!("error: {e:#}");
                commands::diff::error_code()
            }),
        ));
    }
    if !cli.config.join("init.lua").is_file() {
        anyhow::bail!(
            "there's no archix config at {} yet; run `sudo archix init` to write one that describes this machine, or point --config at yours",
            cli.config.display()
        );
    }
    let evaluation = archix::plan::evaluate::evaluate_for_plan(&cli.config, &hostname)?;
    let target = commands::Target {
        config_dir: &cli.config,
        host: &hostname,
        config: &evaluation.config,
        tree: &evaluation.tree,
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
            let rendered = archix::system::render_linked(&evaluation.config, &evaluation.tree, &cli.config)
                .ok()
                .and_then(|r| r.state.secrets);
            if !archix::secrets::tool::check(&cli.config, &hostname, &state, rendered.as_ref())? {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Search { terms } => commands::search(&target, &terms)?,
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
        Command::Diff { .. }
        | Command::Switch { .. }
        | Command::Install { .. }
        | Command::Remove { .. }
        | Command::Upgrade { .. }
        | Command::Init
        | Command::Prefetch { .. }
        | Command::Swapfile { .. }
        | Command::EvaluateIsolated { .. }
        | Command::WriteFile { .. }
        | Command::Secrets(_) => {
            unreachable!("handled before the config is evaluated")
        }
    }
    Ok(ExitCode::SUCCESS)
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

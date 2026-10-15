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
    /// Show what a switch would change to make this machine match the lock
    Diff,
    /// Make this machine's packages match the lock
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
    },
    /// Deal with a switch that was interrupted: continue it, roll it back or abandon it
    #[command(group = clap::ArgGroup::new("mode"))]
    Recover {
        /// Finish the interrupted switch
        #[arg(long = "continue", group = "mode")]
        cont: bool,
        /// Return to the packages from before the interrupted switch
        #[arg(long, group = "mode")]
        rollback: bool,
        /// Close the journal without changing anything, after showing what state the system is in
        #[arg(long, group = "mode")]
        abandon: bool,
    },
    /// Show the lock, pending changes and any unfinished switch
    Status,
    /// Put back the packages of an earlier generation (the previous one by default)
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
    /// List generations; the current one is marked with *
    Generations,
    /// Delete old generations and cached files no kept generation needs
    Gc,
    /// Declare packages in installed.lua and install them (pacman-style: archix -S <packages>)
    Install {
        #[arg(required = true)]
        packages: Vec<String>,
        /// Directory under the config's lua/ whose installed.lua to edit, instead of lua/hosts/<host>/
        #[arg(long)]
        path: Option<PathBuf>,
        /// Install without declaring anything; the next switch removes it again unless it gets declared
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
    /// pacman -Syu, adopt the result, move AUR packages to their newest recipes and switch (also: archix, archix -Syu)
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
    /// Restore a generation's core packages under another root without pacman (rescue mode, or from a live USB)
    Rescue {
        /// The mounted system to repair, such as /mnt or the initramfs's /sysroot
        #[arg(long, default_value = "/")]
        root: PathBuf,
        /// Generation whose packages to restore; the current one by default
        #[arg(long)]
        generation: Option<u32>,
        /// Also restore these packages, beyond the core set
        #[arg(long = "package")]
        packages: Vec<String>,
    },
    /// Run at boot by archix-boot.service: switch to the generation whose boot entry was chosen
    #[command(hide = true)]
    BootCheck {
        #[arg(long)]
        cmdline: Option<String>,
    },
    /// Called by archix's pacman hook to record transactions archix didn't run
    #[command(hide = true)]
    HookRecord,
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
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let command = from_sync(cli.command.unwrap_or(Command::Upgrade { noconfirm: false }))?;
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
        Command::Prefetch { url } => {
            commands::prefetch(&url)?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::Rescue {
            root,
            generation,
            packages,
        } => {
            commands::rescue(&root, generation, &packages)?;
            return Ok(ExitCode::SUCCESS);
        }
        Command::BootCheck { cmdline } => {
            commands::boot_check(cmdline)?;
            return Ok(ExitCode::SUCCESS);
        }
        _ => {}
    }
    if let Command::Recover {
        cont,
        rollback,
        abandon,
    } = command
    {
        let mode = match (cont, rollback, abandon) {
            (true, _, _) => Some(commands::RecoverMode::Continue),
            (_, true, _) => Some(commands::RecoverMode::Rollback),
            (_, _, true) => Some(commands::RecoverMode::Abandon),
            _ => None,
        };
        commands::recover(mode)?;
        return Ok(ExitCode::SUCCESS);
    }
    let hostname = match cli.host {
        Some(host) => host,
        None => system_hostname()?,
    };
    if let Command::Init { defaults } = command {
        commands::init(&cli.config, &hostname, defaults)?;
        return Ok(ExitCode::SUCCESS);
    }
    let edit = match &command {
        Command::Install {
            packages,
            path,
            temp: false,
            ..
        } => Some(commands::edit_installed(
            &cli.config,
            &hostname,
            path.as_deref(),
            packages,
            &[],
        )?),
        Command::Remove { packages, path, .. } => Some(commands::edit_installed(
            &cli.config,
            &hostname,
            path.as_deref(),
            &[],
            packages,
        )?),
        _ => None,
    };
    let undo = || {
        if let Some(edit) = &edit {
            edit.restore();
        }
    };
    let evaluation = match eval::evaluate(&Input {
        config_dir: cli.config.clone(),
        hostname: hostname.clone(),
    }) {
        Ok(evaluation) => evaluation,
        Err(errors) => {
            undo();
            eprintln!("{errors}");
            return Ok(ExitCode::FAILURE);
        }
    };
    let target = commands::Target {
        config_dir: &cli.config,
        host: &hostname,
        config: &evaluation.config,
    };
    match command {
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
            if let Err(e) = result {
                undo();
                return Err(e.context("installed.lua was restored"));
            }
        }
        Command::Remove { noconfirm, .. } => {
            if let Err(e) = commands::switch(&target, noconfirm, None, false) {
                undo();
                return Err(e.context("installed.lua was restored"));
            }
        }
        Command::Search { terms } => commands::search(&target, &terms)?,
        Command::Upgrade { noconfirm } => commands::upgrade(&target, noconfirm)?,
        Command::Eval => println!("{}", serde_json::to_string_pretty(&evaluation.config)?),
        Command::Options { json } => print!("{}", render_options(&evaluation, json)?),
        Command::Lock { noconfirm } => commands::lock(&target, commands::Snapshot::Locked, noconfirm)?,
        Command::Update { to: None, noconfirm } => commands::lock(&target, commands::Snapshot::Latest, noconfirm)?,
        Command::Update {
            to: Some(date),
            noconfirm,
        } => commands::lock(&target, commands::Snapshot::On(date), noconfirm)?,
        Command::Diff => commands::diff(&target)?,
        Command::Switch {
            noconfirm,
            accept_nearest,
            allow_unsafe_downgrade,
        } => commands::switch(&target, noconfirm, accept_nearest, allow_unsafe_downgrade)?,
        Command::Status => commands::status(&target)?,
        Command::Recover { .. }
        | Command::Init { .. }
        | Command::HookRecord
        | Command::Rollback { .. }
        | Command::Generations
        | Command::Gc
        | Command::Prefetch { .. }
        | Command::Rescue { .. }
        | Command::BootCheck { .. }
        | Command::Sync { .. } => {
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

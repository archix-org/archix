use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;

mod packages;

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
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the evaluated configuration as JSON
    Eval,
    /// List every declared option
    Options {
        #[arg(long)]
        json: bool,
    },
    /// Resolve the declared packages into archix.lock, keeping the locked snapshot date
    Lock,
    /// Move archix.lock to the newest archive snapshot (or --to a date) and re-resolve
    Update {
        /// Snapshot date to move to, as YYYY-MM-DD
        #[arg(long)]
        to: Option<archix::date::Date>,
    },
    /// Show what a switch would change to make this machine match the lock
    Diff,
    /// Make this machine's packages match the lock
    Switch {
        /// Don't ask for confirmation
        #[arg(long)]
        noconfirm: bool,
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
    /// Called by archix's pacman hook to record transactions archix didn't run
    #[command(hide = true)]
    HookRecord,
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
    if let Command::HookRecord = cli.command {
        packages::hook_record();
        return Ok(ExitCode::SUCCESS);
    }
    if let Command::Recover {
        cont,
        rollback,
        abandon,
    } = cli.command
    {
        let mode = match (cont, rollback, abandon) {
            (true, _, _) => Some(packages::RecoverMode::Continue),
            (_, true, _) => Some(packages::RecoverMode::Rollback),
            (_, _, true) => Some(packages::RecoverMode::Abandon),
            _ => None,
        };
        packages::recover(mode)?;
        return Ok(ExitCode::SUCCESS);
    }
    let hostname = match cli.host {
        Some(host) => host,
        None => system_hostname()?,
    };
    let evaluation = match eval::evaluate(&Input {
        config_dir: cli.config.clone(),
        hostname: hostname.clone(),
    }) {
        Ok(evaluation) => evaluation,
        Err(errors) => {
            eprintln!("{errors}");
            return Ok(ExitCode::FAILURE);
        }
    };
    let target = packages::Target {
        config_dir: &cli.config,
        host: &hostname,
        config: &evaluation.config,
    };
    match cli.command {
        Command::Eval => println!("{}", serde_json::to_string_pretty(&evaluation.config)?),
        Command::Options { json } => print!("{}", render_options(&evaluation, json)?),
        Command::Lock => packages::lock(&target, packages::Snapshot::Locked)?,
        Command::Update { to: None } => packages::lock(&target, packages::Snapshot::Latest)?,
        Command::Update { to: Some(date) } => packages::lock(&target, packages::Snapshot::On(date))?,
        Command::Diff => packages::diff(&target)?,
        Command::Switch { noconfirm } => packages::switch(&target, noconfirm)?,
        Command::Status => packages::status(&target)?,
        Command::Recover { .. } | Command::HookRecord => unreachable!("handled before the config is evaluated"),
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

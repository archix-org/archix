//! The commands home-manager support runs, behind a trait so tests see the argv instead of running it.

use std::io::Write as _;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Cmd {
    pub argv: Vec<String>,
    pub stdin: Option<Vec<u8>>,
}

impl Cmd {
    pub fn new(argv: Vec<String>) -> Cmd {
        Cmd { argv, stdin: None }
    }

    pub fn line(&self) -> String {
        self.argv.join(" ")
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Output {
    pub success: bool,
    pub stdout: String,
}

pub trait Exec {
    fn run(&self, cmd: &Cmd) -> Result<Output>;
}

/// Runs commands for real; stderr goes to ours, so long builds show progress.
pub struct System;

impl Exec for System {
    fn run(&self, cmd: &Cmd) -> Result<Output> {
        let (program, args) = cmd.argv.split_first().context("an empty command")?;
        let mut child = Command::new(program)
            .args(args)
            .stdin(if cmd.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("running {program}"))?;
        if let Some(input) = &cmd.stdin {
            let mut stdin = child.stdin.take().context("no stdin")?;
            let written = stdin.write_all(input);
            drop(stdin);
            if let Err(e) = written {
                let _ = child.wait();
                return Err(e).with_context(|| format!("writing to {program}"));
            }
        }
        let out = child
            .wait_with_output()
            .with_context(|| format!("waiting for {program}"))?;
        Ok(Output {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        })
    }
}

/// Runs `cmd` and fails unless it succeeds, naming `what`.
pub fn checked(exec: &dyn Exec, cmd: &Cmd, what: &str) -> Result<Output> {
    let out = exec.run(cmd)?;
    if !out.success {
        bail!("{what} failed: {}", cmd.line());
    }
    Ok(out)
}

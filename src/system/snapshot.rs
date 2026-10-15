use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::generations::now;

const DIR: &str = "/.archix-snapshots";

pub fn root_is_btrfs() -> bool {
    Command::new("stat")
        .args(["-f", "-c", "%T", "/"])
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "btrfs")
}

/// A read-only snapshot of / taken before generation `number` is applied, for recovery without a live USB.
pub fn take(number: u32) -> Result<String> {
    fs::create_dir_all(DIR).with_context(|| format!("creating {DIR}"))?;
    let path = format!("{DIR}/{number}-{}", now());
    let status = Command::new("btrfs")
        .args(["subvolume", "snapshot", "-r", "/", &path])
        .status()
        .context("running btrfs")?;
    if !status.success() {
        bail!("btrfs could not snapshot / to {path}");
    }
    Ok(path)
}

pub fn delete(path: &str) -> Result<()> {
    if !Path::new(path).exists() {
        return Ok(());
    }
    let status = Command::new("btrfs")
        .args(["subvolume", "delete", path])
        .status()
        .context("running btrfs")?;
    if !status.success() {
        bail!("btrfs could not delete the snapshot {path}");
    }
    Ok(())
}

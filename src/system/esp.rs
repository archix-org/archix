//! What boot.rs needs from the ESP side of the old deployment commit, kept for stage 3's entries.

use std::fs;
use std::io;
use std::path::Path;

use anyhow::{Context, Result};

/// Puts a generation's boot entry on the ESP; every file it writes must be complete before its final name appears.
pub trait EspStep {
    fn install(&self, number: u32, tree: &Path) -> Result<()>;

    /// Removes temporary files an interrupted `install` left on the ESP.
    fn clean(&self) -> Result<()> {
        Ok(())
    }

    /// Takes back what a failed `install` of generation `number` wrote, leaving `previous` the default.
    fn abandon(&self, _number: u32, _previous: Option<u32>) -> Result<()> {
        Ok(())
    }
}

/// Removes the temporary files an interrupted write left in archix's ESP directories.
pub fn clean_esp(esp: &Path) -> Result<()> {
    for (dir, prefix) in [
        ("EFI/archix", ""),
        (super::rescue::DIR, ""),
        ("EFI/Linux", "archix-fallback-"),
        ("loader/entries", "archix-"),
        ("loader", "loader.conf"),
        ("EFI/systemd", "systemd-bootx64.efi"),
        ("EFI/BOOT", "BOOTX64.EFI"),
    ] {
        let dir = esp.join(dir);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
        };
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let kind = entry
                .file_type()
                .with_context(|| format!("reading {}", entry.path().display()))?;
            if name.starts_with(prefix) && name.ends_with(".tmp") && kind.is_file() {
                fs::remove_file(entry.path()).with_context(|| format!("removing {}", entry.path().display()))?;
            }
        }
    }
    Ok(())
}

/// The generation a boot entry file names: archix-<N>.conf, with or without systemd-boot's +tries suffix.
pub fn entry_number(entry: &str) -> Option<u32> {
    let rest = entry.strip_prefix("archix-")?.strip_suffix(".conf")?;
    let digits = rest.split('+').next()?;
    if digits.is_empty() || digits.starts_with('0') || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The generation loader.conf's last `default` line names.
pub fn boot_default(conf: &str) -> Option<u32> {
    conf.lines()
        .filter_map(|line| line.strip_prefix("default"))
        .rfind(|rest| rest.starts_with([' ', '\t']))
        .and_then(|rest| entry_number(rest.trim()))
}

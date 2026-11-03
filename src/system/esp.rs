//! Cleaning up after an interrupted write to the ESP.

use std::fs;
use std::io;
use std::path::Path;

use anyhow::{Context, Result};

/// Removes the temporary files an interrupted write left in archix's ESP directories.
pub fn clean_esp(esp: &Path) -> Result<()> {
    for (dir, prefix) in [
        ("EFI/archix", ""),
        (super::rescue::DIR, ""),
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

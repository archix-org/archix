//! GRUB's share of archix's menu: the same entries as systemd-boot's, chainloading the same images, picked by hand; GRUB counts no boots, so nothing falls back by itself.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::menu::{Entry, IMAGES, Planned};
use crate::paths::Paths;

/// The script grub-mkconfig runs, which makes grub.cfg read archix's menu at boot, so a switch never has to run grub-mkconfig again.
pub const SCRIPT: &str = "etc/grub.d/41_archix";
/// The menu archix rewrites after each change, beside grub.cfg.
pub const MENU: &str = "boot/grub/archix.cfg";

const SCRIPT_TEXT: &str = "#!/bin/sh\ncat <<'EOF'\nif [ -f ${config_directory}/archix.cfg ]; then\n  source ${config_directory}/archix.cfg\nfi\nEOF\n";

/// The kernel command line an entry passes: the images embed one, which systemd-stub replaces with what GRUB passes while Secure Boot is off, so it's the whole line plus the snapshot to boot.
fn options(entry: &Entry, cmdline: &str) -> String {
    match entry {
        Entry::Snapshot(number) => format!("{} {}={number}", cmdline.trim(), super::initrd::SNAPSHOT_ARG),
        _ => cmdline.trim().to_string(),
    }
}

/// The menu text: one entry per planned one, in the same order, found on the ESP by its filesystem UUID.
pub fn menu(planned: &[Planned], esp_uuid: &str, cmdline: &str) -> String {
    let mut text = String::from("# Written by archix after each change; edit the config instead.\n");
    for planned in planned {
        text.push_str(&format!(
            "menuentry '{}' {{\n  search --no-floppy --fs-uuid --set=root {esp_uuid}\n  chainloader /{IMAGES}/{} {}\n}}\n",
            planned.title.replace('\'', ""),
            planned.image,
            options(&planned.entry, cmdline)
        ));
    }
    text
}

/// Writes the menu, recording each entry as systemd-boot's are, and the script that reads it; the first time, runs grub-mkconfig so grub.cfg has it.
pub fn write(paths: &Paths, esp: &Path, dir: &Path, planned: &[Planned], cmdline: &str) -> Result<()> {
    let uuid = super::boot::disk_link_of(esp, "by-uuid")
        .with_context(|| format!("couldn't tell the filesystem UUID of the ESP at {}", esp.display()))?;
    let entries = dir.join("entries");
    let _ = fs::remove_dir_all(&entries);
    for planned in planned {
        crate::durable::write(
            &super::menu::record_path(dir, &planned.entry.id()),
            options(&planned.entry, cmdline).as_bytes(),
        )?;
    }
    crate::durable::write(&paths.root.join(MENU), menu(planned, &uuid, cmdline).as_bytes())?;
    let script = paths.root.join(SCRIPT);
    if fs::read_to_string(&script).ok().as_deref() == Some(SCRIPT_TEXT) {
        return Ok(());
    }
    crate::durable::write_executable(&script, SCRIPT_TEXT.as_bytes())?;
    let root = super::live::Root::new(&paths.root);
    let output = root.output(&["grub-mkconfig", "-o", "/boot/grub/grub.cfg"])?;
    if !output.status.success() {
        bail!(
            "grub-mkconfig failed, so GRUB doesn't list archix's entries yet: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grubs_menu_chainloads_each_image_and_names_the_snapshot_on_the_command_line() {
        let planned = vec![
            Planned {
                entry: Entry::Current,
                image: "6.17-abc.efi".into(),
                title: "Arch Linux (archix)".into(),
                sort_key: super::super::menu::SORT_CURRENT.into(),
                version: "6.17".into(),
                versioned: true,
            },
            Planned {
                entry: Entry::Snapshot(4),
                image: "6.16-def.efi".into(),
                title: "archix snapshot 4, read-only: it's old".into(),
                sort_key: super::super::menu::SORT_SLOT.into(),
                version: "4".into(),
                versioned: true,
            },
        ];
        let text = menu(&planned, "AB12-CD34", "root=UUID=x rw ");
        assert!(
            text.contains("menuentry 'Arch Linux (archix)' {\n  search --no-floppy --fs-uuid --set=root AB12-CD34\n  chainloader /EFI/archix/6.17-abc.efi root=UUID=x rw\n}\n"),
            "{text}"
        );
        assert!(
            text.contains("menuentry 'archix snapshot 4, read-only: its old'")
                && text.contains("chainloader /EFI/archix/6.16-def.efi root=UUID=x rw archix.snapshot=4\n"),
            "{text}"
        );
        assert!(SCRIPT_TEXT.contains("source ${config_directory}/archix.cfg"));
    }
}

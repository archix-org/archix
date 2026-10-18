use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use anyhow::{Context, Result, bail};

use archix::generations::Store;
use archix::integration;
use archix::paths::Paths;
use archix::system::apply::SystemChange;
use archix::system::{SystemState, boot, snapshot};

use super::{check_pacman_free, confirm, refuse_if_unfinished, require_root};

/// Stops archix managing the machine: systemd-boot starts the distro's kernels without it, and its hooks and boot service come off.
pub fn eject(noconfirm: bool, restore_originals: bool, purge: bool) -> Result<()> {
    require_root("eject")?;
    let paths = Paths::system();
    refuse_if_unfinished(&paths)?;
    check_pacman_free(&paths)?;
    let store = Store::new(&paths);
    let current = match store.current()? {
        Some(number) => Some(store.get(number)?),
        None => None,
    };
    let esp = fs::read_to_string(paths.state.join("boot/esp"))
        .ok()
        .map(|esp| esp.trim().to_string())
        .filter(|esp| !boot::entries(esp).is_empty());
    let kernels = boot::installed_kernels();
    let default = current
        .as_ref()
        .and_then(|generation| generation.settings.boot.as_deref())
        .and_then(|settings| settings.split_whitespace().next())
        .unwrap_or("linux")
        .to_string();
    let sign = esp.is_some() && boot::sbctl_has_keys();

    println!("archix eject: archix stops managing this machine");
    if let Some(esp) = &esp {
        let names: Vec<&str> = kernels.iter().map(|(base, _)| base.as_str()).collect();
        let how = if fs::canonicalize(esp).ok() == fs::canonicalize("/boot").ok() {
            "plain systemd-boot entries for /boot's kernel and initramfs"
        } else {
            "UKIs that mkinitcpio's presets write to EFI/Linux"
        };
        println!(
            "  boot      {how}, for {}{}; then archix's {} entries go",
            names.join(", "),
            if sign {
                ", signed with sbctl, whose hook re-signs them on kernel updates"
            } else {
                ""
            },
            boot::entries(esp).len()
        );
    }
    println!("  removes   archix-boot.service, the rescue initramfs hook, the pacman hook and the login notices");
    if restore_originals {
        println!(
            "  files     puts back the files archix replaced and deletes the ones it created, except /etc/kernel/cmdline"
        );
    } else {
        println!("  files     stay as they are, made writable again (--restore-originals puts the originals back)");
    }
    println!("  keeps     packages, users, groups, services and pacman.conf");
    if purge {
        println!("  deletes   /var/lib/archix, /var/cache/archix and archix's btrfs snapshots");
    } else {
        println!("  keeps     /var/lib/archix and /var/cache/archix (--purge deletes them)");
    }
    if !noconfirm && !confirm("eject?")? {
        bail!("aborted; nothing was changed");
    }

    for path in integration::rescue_files(&paths) {
        if path.exists() {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
    }
    if let Some(esp) = &esp {
        for line in boot::hand_over(esp, &kernels, &default, sign)? {
            println!("boot: {line}");
        }
    }
    integration::remove(&paths)?;
    let _ = Command::new("systemctl").arg("daemon-reload").status();
    if let Some(generation) = &current {
        let mut files = generation.system.files.clone();
        // mkinitcpio reads the kernel command line for UKIs from here.
        files.remove("/etc/kernel/cmdline");
        if restore_originals {
            let change = SystemChange {
                from: SystemState {
                    files,
                    ..SystemState::default()
                },
                to: SystemState::default(),
            };
            archix::system::apply::files(&paths, &change)?;
        } else {
            for (path, file) in &files {
                let writable = file.mode | 0o200;
                if file.link.is_none() && file.merge.is_none() && writable != file.mode {
                    let _ = fs::set_permissions(path, fs::Permissions::from_mode(writable));
                }
            }
        }
    }
    if purge {
        for generation in store.list()? {
            if let Some(path) = &generation.snapshot
                && let Err(e) = snapshot::delete(path)
            {
                eprintln!("warning: couldn't delete the snapshot {path}: {e:#}");
            }
        }
        for dir in [&paths.cache, &paths.state] {
            match fs::remove_dir_all(dir) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    eprintln!("warning: couldn't delete all of {}: {e}", dir.display());
                }
                _ => {}
            }
        }
    }
    println!(
        "archix no longer manages this machine. `pacman -Rns archix` removes archix itself; `archix switch` would take the machine back"
    );
    Ok(())
}

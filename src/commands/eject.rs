use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use anyhow::{Result, bail};

use archix::deploy::eject as eject_deployments;
use archix::generations::Store;
use archix::integration;
use archix::paths::Paths;
use archix::system::apply::{Accounts, SystemChange};
use archix::system::{SystemState, boot};

use super::{check_pacman_free, confirm, require_root};

/// Stops archix managing the machine: systemd-boot starts the distro's kernels without it, and its hooks and boot service come off; a system on deployments becomes plain Arch first.
pub fn eject(noconfirm: bool, restore_originals: bool, purge: bool) -> Result<()> {
    require_root("eject")?;
    let paths = Paths::system();
    check_pacman_free(&paths)?;
    let under_way = eject_deployments::read_state(&paths.state)?;
    if under_way.is_some() || archix::system::deploy_boot::uses_deployments(&paths.root) {
        return from_deployments(&paths, under_way, noconfirm, restore_originals, purge);
    }
    let store = Store::new(&paths);
    let current = match store.current()? {
        Some(number) => Some(store.get(number)?),
        None => None,
    };
    let esp = fs::read_to_string(paths.state.join("boot/esp"))
        .ok()
        .map(|esp| esp.trim().to_string())
        .filter(|esp| !boot::entries(std::path::Path::new(esp)).is_empty());
    let kernels = boot::installed_kernels();
    let default = current
        .as_ref()
        .and_then(|generation| generation.settings.boot.as_ref())
        .map_or_else(|| "linux".to_string(), |boot| boot.kernel.clone());
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
            boot::entries(std::path::Path::new(esp)).len()
        );
    }
    println!(
        "  removes   archix-boot.service and archix-boot-complete.service, the deployment units and initramfs hook, the pacman hook and the login notices"
    );
    if restore_originals {
        println!(
            "  files     puts back the files archix replaced and deletes the ones it created, except /etc/kernel/cmdline"
        );
    } else {
        println!("  files     stay as they are, made writable again (--restore-originals puts the originals back)");
    }
    println!("  keeps     packages, users, groups, services and pacman.conf");
    if purge {
        println!("  deletes   /var/lib/archix and /var/cache/archix");
    } else {
        println!("  keeps     /var/lib/archix and /var/cache/archix (--purge deletes them)");
    }
    if !noconfirm && !confirm("eject?")? {
        bail!("aborted; nothing was changed");
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
            let root = paths.root.clone();
            let live = move |path: &str| Some(root.join(path.trim_start_matches('/')));
            archix::system::apply::files(&paths, &change, &live, &Accounts::read(&paths.root.join("etc")))?;
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
        purge_state(&paths);
    }
    println!(
        "archix no longer manages this machine. `pacman -Rns archix` removes archix itself; `archix switch` would take the machine back"
    );
    Ok(())
}

fn purge_state(paths: &Paths) {
    for dir in [&paths.cache, &paths.state] {
        match fs::remove_dir_all(dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                eprintln!("warning: couldn't delete all of {}: {e}", dir.display());
            }
            _ => {}
        }
    }
}

/// Ejects a system that boots through deployments, or finishes an eject a crash cut off: the running deployment becomes the root's own trees, the ESP boots them through a plain UKI, and /archix goes.
fn from_deployments(
    paths: &Paths,
    under_way: Option<eject_deployments::State>,
    noconfirm: bool,
    restore_originals: bool,
    purge: bool,
) -> Result<()> {
    let restore_originals = under_way
        .as_ref()
        .map_or(restore_originals, |state| state.restore_originals);
    match &under_way {
        Some(state) => println!(
            "archix eject: finishing the eject to plain Arch from deployment {} that was cut off",
            state.deployment
        ),
        None => {
            let deployment = eject_deployments::deployment_to_eject(&paths.root)?;
            println!("archix eject: archix stops managing this machine, which becomes plain Arch");
            println!(
                "  system    deployment {deployment}, which runs now, becomes the root's own /usr, /etc, /opt, /boot and pacman database; the running system switches to them at once"
            );
            println!(
                "  boot      a UKI that mkinitcpio's preset writes to EFI/Linux{}, the default instead of archix's entries, which go with the fallback UKI",
                if boot::sbctl_has_keys() {
                    ", signed with sbctl"
                } else {
                    ""
                }
            );
            println!("  deletes   /archix and every deployment in it");
            println!(
                "  removes   archix's boot units, the deployment initramfs hook and units, the pacman hooks and the login notices; pacman.conf's CheckSpace is back on"
            );
            if restore_originals {
                println!(
                    "  files     puts back the files archix replaced and deletes the ones it created, except /etc/kernel/cmdline"
                );
            } else {
                println!(
                    "  files     stay as they are, made writable again (--restore-originals puts the originals back)"
                );
            }
            println!("  keeps     packages, users, groups, services and pacman.conf");
        }
    }
    if purge {
        println!("  deletes   /var/lib/archix and /var/cache/archix");
    } else {
        println!("  keeps     /var/lib/archix and /var/cache/archix (--purge deletes them)");
    }
    if !noconfirm && !confirm("eject?")? {
        bail!("aborted; nothing was changed");
    }
    let archix = archix::integration::own_path()?;
    let written = eject_deployments::eject(paths, &archix, restore_originals).map_err(|e| {
        e.context(
            "the eject stopped partway; the machine still boots, and running `sudo archix eject` again finishes it",
        )
    })?;
    for line in written {
        println!("boot: {line}");
    }
    if paths.root == std::path::Path::new("/") {
        let _ = Command::new("systemctl").arg("daemon-reload").status();
    }
    if purge {
        purge_state(paths);
    } else {
        eject_deployments::clear_state(&paths.state)?;
    }
    println!(
        "this machine is plain Arch now and archix no longer manages it; restart when convenient. `pacman -Rns archix` removes archix itself; `archix switch` would lay the machine out as deployments again"
    );
    Ok(())
}

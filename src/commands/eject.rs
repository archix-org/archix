use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use anyhow::{Result, bail};

use archix::deploy::eject::{self as eject_deployments, SecretsMode};
use archix::generations::Store;
use archix::integration;
use archix::paths::Paths;
use archix::system::apply::{Accounts, SystemChange};
use archix::system::{SystemState, boot};

use super::{check_pacman_free, confirm, require_root};

/// What `archix eject` was asked to do.
#[derive(Debug, Clone, Copy)]
pub struct How {
    pub noconfirm: bool,
    pub restore_originals: bool,
    pub purge: bool,
    pub discard_pending: bool,
}

/// Stops archix managing the machine: systemd-boot starts the distro's kernels without it, and its hooks and boot service come off; a system on deployments becomes plain Arch first, from the deployment that runs.
pub fn eject(how: How, secrets: Option<SecretsMode>) -> Result<()> {
    let How {
        noconfirm,
        restore_originals,
        purge,
        ..
    } = how;
    require_root("eject")?;
    let paths = Paths::system();
    check_pacman_free(&paths)?;
    let under_way = eject_deployments::read_state(&paths.state)?;
    let target = match &under_way {
        None if archix::system::deploy_boot::uses_deployments(&paths.root) => {
            Some(eject_deployments::deployment_to_eject(&paths.root)?)
        }
        _ => None,
    };
    let store = Store::new(&paths);
    // The generation whose files and secrets come off: the deployment eject lays out, else current.
    let current = match (&under_way, &target) {
        (Some(state), _) => store.get(state.deployment).ok(),
        (None, Some(target)) => Some(store.get(target.deployment)?),
        (None, None) => match store.current()? {
            Some(number) => Some(store.get(number)?),
            None => None,
        },
    };
    let declared = current
        .as_ref()
        .and_then(|generation| generation.system.secrets.as_ref())
        .filter(|manifest| !manifest.secrets.is_empty());
    let secrets = match (declared, secrets, &under_way) {
        (_, _, Some(state)) => state.secrets,
        (Some(manifest), None, None) => bail!(
            "generation {} declares {} secrets, which plain Arch has nothing to decrypt; run eject with --secrets drop (their links go) or --secrets materialize (each becomes a real file at its path, from what this boot decrypted); nothing was changed",
            current.as_ref().map_or(0, |g| g.number),
            manifest.secrets.len()
        ),
        (_, mode, None) => mode.unwrap_or_default(),
    };
    if secrets == SecretsMode::Materialize
        && archix::system::secureboot::in_chroot()
        && under_way
            .as_ref()
            .is_none_or(|state| state.phase < eject_deployments::Phase::Copied)
    {
        bail!(
            "--secrets materialize copies what a boot decrypted into {}, and nothing in a chroot did; eject with --secrets drop here (the identity and ciphertexts stay in /var/lib/archix/secrets), or with --secrets materialize from the booted system; nothing was changed",
            archix::secrets::RUN_DIR
        );
    }
    if secrets == SecretsMode::Materialize
        && under_way.is_none()
        && declared.is_some()
        && let Some(report) = archix::secrets::activate::read_report(&paths.root)
        && !report.missing.is_empty()
    {
        bail!(
            "--secrets materialize copies what this boot decrypted, and {} weren't: {}; nothing was changed",
            report.missing.len(),
            report.missing.keys().cloned().collect::<Vec<_>>().join(", ")
        );
    }
    if declared.is_some() && under_way.is_none() {
        println!(
            "  secrets   {}",
            match secrets {
                SecretsMode::Drop => "their links go; the identity and ciphertexts stay in /var/lib/archix/secrets",
                SecretsMode::Materialize =>
                    "each becomes a real file at its path, holding what this boot decrypted, readable by whoever reads the disk",
            }
        );
    }
    if under_way.is_some() || target.is_some() {
        return from_deployments(&paths, under_way, target, how, secrets);
    }
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
        if let Some(manifest) = &generation.system.secrets {
            for line in eject_deployments::eject_secrets(&paths.root, &paths.root, manifest, secrets)? {
                println!("secrets: {line}");
            }
        }
        let mut files = generation.system.files.clone();
        files.retain(|_, file| !file.link.as_deref().is_some_and(archix::secrets::is_secret_link));
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

/// Ejects a system that boots through deployments, or finishes an eject a crash cut off: the deployment that runs, booted or bound for a live USB's chroot, becomes the root's own trees, the ESP boots them through a plain UKI, and /archix goes with every other deployment, current included.
fn from_deployments(
    paths: &Paths,
    under_way: Option<eject_deployments::State>,
    target: Option<eject_deployments::Target>,
    how: How,
    secrets: SecretsMode,
) -> Result<()> {
    let How {
        noconfirm,
        restore_originals,
        purge,
        discard_pending,
    } = how;
    let chroot = archix::system::secureboot::in_chroot();
    let restore_originals = under_way
        .as_ref()
        .map_or(restore_originals, |state| state.restore_originals);
    match (&under_way, target) {
        (Some(state), _) => println!(
            "archix eject: finishing the eject to plain Arch from deployment {} that was cut off",
            state.deployment
        ),
        (None, None) => bail!("there's no deployment to eject to; nothing was changed"),
        (None, Some(target)) => {
            let held = archix::system::usr_local::boot_files(&paths.root);
            if archix::system::usr_local::read_hold(&paths.state).is_some() && !held.is_empty() {
                bail!(
                    "/usr/local is held out of every boot, and eject would make /var/usrlocal, with the files systemd, udev or the module loader read at boot ({}), the root's own /usr/local, which every boot then reads; `sudo archix usr-local quarantine` moves them away, or fix them, `sudo archix usr-local release` and boot with them first; nothing was changed",
                    held.join(", ")
                );
            }
            let (deployment, current) = (target.deployment, target.current);
            let runs = if chroot { "this chroot runs" } else { "this boot runs" };
            if current != deployment && noconfirm && !discard_pending {
                bail!(
                    "deployment {current} is current, but {runs} deployment {deployment}, which eject lays out; deployment {current} would go with /archix. Run eject again with --discard-pending to do that, or restart into {current} and eject there; nothing was changed"
                );
            }
            if let Some(esp) = eject_deployments::esp(paths) {
                boot::require_esp(&esp).map_err(|e| match chroot {
                    true => e.context(format!(
                        "from the live system, mount the ESP at ROOT{} before `arch-chroot ROOT`",
                        esp.display()
                    )),
                    false => e,
                })?;
            }
            println!("archix eject: archix stops managing this machine, which becomes plain Arch");
            if chroot {
                println!(
                    "  system    deployment {deployment}, which this chroot runs (bound by mount-deployment), becomes the root's own /usr, /etc, /opt, /boot and pacman database"
                );
            } else {
                println!(
                    "  system    deployment {deployment}, which runs now, becomes the root's own /usr, /etc, /opt, /boot and pacman database; the running system switches to them at once"
                );
            }
            if current != deployment {
                let store = archix::deploy::Store::open(&paths.root)?;
                let booted = archix::system::activate::activated(&paths.state, &store.path(current), current);
                println!(
                    "  pending   deployment {current}, {}, is discarded: its packages and config never become the system{}; to keep it, restart into it and eject there",
                    if booted {
                        "the current one"
                    } else {
                        "which hasn't booted"
                    },
                    if booted {
                        ""
                    } else {
                        ", and the hooks and /var changes waiting for its first boot never run"
                    }
                );
            }
            println!("  /usr/local the shared /var/usrlocal becomes the root's own /usr/local, and /var/usrlocal goes");
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
        println!("  deletes   /var/lib/archix and /var/cache/archix, this machine's secrets identity with them");
    } else {
        println!("  keeps     /var/lib/archix and /var/cache/archix (--purge deletes them)");
    }
    if !noconfirm && !confirm("eject?")? {
        bail!("aborted; nothing was changed");
    }
    let archix = archix::integration::own_path()?;
    let written = eject_deployments::eject(paths, &archix, restore_originals, secrets).map_err(|e| {
        e.context(match chroot {
            true => {
                "the eject stopped partway; the deployments still boot, and running `archix eject` again in this chroot finishes it"
            }
            false => {
                "the eject stopped partway; the machine still boots, and running `sudo archix eject` again finishes it"
            }
        })
    })?;
    for line in written {
        println!("boot: {line}");
    }
    if paths.root == std::path::Path::new("/") && !chroot {
        let _ = Command::new("systemctl").arg("daemon-reload").status();
    }
    if purge {
        purge_state(paths);
    } else {
        eject_deployments::clear_state(&paths.state)?;
    }
    if chroot {
        println!(
            "this root is plain Arch now and archix no longer manages it; leave the chroot (`exit`), `umount -R` the root and restart. `pacman -Rns archix` removes archix itself; `archix switch` would lay the machine out as deployments again"
        );
    } else {
        println!(
            "this machine is plain Arch now and archix no longer manages it; restart when convenient. `pacman -Rns archix` removes archix itself; `archix switch` would lay the machine out as deployments again"
        );
    }
    Ok(())
}

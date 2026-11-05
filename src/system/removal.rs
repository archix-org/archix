//! Removing archix with pacman: the step 00-archix-removal.hook arms and the step after the transaction runs once archix is gone, which takes archix's units, initcpio hook and pacman hooks off and hands the ESP to plain systemd-boot.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::paths::Paths;

/// Where the removal hook copies the static archix, which the step after the transaction runs once the package is gone.
pub const COPY: &str = "/run/archix/archix-removed";

/// Every file archix writes outside the snapshots for its own integration, below `root`: the pacman hooks and strict script, the boot and secrets units and their links, the bless, emergency and timeout drop-ins, the initcpio hook, the /var/lib split's unit and the units making declared swap files.
pub fn integration_files(paths: &Paths, root: &Path) -> Vec<PathBuf> {
    let archix = Path::new("/usr/bin/archix");
    let mut files: Vec<PathBuf> = crate::integration::files(paths, archix)
        .into_iter()
        .chain(crate::integration::boot_files(archix))
        .chain(super::initrd::hook_files())
        .map(|(path, _)| path)
        .chain(crate::integration::boot_wants().into_iter().map(|(link, _)| link))
        .chain([
            PathBuf::from(super::health::TIMEOUT_DROP_IN),
            PathBuf::from(super::emergency::DROP_IN),
            Path::new("/etc/systemd/system").join(crate::snapshot::layout::SPLIT_UNIT),
            Path::new("/etc/systemd/system/var-lib.mount.requires").join(crate::snapshot::layout::SPLIT_UNIT),
        ])
        .collect();
    let units = root.join("etc/systemd/system");
    for entry in fs::read_dir(&units).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("archix-swapfile-") && name.ends_with(".service") {
            files.push(Path::new("/etc/systemd/system").join(&name));
        }
        if name.ends_with(".swap.d") {
            files.push(
                Path::new("/etc/systemd/system")
                    .join(&name)
                    .join("archix-swapfile.conf"),
            );
        }
    }
    files
}

/// Deletes `integration_files` below the root, and the directories they leave empty; returns what went, absolute.
pub fn remove_integration(paths: &Paths) -> Vec<PathBuf> {
    let root = &paths.root;
    let mut removed = Vec::new();
    for path in integration_files(paths, root) {
        let at = root.join(path.strip_prefix("/").unwrap_or(&path));
        if fs::symlink_metadata(&at).is_ok() && fs::remove_file(&at).is_ok() {
            if let Some(dir) = at.parent() {
                let _ = fs::remove_dir(dir);
            }
            removed.push(path);
        }
    }
    let _ = fs::remove_dir(root.join("etc/initcpio/archix"));
    removed
}

/// A kernel preset's text with `default_uki` writing to `uki`, so kernel updates without archix build what systemd-boot lists by itself.
fn preset_with_uki(text: &str, uki: &Path) -> String {
    let line = format!("default_uki=\"{}\"", uki.display());
    if text.lines().any(|existing| existing.trim() == line) {
        return text.to_string();
    }
    format!(
        "{}\n# archix was removed: a UKI on the ESP, which systemd-boot lists by itself\n{line}\n",
        text.trim_end()
    )
}

/// loader.conf with `default` as its only default and no `preferred`, which named archix's entries.
fn loader_without_archix(text: &str, default: &str) -> String {
    let mut lines: Vec<String> = text
        .lines()
        .filter(|line| !matches!(line.split_whitespace().next(), Some("default" | "preferred")))
        .map(str::to_string)
        .collect();
    lines.push(format!("default {default}"));
    lines.join("\n") + "\n"
}

/// Hands the ESP at `esp` to plain systemd-boot: each installed kernel's preset writes a UKI to EFI/Linux, which is built now in the root (signed with sbctl when `sign`), the main kernel's becomes loader.conf's default, and archix's entries, images and rescue entry go. Fails, leaving archix's entries as they are, when a UKI can't be built or signed.
pub fn hand_over(paths: &Paths, esp: &Path, main: &str, sign: bool) -> Result<Vec<String>> {
    let root = super::live::Root::new(&paths.root);
    let inside = |path: &str| paths.root.join(path.trim_start_matches('/'));
    let kernels = super::boot::kernels_in(&super::boot::BootRoot::local_db(&root));
    if kernels.is_empty() {
        bail!("no installed kernel package owns a modules directory");
    }
    let linux = esp.join("EFI/Linux");
    fs::create_dir_all(&linux).with_context(|| format!("creating {}", linux.display()))?;
    let mut written: Vec<PathBuf> = Vec::new();
    let mut default = None;
    let built = |base: &str| -> Result<PathBuf> {
        let preset = inside(&format!("/etc/mkinitcpio.d/{base}.preset"));
        let text = fs::read_to_string(&preset).with_context(|| format!("reading {}", preset.display()))?;
        let uki = esp.join("EFI/Linux").join(format!("arch-{base}.efi"));
        crate::durable::write(&preset, preset_with_uki(&text, &uki).as_bytes())?;
        let inner = format!("{}/arch-{base}.efi", super::boot::BUILT);
        root.run(&["mkdir", "-p", super::boot::BUILT])?;
        let version = kernels
            .iter()
            .find(|(name, _)| name == base)
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        root.run(&["mkinitcpio", "-k", &version, "-U", &inner])?;
        Ok(inside(&inner))
    };
    for (base, _) in &kernels {
        let uki = linux.join(format!("arch-{base}.efi"));
        let made = built(base).and_then(|file| {
            crate::durable::copy(&file, &uki)?;
            let _ = fs::remove_file(&file);
            if sign {
                let status = std::process::Command::new("sbctl")
                    .env("ESP_PATH", esp)
                    .args(["sign", "-s"])
                    .arg(&uki)
                    .status()
                    .context("running sbctl")?;
                if !status.success() {
                    bail!("sbctl couldn't sign {}", uki.display());
                }
            }
            Ok(())
        });
        if let Err(e) = made {
            for file in &written {
                let _ = fs::remove_file(file);
            }
            let _ = fs::remove_file(&uki);
            return Err(e).with_context(|| format!("building {base}'s UKI for plain systemd-boot"));
        }
        written.push(uki);
        if base == main || default.is_none() {
            default = Some(format!("arch-{base}.efi"));
        }
    }
    let default = default.unwrap_or_default();
    let conf = esp.join("loader/loader.conf");
    let text = fs::read_to_string(&conf).unwrap_or_default();
    crate::durable::write_esp(&conf, loader_without_archix(&text, &default).as_bytes())?;
    crate::durable::sync_esp(esp)?;
    for (_, name) in super::menu::on_esp(esp) {
        let _ = fs::remove_file(esp.join("loader/entries").join(name));
    }
    crate::durable::sync_esp(esp)?;
    let _ = super::rescue::remove(paths, esp);
    let _ = fs::remove_dir_all(esp.join(super::menu::IMAGES));
    crate::durable::sync_esp(esp)?;
    let mut lines = vec![format!("loader.conf's default is {default}")];
    lines.extend(written.iter().map(|path| format!("wrote {}", path.display())));
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preset_gets_a_uki_line_once() {
        let preset = "PRESETS=('default')\ndefault_image=\"/boot/initramfs-linux.img\"\n";
        let once = preset_with_uki(preset, Path::new("/efi/EFI/Linux/arch-linux.efi"));
        assert!(
            once.ends_with("default_uki=\"/efi/EFI/Linux/arch-linux.efi\"\n"),
            "{once}"
        );
        assert_eq!(preset_with_uki(&once, Path::new("/efi/EFI/Linux/arch-linux.efi")), once);
    }

    #[test]
    fn loader_conf_loses_archixs_default_and_preferred() {
        let text = "timeout 0\ndefault archix-current.conf\npreferred archix-*\neditor no\n";
        assert_eq!(
            loader_without_archix(text, "arch-linux.efi"),
            "timeout 0\neditor no\ndefault arch-linux.efi\n"
        );
    }

    #[test]
    fn the_integration_files_cover_units_links_initcpio_hooks_and_swap_file_units() {
        let root = tempfile::tempdir().unwrap();
        let units = root.path().join("etc/systemd/system");
        fs::create_dir_all(units.join("swap-swapfile.swap.d")).unwrap();
        fs::write(units.join("archix-swapfile-swap-swapfile.service"), "").unwrap();
        let paths = Paths::under(root.path());
        let files = integration_files(&paths, root.path());
        for wanted in [
            "/etc/systemd/system/archix-boot.service",
            "/etc/systemd/system/archix-health-check.service",
            "/etc/systemd/system/sysinit.target.wants/archix-boot.service",
            "/etc/systemd/system/systemd-bless-boot.service.d/archix.conf",
            "/etc/initcpio/install/archix",
            "/etc/mkinitcpio.conf.d/archix.conf",
            "/usr/share/libalpm/hooks/01-archix-snapshot.hook",
            "/etc/systemd/system/archix-swapfile-swap-swapfile.service",
            "/etc/systemd/system/swap-swapfile.swap.d/archix-swapfile.conf",
            "/etc/systemd/system/emergency.service.d/archix-required-mounts.conf",
        ] {
            assert!(files.contains(&PathBuf::from(wanted)), "{wanted} in {files:?}");
        }
        fs::create_dir_all(root.path().join("etc/initcpio/install")).unwrap();
        fs::write(root.path().join("etc/initcpio/install/archix"), "x").unwrap();
        fs::write(root.path().join("etc/initcpio/install/other"), "x").unwrap();
        let removed = remove_integration(&paths);
        assert!(removed.contains(&PathBuf::from("/etc/initcpio/install/archix")));
        assert!(
            root.path().join("etc/initcpio/install/other").exists(),
            "another hook stays"
        );
        assert!(!units.join("archix-swapfile-swap-swapfile.service").exists());
    }
}

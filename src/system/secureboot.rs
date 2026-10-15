use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

const GLOBAL: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";
/// What Easy Anti-Cheat Linux v2 asks of the boot session's kernel command line, besides a kernel signed with a well-known key.
pub const ANTICHEAT_CMDLINE: &str = "lockdown=integrity sysctl.kernel.kexec_load_disabled=1 lsm=landlock,lockdown,yama,integrity,bpf iommu.passthrough=0";

/// The firmware's Secure Boot state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct State {
    pub enabled: bool,
    pub setup_mode: bool,
}

/// Reads firmware state and kernel facts under `root`, which is `/` except in tests.
pub struct Machine {
    root: PathBuf,
}

impl Machine {
    pub fn new() -> Machine {
        Machine {
            root: PathBuf::from("/"),
        }
    }

    pub fn under(root: &Path) -> Machine {
        Machine {
            root: root.to_path_buf(),
        }
    }

    fn read(&self, path: &str) -> Option<String> {
        fs::read_to_string(self.root.join(path.trim_start_matches('/'))).ok()
    }

    fn efivar(&self, name: &str) -> Option<u8> {
        let bytes = fs::read(self.root.join(format!("sys/firmware/efi/efivars/{name}-{GLOBAL}"))).ok()?;
        bytes.get(4).copied()
    }

    /// None when the machine didn't boot through UEFI; bootctl answers when the variables can't be read directly.
    pub fn state(&self) -> Option<State> {
        if !self.root.join("sys/firmware/efi").is_dir() {
            return None;
        }
        if let (Some(enabled), Some(setup)) = (self.efivar("SecureBoot"), self.efivar("SetupMode")) {
            return Some(State {
                enabled: enabled == 1,
                setup_mode: setup == 1,
            });
        }
        let status = if self.root == Path::new("/") {
            Command::new("bootctl")
                .arg("status")
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default()
        } else {
            String::new()
        };
        let line = status
            .lines()
            .find(|line| line.trim_start().starts_with("Secure Boot:"))
            .unwrap_or_default();
        Some(State {
            enabled: self.efivar("SecureBoot") == Some(1) || line.contains(": enabled"),
            setup_mode: self.efivar("SetupMode") == Some(1) || line.contains("(setup)"),
        })
    }

    /// One line per requirement, true when met; with `anticheat`, also what Easy Anti-Cheat Linux v2 checks.
    pub fn check(&self, anticheat: bool) -> Vec<(bool, String)> {
        let mut lines = Vec::new();
        match self.state() {
            None => lines.push((false, "the machine didn't boot through UEFI".to_string())),
            Some(state) => {
                lines.push((state.enabled, "Secure Boot is on".to_string()));
                lines.push((
                    !state.setup_mode,
                    "the firmware isn't in setup mode (keys are enrolled)".to_string(),
                ));
            }
        }
        if !anticheat {
            return lines;
        }
        let cmdline = self.read("proc/cmdline").unwrap_or_default();
        lines.push((
            false,
            if cmdline.contains("archix.generation=") {
                "the kernel is signed with a well-known key (archix signs with your own key, and Arch has no well-known-signed kernel)"
            } else {
                "the kernel is signed with a well-known key (not booted from an archix entry, so archix can't tell)"
            }
            .to_string(),
        ));
        let lockdown = self.read("sys/kernel/security/lockdown").unwrap_or_default();
        lines.push((
            lockdown.contains("[integrity]"),
            "kernel lockdown is in integrity mode".to_string(),
        ));
        lines.push((
            self.read("proc/sys/kernel/kexec_load_disabled")
                .is_some_and(|v| v.trim() == "1"),
            "kexec is disabled".to_string(),
        ));
        lines.push((
            self.read("sys/kernel/security/lsm")
                .is_some_and(|lsm| lsm.split(',').any(|m| m.trim() == "bpf")),
            "the BPF LSM is active".to_string(),
        ));
        let iommu = fs::read_dir(self.root.join("sys/class/iommu")).is_ok_and(|mut d| d.next().is_some());
        let identity = fs::read_dir(self.root.join("sys/kernel/iommu_groups"))
            .into_iter()
            .flatten()
            .flatten()
            .any(|group| fs::read_to_string(group.path().join("type")).is_ok_and(|t| t.trim() == "identity"));
        lines.push((iommu && !identity, "the IOMMU is on, without passthrough".to_string()));
        lines.push((
            self.read("sys/class/tpm/tpm0/tpm_version_major")
                .is_some_and(|v| v.trim() == "2"),
            "a TPM 2.0 is present".to_string(),
        ));
        lines.push((
            self.root
                .join("sys/kernel/security/tpm0/binary_bios_measurements")
                .exists(),
            "the boot was measured into the TPM".to_string(),
        ));
        let tainted: u64 = self
            .read("proc/sys/kernel/tainted")
            .and_then(|t| t.trim().parse().ok())
            .unwrap_or(0);
        lines.push((
            tainted & (1 << 12 | 1 << 13) == 0,
            "no out-of-tree or unsigned kernel modules are loaded (NVIDIA and DKMS modules count)".to_string(),
        ));
        lines
    }
}

/// Inside a chroot (arch-chroot from a live USB, an image build) the firmware belongs to the host, not to the system being set up.
pub fn in_chroot() -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::metadata("/proc/1/root"), fs::metadata("/")) {
        (Ok(init), Ok(here)) => (init.dev(), init.ino()) != (here.dev(), here.ino()),
        _ => false,
    }
}

impl Default for Machine {
    fn default() -> Self {
        Machine::new()
    }
}

/// Whether a PE image (a UKI, a boot loader) carries an Authenticode signature.
pub fn is_signed(path: &Path) -> Result<bool> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let u16_at = |at: usize| bytes.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let u32_at = |at: usize| {
        bytes
            .get(at..at + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let Some(pe) = u32_at(0x3c).map(|offset| offset as usize) else {
        bail!("{} isn't a PE image", path.display());
    };
    if bytes.get(pe..pe + 4) != Some(b"PE\0\0") {
        bail!("{} isn't a PE image", path.display());
    }
    let optional = pe + 24;
    let directories = match u16_at(optional) {
        Some(0x20b) => optional + 112,
        Some(0x10b) => optional + 96,
        _ => bail!("{} has an unknown PE optional header", path.display()),
    };
    Ok(u32_at(directories + 4 * 8 + 4).unwrap_or(0) > 0)
}

/// Creates sbctl keys if needed and enrolls them, with Microsoft's unless told otherwise, while the firmware is in setup mode.
pub fn setup(microsoft: bool) -> Result<()> {
    let sbctl = |args: &[&str]| -> Result<()> {
        let status = Command::new("sbctl")
            .args(args)
            .status()
            .context("running sbctl; install it with pacman -S sbctl")?;
        if !status.success() {
            bail!("sbctl {} failed", args.join(" "));
        }
        Ok(())
    };
    let have_keys = Path::new("/var/lib/sbctl/keys/db/db.key").exists()
        || Path::new("/usr/share/secureboot/keys/db/db.key").exists();
    if !have_keys {
        sbctl(&["create-keys"])?;
    }
    match Machine::new().state() {
        None => bail!("this machine didn't boot through UEFI, so it has no Secure Boot"),
        Some(state) if !state.setup_mode => {
            println!("the keys exist, but the firmware isn't in setup mode, so they can't be enrolled");
            println!(
                "reboot into the firmware settings, reset or clear the Secure Boot keys (setup mode), then run `sudo archix secure-boot setup` again"
            );
            return Ok(());
        }
        Some(_) => {}
    }
    if microsoft {
        sbctl(&["enroll-keys", "--microsoft"])?;
    } else {
        sbctl(&["enroll-keys", "--yes-this-might-brick-my-machine"])?;
    }
    println!("enrolled your Secure Boot keys; the next `archix switch` signs its boot entries with them");
    println!("then turn Secure Boot on in the firmware settings");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pe(signed: bool) -> Vec<u8> {
        let mut bytes = vec![0u8; 512];
        bytes[0x3c] = 0x80;
        bytes[0x80..0x84].copy_from_slice(b"PE\0\0");
        let optional = 0x80 + 24;
        bytes[optional..optional + 2].copy_from_slice(&0x20bu16.to_le_bytes());
        if signed {
            let security = optional + 112 + 4 * 8;
            bytes[security..security + 4].copy_from_slice(&0x100u32.to_le_bytes());
            bytes[security + 4..security + 8].copy_from_slice(&0x40u32.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn signatures_are_read_from_the_pe_security_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (signed, unsigned) = (dir.path().join("signed.efi"), dir.path().join("unsigned.efi"));
        fs::write(&signed, pe(true)).unwrap();
        fs::write(&unsigned, pe(false)).unwrap();
        assert!(is_signed(&signed).unwrap());
        assert!(!is_signed(&unsigned).unwrap());
        fs::write(&unsigned, b"not a PE").unwrap();
        assert!(is_signed(&unsigned).is_err());
    }

    #[test]
    fn the_anticheat_check_reads_kernel_state() {
        let root = tempfile::tempdir().unwrap();
        let write = |path: &str, bytes: &[u8]| {
            let path = root.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        };
        write(
            &format!("sys/firmware/efi/efivars/SecureBoot-{GLOBAL}"),
            &[6, 0, 0, 0, 1],
        );
        write(
            &format!("sys/firmware/efi/efivars/SetupMode-{GLOBAL}"),
            &[6, 0, 0, 0, 0],
        );
        write("proc/cmdline", b"root=/dev/vda2 archix.generation=3 lockdown=integrity");
        write("sys/kernel/security/lockdown", b"none [integrity] confidentiality\n");
        write("proc/sys/kernel/kexec_load_disabled", b"1\n");
        write("sys/kernel/security/lsm", b"capability,landlock,lockdown,yama,bpf");
        write("sys/class/iommu/dmar0/x", b"");
        write("sys/kernel/iommu_groups/0/type", b"DMA-FQ\n");
        write("sys/class/tpm/tpm0/tpm_version_major", b"2\n");
        write("sys/kernel/security/tpm0/binary_bios_measurements", b"");
        write("proc/sys/kernel/tainted", b"4096\n");
        let machine = Machine::under(root.path());
        assert_eq!(
            machine.state(),
            Some(State {
                enabled: true,
                setup_mode: false
            })
        );
        let failed: Vec<String> = machine
            .check(true)
            .into_iter()
            .filter(|(ok, _)| !ok)
            .map(|(_, line)| line)
            .collect();
        assert_eq!(failed.len(), 2, "{failed:?}");
        assert!(failed[0].contains("well-known key") && failed[1].contains("out-of-tree"));
    }
}

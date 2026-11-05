//! Moving a Limine machine to systemd-boot without a boot it can't come back from: systemd-boot goes on the ESP and gets a firmware entry after Limine's, tried once through BootNext; only once a boot through it counted as good does it go first, take the removable-media path, and Limine's and limine-snapper-sync's hooks stop copying kernels to the ESP. Limine stays installed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::boot;
use crate::paths::Paths;

/// The firmware entry's description, which finds it again.
pub const LABEL: &str = "archix systemd-boot";
const LOADER: &str = "EFI/systemd/systemd-bootx64.efi";
const FALLBACK: &str = "EFI/BOOT/BOOTX64.EFI";
/// Where the removable-media loader the migration replaces is kept.
const FALLBACK_KEPT: &str = "EFI/BOOT/BOOTX64.EFI.before-archix";
const STATE: &str = "limine-migration.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// systemd-boot is on the ESP with an entry after Limine's, tried through BootNext until a boot through it counts as good.
    Trying,
    Done,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Migration {
    pub phase: Phase,
    pub entry: u16,
}

fn state_path(paths: &Paths) -> PathBuf {
    paths.state.join("boot").join(STATE)
}

pub fn load(paths: &Paths) -> Option<Migration> {
    serde_json::from_str(&fs::read_to_string(state_path(paths)).ok()?).ok()
}

fn save(paths: &Paths, migration: &Migration) -> Result<()> {
    crate::durable::write(&state_path(paths), serde_json::to_string_pretty(migration)?.as_bytes())
}

/// Whether a move from Limine to systemd-boot is due and not done: the config says systemd-boot and Limine is there.
pub fn pending(paths: &Paths, plan: &boot::BootPlan) -> bool {
    plan.loader == boot::Loader::SystemdBoot
        && present(paths, Path::new(&plan.esp))
        && load(paths).is_none_or(|m| m.phase != Phase::Done)
}

/// Whether Limine boots, or booted, this machine: its package, or its loader on the ESP.
pub fn present(paths: &Paths, esp: &Path) -> bool {
    let installed = fs::read_dir(paths.local_db())
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| crate::pacman::name_from_entry(&entry.file_name().to_string_lossy()) == "limine");
    installed || esp.join("EFI/limine").is_dir() || esp.join("limine.conf").is_file()
}

/// The firmware's boot entries and order, read from the EFI variables and changed with efibootmgr, or, on test efivars, written there directly.
pub struct Firmware {
    dir: PathBuf,
    test: bool,
}

impl Firmware {
    pub fn new() -> Option<Firmware> {
        let dir = crate::testhooks::efivars()?;
        let test = cfg!(debug_assertions) && std::env::var_os("ARCHIX_TEST_EFIVARS").is_some();
        Some(Firmware { dir, test })
    }

    fn var(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}-{}", boot::GLOBAL_GUID))
    }

    pub fn order(&self) -> Vec<u16> {
        fs::read(self.var("BootOrder"))
            .unwrap_or_default()
            .get(4..)
            .unwrap_or_default()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect()
    }

    /// Entry `number`'s description and file paths.
    pub fn option(&self, number: u16) -> Option<(String, Vec<String>)> {
        boot::load_option(&fs::read(self.var(&format!("Boot{number:04X}"))).ok()?)
    }

    /// The entry starting `loader` (lowercase, forward slashes) from the ESP, if one does.
    fn find(&self, loader: &str) -> Option<u16> {
        (0..=0xFFFFu16).take(0x100).find(|n| {
            self.option(*n)
                .is_some_and(|(_, paths)| paths.iter().any(|path| path.ends_with(loader)))
        })
    }

    fn write_test(&self, name: &str, data: &[u8]) -> Result<()> {
        let mut bytes = vec![7u8, 0, 0, 0];
        bytes.extend_from_slice(data);
        fs::write(self.var(name), bytes).with_context(|| format!("writing {name}"))
    }

    fn efibootmgr(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("efibootmgr")
            .args(args)
            .output()
            .context("running efibootmgr")?;
        if !output.status.success() {
            bail!(
                "efibootmgr {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    pub fn set_order(&self, order: &[u16]) -> Result<()> {
        match self.test {
            true => self.write_test(
                "BootOrder",
                &order.iter().flat_map(|n| n.to_le_bytes()).collect::<Vec<u8>>(),
            ),
            false => {
                let list: Vec<String> = order.iter().map(|n| format!("{n:04X}")).collect();
                self.efibootmgr(&["-q", "-o", &list.join(",")]).map(drop)
            }
        }
    }

    pub fn set_next(&self, number: u16) -> Result<()> {
        match self.test {
            true => self.write_test("BootNext", &number.to_le_bytes()),
            false => self.efibootmgr(&["-q", "-n", &format!("{number:04X}")]).map(drop),
        }
    }

    pub fn next(&self) -> Option<u16> {
        let bytes = fs::read(self.var("BootNext")).ok()?;
        Some(u16::from_le_bytes(bytes.get(4..6)?.try_into().ok()?))
    }

    /// Adds an entry starting `loader` on the ESP at `esp`, last in the boot order; returns its number.
    fn create(&self, esp: &Path, loader: &str) -> Result<u16> {
        if let Some(number) = self.find(&loader.to_lowercase()) {
            return Ok(number);
        }
        let before = self.order();
        if self.test {
            let number = (0..=0xFFFFu16)
                .find(|n| !self.var(&format!("Boot{n:04X}")).exists())
                .context("no free entry")?;
            let path: Vec<u8> = format!("\\{}", loader.replace('/', "\\"))
                .encode_utf16()
                .chain([0])
                .flat_map(u16::to_le_bytes)
                .collect();
            let mut list = vec![4u8, 4];
            list.extend_from_slice(&((path.len() + 4) as u16).to_le_bytes());
            list.extend_from_slice(&path);
            list.extend_from_slice(&[0x7f, 0xff, 4, 0]);
            let mut option = 1u32.to_le_bytes().to_vec();
            option.extend_from_slice(&(list.len() as u16).to_le_bytes());
            option.extend(LABEL.encode_utf16().chain([0]).flat_map(u16::to_le_bytes));
            option.extend_from_slice(&list);
            self.write_test(&format!("Boot{number:04X}"), &option)?;
        } else {
            let (disk, part) = disk_and_partition(esp)?;
            let loader = format!("\\{}", loader.replace('/', "\\"));
            self.efibootmgr(&[
                "-q", "--create", "--disk", &disk, "--part", &part, "--label", LABEL, "--loader", &loader,
            ])?;
        }
        let number = self
            .find(&loader.to_lowercase())
            .context("efibootmgr made no entry for systemd-boot")?;
        let mut order: Vec<u16> = before.into_iter().filter(|n| *n != number).collect();
        order.push(number);
        self.set_order(&order)?;
        Ok(number)
    }
}

/// The disk and partition number the ESP at `esp` is on, as efibootmgr takes them.
fn disk_and_partition(esp: &Path) -> Result<(String, String)> {
    let output = Command::new("findmnt")
        .args(["-n", "-o", "SOURCE", "-T"])
        .arg(esp)
        .output()
        .context("running findmnt")?;
    let source = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let name = Path::new(&source)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let part = fs::read_to_string(format!("/sys/class/block/{name}/partition"))
        .with_context(|| format!("{source} isn't a partition"))?
        .trim()
        .to_string();
    let disk = fs::canonicalize(format!("/sys/class/block/{name}/.."))?
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    Ok((format!("/dev/{disk}"), part))
}

/// systemd-boot's own file, signed for Secure Boot where the package ships one.
fn systemd_boot_binary(root: &Path) -> PathBuf {
    let dir = root.join("usr/lib/systemd/boot/efi");
    let signed = dir.join("systemd-bootx64.efi.signed");
    match signed.is_file() {
        true => signed,
        false => dir.join("systemd-bootx64.efi"),
    }
}

fn copy_loader(paths: &Paths, plan: &boot::BootPlan, to: &Path) -> Result<()> {
    let esp = Path::new(&plan.esp);
    crate::durable::copy_esp(&systemd_boot_binary(&paths.root), to)?;
    if plan.secure_boot.unwrap_or_else(boot::sbctl_has_keys) {
        boot::sign_replacing(esp, to)?;
    }
    crate::durable::sync_esp(esp)
}

/// After a switch wrote systemd-boot's menu on a Limine machine: puts systemd-boot on the ESP, signed under Secure Boot, with a firmware entry after Limine's, and has the firmware try it once at the next boot; Limine stays first, so a boot that fails goes back to it. Nothing when the migration is done already. Returns what to tell.
pub fn start(paths: &Paths, plan: &boot::BootPlan) -> Result<Option<String>> {
    let esp = Path::new(&plan.esp);
    if plan.loader != boot::Loader::SystemdBoot || !present(paths, esp) {
        return Ok(None);
    }
    if load(paths).is_some_and(|m| m.phase == Phase::Done) {
        return Ok(None);
    }
    let Some(firmware) = Firmware::new() else {
        return Ok(Some("Limine boots this machine; on a test root or in a chroot archix can't add systemd-boot's firmware entry, so the move to systemd-boot waits for a switch on the booted system".into()));
    };
    copy_loader(paths, plan, &esp.join(LOADER))?;
    crate::testhooks::mark("limine-loader-copied");
    let entry = firmware.create(esp, LOADER)?;
    crate::testhooks::mark("limine-entry-created");
    save(
        paths,
        &Migration {
            phase: Phase::Trying,
            entry,
        },
    )?;
    firmware.set_next(entry)?;
    Ok(Some(format!(
        "systemd-boot is on the ESP with a firmware entry ({entry:04X}) after Limine's, and the next boot tries it once; Limine stays first until a boot through systemd-boot counts as good, then systemd-boot goes first"
    )))
}

/// Whether the next boot goes through the systemd-boot a move from Limine puts on the ESP: the running loader's LoaderFeatures are Limine's then, which don't say systemd-boot counts boots, but that boot must be counted to count as good and finish the move.
pub fn next_boot_is_archix_systemd_boot(paths: &Paths, loader: boot::Loader, esp: &Path) -> bool {
    loader == boot::Loader::SystemdBoot && present(paths, esp) && !load(paths).is_some_and(|m| m.phase == Phase::Done)
}

/// Whether systemd-boot started this boot, by the LoaderInfo variable it sets.
fn booted_by_systemd_boot() -> bool {
    let Some(path) = crate::testhooks::efivars().map(|dir| dir.join(format!("LoaderInfo-{}", boot::LOADER_GUID)))
    else {
        return false;
    };
    let units: Vec<u16> = fs::read(path)
        .unwrap_or_default()
        .get(4..)
        .unwrap_or_default()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    String::from_utf16_lossy(&units).starts_with("systemd-boot")
}

/// Hook and unit files under the root at `root` whose name says Limine: limine-snapper-sync's and CachyOS's, which copy kernels to the ESP.
fn limine_files(root: &Path) -> (Vec<String>, Vec<String>) {
    let names = |dir: &str| -> Vec<String> {
        let mut found: Vec<String> = fs::read_dir(root.join(dir))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.to_lowercase().contains("limine"))
            .collect();
        found.sort();
        found
    };
    (names("usr/share/libalpm/hooks"), names("usr/lib/systemd/system"))
}

/// Once a boot through systemd-boot counted as good: systemd-boot goes first in the boot order and onto the removable-media path (the loader there kept beside it), signed under Secure Boot, and Limine's pacman hooks and units are masked so they stop copying kernels to the ESP. Returns what it did.
pub fn finish(paths: &Paths, plan: &boot::BootPlan) -> Result<Vec<String>> {
    let Some(mut migration) = load(paths).filter(|m| m.phase == Phase::Trying) else {
        return Ok(Vec::new());
    };
    if !booted_by_systemd_boot() {
        return Ok(vec!["archix: this boot didn't come through systemd-boot, so Limine stays first; the next switch has the firmware try systemd-boot again".to_string()]);
    }
    let firmware = Firmware::new().context("no EFI variables")?;
    let mut order: Vec<u16> = firmware.order().into_iter().filter(|n| *n != migration.entry).collect();
    order.insert(0, migration.entry);
    firmware.set_order(&order)?;
    crate::testhooks::mark("limine-order-set");
    let esp = Path::new(&plan.esp);
    let fallback = esp.join(FALLBACK);
    if fallback.is_file() && !esp.join(FALLBACK_KEPT).exists() && !is_systemd_boot(&fallback) {
        crate::durable::copy_esp(&fallback, &esp.join(FALLBACK_KEPT))?;
        // The kept copy is on the disk before the loader it keeps is replaced.
        crate::durable::sync_esp(esp)?;
    }
    copy_loader(paths, plan, &fallback)?;
    crate::testhooks::mark("limine-fallback-written");
    let (hooks, units) = limine_files(&paths.root);
    let masks = paths.root.join("etc/pacman.d/hooks");
    fs::create_dir_all(&masks).with_context(|| format!("creating {}", masks.display()))?;
    for hook in &hooks {
        let link = masks.join(hook);
        if fs::symlink_metadata(&link).is_err() {
            std::os::unix::fs::symlink("/dev/null", &link).with_context(|| format!("masking {}", link.display()))?;
        }
    }
    let unit_dir = paths.root.join("etc/systemd/system");
    fs::create_dir_all(&unit_dir)?;
    let running = super::live::Root::new(&paths.root).running();
    for unit in &units {
        if running {
            let _ = Command::new("systemctl")
                .args(["disable", "--now", "--quiet", unit])
                .status();
        }
        let link = unit_dir.join(unit);
        if fs::symlink_metadata(&link).is_err() {
            std::os::unix::fs::symlink("/dev/null", &link).with_context(|| format!("masking {}", link.display()))?;
        }
    }
    migration.phase = Phase::Done;
    save(paths, &migration)?;
    let mut said = vec![format!(
        "archix: this boot through systemd-boot counted as good, so systemd-boot is first in the boot order and at {FALLBACK}; Limine stays installed, after it"
    )];
    if !hooks.is_empty() || !units.is_empty() {
        said.push(format!(
            "archix: masked Limine's pacman hooks and units, which copied kernels to the ESP: {}",
            hooks.iter().chain(&units).cloned().collect::<Vec<_>>().join(" ")
        ));
    }
    Ok(said)
}

fn is_systemd_boot(path: &Path) -> bool {
    fs::read(path).is_ok_and(|bytes| bytes.windows(29).any(|w| w == b"#### LoaderInfo: systemd-boot"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_menu_written_for_the_move_from_limine_counts_its_boots_whatever_limine_reports() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        let esp = root.path().join("efi");
        fs::create_dir_all(esp.join("EFI/limine")).unwrap();
        assert!(
            next_boot_is_archix_systemd_boot(&paths, boot::Loader::SystemdBoot, &esp),
            "the boot after the switch goes through the systemd-boot archix puts on the ESP"
        );
        assert!(
            !next_boot_is_archix_systemd_boot(&paths, boot::Loader::Limine, &esp),
            "staying on Limine"
        );
        save(
            &paths,
            &Migration {
                phase: Phase::Done,
                entry: 8,
            },
        )
        .unwrap();
        assert!(
            !next_boot_is_archix_systemd_boot(&paths, boot::Loader::SystemdBoot, &esp),
            "once moved, systemd-boot's own LoaderFeatures say"
        );
        fs::remove_dir_all(esp.join("EFI/limine")).unwrap();
        fs::remove_file(state_path(&paths)).unwrap();
        assert!(
            !next_boot_is_archix_systemd_boot(&paths, boot::Loader::SystemdBoot, &esp),
            "no Limine, nothing to move from"
        );
    }
}

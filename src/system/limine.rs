//! Moving a Limine machine to systemd-boot without a boot it can't come back from: systemd-boot goes on the ESP and gets a firmware entry after Limine's, tried once through BootNext and, on firmware that ignored that, first in the boot order with Limine second; what the firmware holds is read back, and each boot records which loader started it. Only once a boot through systemd-boot counted as good does it take the removable-media path, and Limine's and limine-snapper-sync's hooks stop copying kernels to the ESP. Limine stays installed.

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
/// A systemd-boot menu entry that starts Limine, so the menu leads back to it by hand.
const CHAIN_ENTRY: &str = "loader/entries/limine-before-archix.conf";
/// `archix switch`'s flag that changes packages on a boot Limine started.
pub const ALLOW_FLAG: &str = "--allow-limine-boot";
const UNPROTECTED: &str = "boot counting, fallback and the rescue entry are not in effect";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// systemd-boot is on the ESP with an entry after Limine's, tried through BootNext until a boot through it counts as good.
    Trying,
    Done,
}

/// How the firmware is asked to start systemd-boot for a trial boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Method {
    /// BootNext names the entry, which stays after Limine's in the boot order.
    Next,
    /// The entry is also first in the boot order, Limine's second, for firmware that ignored BootNext.
    First,
}

impl Method {
    fn name(self) -> &'static str {
        match self {
            Method::Next => "BootNext",
            Method::First => "first place in the boot order",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Armed {
    /// The boot id of the boot that armed the trial; a later boot is the trial.
    pub boot: String,
    pub method: Method,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Migration {
    pub phase: Phase,
    pub entry: u16,
    pub armed: Option<Armed>,
    /// The methods after which the firmware started Limine all the same.
    pub missed: Vec<Method>,
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

/// A firmware boot entry (Boot####): its partition by GPT PARTUUID and the file it starts, with forward slashes.
#[derive(Debug, Clone, PartialEq)]
pub struct BootEntry {
    pub number: u16,
    pub active: bool,
    pub label: String,
    pub partuuid: Option<String>,
    pub file: Option<String>,
}

impl BootEntry {
    fn starts(&self, loader: &str, partuuid: &str) -> bool {
        self.active
            && self.partuuid.as_deref() == Some(partuuid)
            && self
                .file
                .as_deref()
                .is_some_and(|file| file.trim_start_matches('/').eq_ignore_ascii_case(loader))
    }

    fn describe(&self) -> String {
        format!(
            "{:04X} \"{}\" partition {} file {}{}",
            self.number,
            self.label,
            self.partuuid.as_deref().unwrap_or("-"),
            self.file.as_deref().unwrap_or("-"),
            if self.active { "" } else { " (inactive)" }
        )
    }
}

/// An EFI GUID as text: its first three fields are little-endian.
fn guid_string(b: &[u8]) -> String {
    let hex = |bytes: &[u8]| bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    format!(
        "{:08x}-{:04x}-{:04x}-{}-{}",
        u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        u16::from_le_bytes([b[4], b[5]]),
        u16::from_le_bytes([b[6], b[7]]),
        hex(&b[8..10]),
        hex(&b[10..16])
    )
}

fn utf16(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .take_while(|unit| *unit != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// Parses an EFI load option as efivarfs gives it, 4 bytes of attributes first.
fn parse_entry(number: u16, bytes: &[u8]) -> Option<BootEntry> {
    let data = bytes.get(4..)?;
    let attributes = u32::from_le_bytes(data.get(..4)?.try_into().ok()?);
    let list_len = u16::from_le_bytes(data.get(4..6)?.try_into().ok()?) as usize;
    let label = utf16(data.get(6..)?);
    let start = 6 + (label.encode_utf16().count() + 1) * 2;
    let mut list = data.get(start..start + list_len)?;
    let mut entry = BootEntry {
        number,
        active: attributes & 1 == 1,
        label,
        partuuid: None,
        file: None,
    };
    while list.len() >= 4 {
        let len = u16::from_le_bytes([list[2], list[3]]) as usize;
        if len < 4 || len > list.len() {
            break;
        }
        match (list[0], list[1]) {
            // A hard drive node whose signature is a GPT partition GUID.
            (4, 1) if len >= 42 && list[41] == 2 => entry.partuuid = Some(guid_string(&list[24..40])),
            (4, 4) => entry.file = Some(utf16(&list[4..len]).replace('\\', "/")),
            _ => {}
        }
        list = &list[len..];
    }
    Some(entry)
}

/// The GPT PARTUUID test efivars' ESP has.
const TEST_PARTUUID: [u8; 16] = [
    0xa1, 0xb2, 0xc3, 0xd4, 0x11, 0x22, 0x33, 0x44, 0x80, 0x01, 1, 2, 3, 4, 5, 6,
];

impl Firmware {
    pub fn new() -> Option<Firmware> {
        let dir = crate::testhooks::efivars()?;
        let test = cfg!(debug_assertions) && std::env::var_os("ARCHIX_TEST_EFIVARS").is_some();
        Some(Firmware { dir, test })
    }

    fn var(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}-{}", boot::GLOBAL_GUID))
    }

    fn word(&self, name: &str) -> Option<u16> {
        let bytes = fs::read(self.var(name)).ok()?;
        Some(u16::from_le_bytes(bytes.get(4..6)?.try_into().ok()?))
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

    pub fn next(&self) -> Option<u16> {
        self.word("BootNext")
    }

    /// The entry the firmware started this boot from.
    pub fn current(&self) -> Option<u16> {
        self.word("BootCurrent")
    }

    pub fn entry(&self, number: u16) -> Option<BootEntry> {
        parse_entry(number, &fs::read(self.var(&format!("Boot{number:04X}"))).ok()?)
    }

    /// Every boot entry the firmware holds, by number.
    pub fn entries(&self) -> Vec<BootEntry> {
        let suffix = format!("-{}", boot::GLOBAL_GUID);
        let mut found: Vec<BootEntry> = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|file| {
                let name = file.file_name().to_string_lossy().into_owned();
                let hex = name.strip_suffix(&suffix)?.strip_prefix("Boot")?;
                let number = u16::from_str_radix(hex, 16).ok().filter(|_| hex.len() == 4)?;
                self.entry(number)
            })
            .collect();
        found.sort_by_key(|entry| entry.number);
        found
    }

    /// The loader that started this boot as it names itself in LoaderInfo, which systemd-boot sets.
    pub fn loader_info(&self) -> Option<String> {
        let bytes = fs::read(self.dir.join(format!("LoaderInfo-{}", boot::LOADER_GUID))).ok()?;
        Some(utf16(bytes.get(4..)?)).filter(|info| !info.is_empty())
    }

    pub fn booted_by_systemd_boot(&self) -> bool {
        self.loader_info().is_some_and(|info| info.starts_with("systemd-boot"))
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
            .context("running efibootmgr (the efibootmgr package)")?;
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

    fn delete(&self, number: u16) -> Result<()> {
        match self.test {
            true => {
                let order: Vec<u16> = self.order().into_iter().filter(|n| *n != number).collect();
                self.set_order(&order)?;
                fs::remove_file(self.var(&format!("Boot{number:04X}"))).context("removing a boot entry")
            }
            false => self.efibootmgr(&["-q", "-b", &format!("{number:04X}"), "-B"]).map(drop),
        }
    }

    /// The GPT PARTUUID of the partition the ESP at `esp` is.
    fn esp_partuuid(&self, esp: &Path) -> Result<String> {
        if self.test {
            return Ok(guid_string(&TEST_PARTUUID));
        }
        let device = esp_device(esp)?.0;
        let probes: [(&str, &[&str]); 2] = [
            ("blkid", &["-p", "-s", "PART_ENTRY_UUID", "-o", "value"]),
            ("lsblk", &["-dno", "PARTUUID"]),
        ];
        for (program, args) in probes {
            let found = Command::new(program).args(args).arg(&device).output().ok();
            let uuid = found.map(|output| String::from_utf8_lossy(&output.stdout).trim().to_lowercase());
            if let Some(uuid) = uuid.filter(|uuid| !uuid.is_empty()) {
                return Ok(uuid);
            }
        }
        bail!(
            "{device}, the ESP at {}, has no GPT PARTUUID, which a firmware entry names it by",
            esp.display()
        )
    }

    /// The active entry starting `loader` from the ESP's own partition, made if there is none, and in the boot order (a new one last); an entry with archix's label for another partition or file, which an earlier install left, is deleted. Returns its number.
    fn create(&self, esp: &Path, loader: &str) -> Result<u16> {
        let partuuid = self.esp_partuuid(esp)?;
        for stale in self.entries() {
            if stale.label == LABEL && !stale.starts(loader, &partuuid) {
                self.delete(stale.number)?;
            }
        }
        let find = || self.entries().into_iter().find(|entry| entry.starts(loader, &partuuid));
        let before = self.order();
        let (number, fresh) = match find() {
            Some(entry) => (entry.number, false),
            None => {
                self.make(esp, loader)?;
                let made = find().with_context(|| {
                    format!(
                        "the firmware has no entry starting {loader} from the ESP's partition {partuuid} after efibootmgr made one; it has: {}",
                        self.listing()
                    )
                })?;
                (made.number, true)
            }
        };
        // A new entry may reuse a number a deleted one left in the boot order; it goes last until a boot through it counted as good.
        if fresh || !before.contains(&number) {
            let mut order: Vec<u16> = before.into_iter().filter(|n| *n != number).collect();
            order.push(number);
            self.set_order(&order)?;
        }
        Ok(number)
    }

    fn make(&self, esp: &Path, loader: &str) -> Result<()> {
        let file = format!("\\{}", loader.replace('/', "\\"));
        if !self.test {
            let (_, disk, part) = esp_device(esp)?;
            return self
                .efibootmgr(&[
                    "-q",
                    "--create-only",
                    "--disk",
                    &disk,
                    "--part",
                    &part,
                    "--label",
                    LABEL,
                    "--loader",
                    &file,
                ])
                .map(drop);
        }
        let number = (0..=0xFFFFu16)
            .find(|n| !self.var(&format!("Boot{n:04X}")).exists())
            .context("no free entry")?;
        let path: Vec<u8> = file.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
        let mut list = vec![4u8, 1, 42, 0, 1, 0, 0, 0];
        list.extend_from_slice(&[0u8; 16]);
        list.extend_from_slice(&TEST_PARTUUID);
        list.extend_from_slice(&[2, 2, 4, 4]);
        list.extend_from_slice(&((path.len() + 4) as u16).to_le_bytes());
        list.extend_from_slice(&path);
        list.extend_from_slice(&[0x7f, 0xff, 4, 0]);
        let mut option = 1u32.to_le_bytes().to_vec();
        option.extend_from_slice(&(list.len() as u16).to_le_bytes());
        option.extend(LABEL.encode_utf16().chain([0]).flat_map(u16::to_le_bytes));
        option.extend_from_slice(&list);
        self.write_test(&format!("Boot{number:04X}"), &option)
    }

    fn listing(&self) -> String {
        let entries: Vec<String> = self.entries().iter().map(BootEntry::describe).collect();
        match entries.is_empty() {
            true => "no entries".to_string(),
            false => entries.join("; "),
        }
    }

    /// What keeps the firmware, as read back, from starting systemd-boot through entry `number` at the next boot by `method`: nothing when all is as archix set it.
    pub fn problems(&self, esp: &Path, number: u16, method: Method) -> Vec<String> {
        let mut problems = Vec::new();
        let order = self.order();
        match (self.entry(number), self.esp_partuuid(esp)) {
            (None, _) => problems.push(format!("the firmware holds no entry {number:04X}")),
            (_, Err(e)) => problems.push(format!("{e:#}")),
            (Some(entry), Ok(partuuid)) => {
                if !entry.active {
                    problems.push(format!("entry {number:04X} is inactive"));
                }
                if entry.partuuid.as_deref() != Some(partuuid.as_str()) {
                    problems.push(format!(
                        "entry {number:04X} names partition {}, but the ESP at {} is {partuuid}",
                        entry.partuuid.as_deref().unwrap_or("none by GPT PARTUUID"),
                        esp.display()
                    ));
                }
                if !entry
                    .file
                    .as_deref()
                    .is_some_and(|file| file.trim_start_matches('/').eq_ignore_ascii_case(LOADER))
                {
                    problems.push(format!(
                        "entry {number:04X} starts {}, not {LOADER}",
                        entry.file.as_deref().unwrap_or("no file")
                    ));
                }
            }
        }
        if !esp.join(LOADER).is_file() {
            problems.push(format!("{} isn't on the ESP", esp.join(LOADER).display()));
        }
        if !order.contains(&number) {
            problems.push(format!(
                "entry {number:04X} isn't in the boot order ({}), and firmware drops or skips such entries",
                order_text(&order)
            ));
        }
        if self.next() != Some(number) {
            problems.push(match self.next() {
                Some(other) => format!("BootNext names {other:04X}, not {number:04X}"),
                None => format!("BootNext isn't set to {number:04X}"),
            });
        }
        if method == Method::First && order.first() != Some(&number) {
            problems.push(format!(
                "entry {number:04X} isn't first in the boot order ({})",
                order_text(&order)
            ));
        }
        problems
    }
}

fn order_text(order: &[u16]) -> String {
    let list: Vec<String> = order.iter().map(|n| format!("{n:04X}")).collect();
    match list.is_empty() {
        true => "empty".to_string(),
        false => list.join(","),
    }
}

/// The partition the ESP at `esp` is on, its disk and its number there, as efibootmgr takes them.
fn esp_device(esp: &Path) -> Result<(String, String, String)> {
    let output = Command::new("findmnt")
        .args(["-n", "-o", "SOURCE", "-T"])
        .arg(esp)
        .output()
        .context("running findmnt")?;
    // An automounted ESP lists its autofs mount first.
    let listed = String::from_utf8_lossy(&output.stdout);
    let source = listed.lines().last().unwrap_or_default().trim().to_string();
    let name = Path::new(&source)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let part = fs::read_to_string(format!("/sys/class/block/{name}/partition"))
        .with_context(|| format!("{} is on \"{source}\", which isn't a partition", esp.display()))?
        .trim()
        .to_string();
    let disk = fs::canonicalize(format!("/sys/class/block/{name}/.."))?
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    Ok((format!("/dev/{name}"), format!("/dev/{disk}"), part))
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
    let ours =
        super::secureboot::own_db_cert().is_some_and(|cert| super::secureboot::signed_with(to, &cert).unwrap_or(false));
    if plan.secure_boot.unwrap_or_else(boot::sbctl_has_keys) && !ours {
        boot::sign_replacing(esp, to)?;
    }
    crate::durable::sync_esp(esp)
}

/// After a switch wrote systemd-boot's menu on a Limine machine: puts systemd-boot on the ESP, signed under Secure Boot, with a firmware entry after Limine's, has the firmware try it at the next boot (through BootNext; first in the boot order too once the firmware ignored BootNext) and reads back that the firmware holds all of it. Nothing when the migration is done already. Returns what to tell.
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
    arm(paths, &firmware, esp, &crate::testhooks::boot_id()).map(Some)
}

/// Gives systemd-boot its firmware entry and arms a trial boot, recorded with the boot that armed it once the firmware's state read back right.
fn arm(paths: &Paths, firmware: &Firmware, esp: &Path, boot_id: &str) -> Result<String> {
    let entry = firmware.create(esp, LOADER)?;
    crate::testhooks::mark("limine-entry-created");
    let missed = load(paths).map(|m| m.missed).unwrap_or_default();
    let mut migration = Migration {
        phase: Phase::Trying,
        entry,
        armed: None,
        missed,
    };
    save(paths, &migration)?;
    if let Err(e) = write_chain_entry(esp) {
        eprintln!("warning: systemd-boot's menu got no entry that starts Limine: {e:#}");
    }
    let method = match migration.missed.contains(&Method::Next) {
        true => Method::First,
        false => Method::Next,
    };
    if method == Method::First {
        let mut order: Vec<u16> = firmware.order().into_iter().filter(|n| *n != entry).collect();
        order.insert(0, entry);
        firmware.set_order(&order)?;
    }
    firmware.set_next(entry)?;
    let problems = firmware.problems(esp, entry, method);
    if !problems.is_empty() {
        bail!(
            "the firmware doesn't hold what archix set for a boot through systemd-boot: {}; `archix boot-info` shows its entries",
            problems.join("; ")
        );
    }
    migration.armed = Some(Armed {
        boot: boot_id.to_string(),
        method,
    });
    save(paths, &migration)?;
    Ok(match method {
        Method::Next => format!(
            "systemd-boot is on the ESP with a firmware entry ({entry:04X}) after Limine's, and the next boot tries it once (BootNext, read back from the firmware); Limine stays first until a boot through systemd-boot counts as good, then systemd-boot goes first"
        ),
        Method::First => format!(
            "the firmware ignored BootNext, so systemd-boot's entry ({entry:04X}) is first in the boot order for the next boot, with Limine's second: firmware that can't start systemd-boot goes on to Limine, and a boot Limine starts puts Limine first again"
        ),
    })
}

/// A systemd-boot menu entry that starts Limine's loader on the ESP, never the default: the way back to Limine from systemd-boot's menu.
fn write_chain_entry(esp: &Path) -> Result<()> {
    let Some(loader) = fs::read_dir(esp.join("EFI/limine"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|file| file.file_name().to_string_lossy().into_owned())
        .find(|name| name.to_lowercase().ends_with(".efi"))
    else {
        return Ok(());
    };
    let text = format!("title Limine (the boot loader before archix)\nefi /EFI/limine/{loader}\n");
    let path = esp.join(CHAIN_ENTRY);
    if fs::read_to_string(&path).is_ok_and(|now| now == text) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    crate::durable::write_esp(&path, text.as_bytes())
}

/// Whether the next boot goes through the systemd-boot a move from Limine puts on the ESP: the running loader's LoaderFeatures are Limine's then, which don't say systemd-boot counts boots, but that boot must be counted to count as good and finish the move.
pub fn next_boot_is_archix_systemd_boot(paths: &Paths, loader: boot::Loader, esp: &Path) -> bool {
    loader == boot::Loader::SystemdBoot && present(paths, esp) && !load(paths).is_some_and(|m| m.phase == Phase::Done)
}

/// At each boot during the move: a trial an earlier boot armed that Limine started all the same is recorded as missed, and Limine goes first in the boot order again if systemd-boot's entry was put there. Returns the notice for a boot Limine started.
pub fn after_boot(paths: &Paths) -> Option<String> {
    let firmware = Firmware::new()?;
    record_boot(paths, &firmware, &crate::testhooks::boot_id())
}

fn record_boot(paths: &Paths, firmware: &Firmware, boot_id: &str) -> Option<String> {
    let mut migration = load(paths).filter(|m| m.phase == Phase::Trying)?;
    let through = firmware.booted_by_systemd_boot();
    if let Some(armed) = migration.armed.clone().filter(|armed| armed.boot != boot_id) {
        if !through {
            if !migration.missed.contains(&armed.method) {
                migration.missed.push(armed.method);
            }
            let order = firmware.order();
            if order.first() == Some(&migration.entry) && order.len() > 1 {
                let mut back: Vec<u16> = order.into_iter().filter(|n| *n != migration.entry).collect();
                back.push(migration.entry);
                if let Err(e) = firmware.set_order(&back) {
                    println!("archix: couldn't put Limine first in the boot order again: {e:#}");
                }
            }
        }
        migration.armed = None;
        if let Err(e) = save(paths, &migration) {
            println!("archix: couldn't record which loader started this boot: {e:#}");
        }
    }
    (!through).then(|| notice(&migration, boot_id))
}

/// What a boot Limine started during the move means and what to do, for the login notes, `archix status` and `archix switch`.
fn notice(migration: &Migration, boot_id: &str) -> String {
    let entry = migration.entry;
    let lead = format!(
        "this boot came through Limine, not archix's systemd-boot: {UNPROTECTED}, so a change that breaks the boot would need a live USB"
    );
    let armed_now = migration.armed.as_ref().filter(|armed| armed.boot == boot_id);
    let then = if let Some(armed) = armed_now {
        format!(
            "The next boot tries systemd-boot (firmware entry {entry:04X}, by {}): reboot, and `archix boot-info` then says which loader started",
            armed.method.name()
        )
    } else if migration.missed.contains(&Method::First) {
        format!(
            "The firmware started Limine although entry {entry:04X} (\"{LABEL}\") was first in its boot order and BootNext named it, so it doesn't follow its boot entries. Choose \"{LABEL}\" in the firmware's boot menu (often F8, F11 or F12 at power-on) or put it first in the firmware setup; once a boot through it counts as good archix finishes the move. `archix boot-info` shows the firmware's entries, and `archix switch {ALLOW_FLAG}` changes packages without that protection"
        )
    } else if migration.missed.contains(&Method::Next) {
        format!(
            "The firmware ignored BootNext for entry {entry:04X}. The next `archix switch` puts that entry first in the boot order for one boot, with Limine's second, and changes no packages until a boot came through systemd-boot (`archix switch {ALLOW_FLAG}` changes them without that protection)"
        )
    } else {
        format!("The next `archix switch` has the firmware try systemd-boot (entry {entry:04X}) at the boot after it")
    };
    format!("{lead}. {then}")
}

/// While a move from Limine is under way (or due) and Limine started this boot: why a switch that changes packages isn't protected. None once systemd-boot started the boot, when the move is done, and where the firmware can't be read.
pub fn unprotected(paths: &Paths, plan: &boot::BootPlan) -> Option<String> {
    if !pending(paths, plan) {
        return None;
    }
    let firmware = Firmware::new()?;
    if firmware.booted_by_systemd_boot() {
        return None;
    }
    let boot_id = crate::testhooks::boot_id();
    Some(match load(paths) {
        Some(migration) => notice(&migration, &boot_id),
        None => format!(
            "this boot came through Limine, not archix's systemd-boot: {UNPROTECTED} until a boot through systemd-boot, which this switch sets up"
        ),
    })
}

/// Whether every way archix has to make the firmware start systemd-boot was tried and Limine started all the same.
pub fn out_of_methods(paths: &Paths) -> bool {
    load(paths).is_some_and(|m| m.phase == Phase::Trying && m.missed.contains(&Method::First))
}

/// The move's line for `archix status`, while it's under way.
pub fn status_line(paths: &Paths) -> Option<String> {
    let migration = load(paths).filter(|m| m.phase == Phase::Trying)?;
    let Some(firmware) = Firmware::new() else {
        return Some(format!(
            "moving from Limine to systemd-boot (firmware entry {:04X})",
            migration.entry
        ));
    };
    Some(match firmware.booted_by_systemd_boot() {
        true => format!(
            "moving from Limine to systemd-boot: this boot came through systemd-boot (firmware entry {:04X}); once it counts as good, systemd-boot goes first and Limine stays installed after it",
            migration.entry
        ),
        false => format!("warning: {}", notice(&migration, &crate::testhooks::boot_id())),
    })
}

/// `archix boot-info`: which loader started this boot, whether archix's boot protection is in effect, the firmware's entries and order, and where a move from Limine stands, one `key: value` per line.
pub fn info(paths: &Paths, esp: Option<&Path>) -> Vec<String> {
    let Some(firmware) = Firmware::new() else {
        return vec!["loader: unknown (no EFI variables: not booted through UEFI, or in a chroot)".to_string()];
    };
    let entries = firmware.entries();
    let current = firmware.current();
    let started = current.and_then(|number| entries.iter().find(|entry| entry.number == number));
    let info = firmware.loader_info();
    let says_limine = |text: &str| text.to_lowercase().contains("limine");
    let loader = if firmware.booted_by_systemd_boot() {
        "systemd-boot"
    } else if info.as_deref().is_some_and(says_limine)
        || started.is_some_and(|entry| says_limine(&entry.label) || entry.file.as_deref().is_some_and(says_limine))
        || esp.is_some_and(|esp| present(paths, esp))
    {
        "limine"
    } else {
        "other"
    };
    let mut lines = vec![format!("loader: {loader}")];
    lines.push(format!("loader-info: {}", info.as_deref().unwrap_or("none")));
    let selected = crate::testhooks::selected_entry();
    lines.push(format!("loader-entry: {}", selected.as_deref().unwrap_or("none")));
    let protected = loader == "systemd-boot" && selected.as_deref().is_some_and(|entry| entry.starts_with("archix-"));
    lines.push(match protected {
        true => {
            "protection: in effect (systemd-boot started an archix entry: boot counting, fallback and the rescue entry)"
                .to_string()
        }
        false => format!("protection: none ({UNPROTECTED}: they need a boot through archix's systemd-boot entries)"),
    });
    let number = |n: Option<u16>| n.map(|n| format!("{n:04X}")).unwrap_or_else(|| "none".to_string());
    lines.push(format!("firmware-current: {}", number(current)));
    lines.push(format!("firmware-next: {}", number(firmware.next())));
    lines.push(format!("firmware-order: {}", order_text(&firmware.order())));
    lines.extend(
        entries
            .iter()
            .map(|entry| format!("firmware-entry: {}", entry.describe())),
    );
    let boot_id = crate::testhooks::boot_id();
    lines.push(match load(paths) {
        None => "migration: none".to_string(),
        Some(m) if m.phase == Phase::Done => format!("migration: done (entry {:04X})", m.entry),
        Some(m) => {
            let armed = match &m.armed {
                Some(armed) if armed.boot == boot_id => format!("armed in this boot by {}", armed.method.name()),
                Some(armed) => format!("armed in an earlier boot by {}", armed.method.name()),
                None => "not armed".to_string(),
            };
            let missed: Vec<&str> = m.missed.iter().map(|method| method.name()).collect();
            let missed = match missed.is_empty() {
                true => "none".to_string(),
                false => missed.join(", "),
            };
            format!(
                "migration: trying (entry {:04X}, {armed}, ignored by the firmware: {missed})",
                m.entry
            )
        }
    });
    if let (Some(m), Some(esp)) = (load(paths).filter(|m| m.phase == Phase::Trying), esp) {
        let method = m.armed.as_ref().map(|armed| armed.method).unwrap_or(Method::Next);
        let armed_now = m.armed.as_ref().is_some_and(|armed| armed.boot == boot_id);
        let problems: Vec<String> = firmware
            .problems(esp, m.entry, method)
            .into_iter()
            .filter(|problem| armed_now || !problem.starts_with("BootNext"))
            .collect();
        lines.extend(problems.into_iter().map(|problem| format!("problem: {problem}")));
    }
    lines
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
    let firmware = Firmware::new().context("no EFI variables")?;
    if !firmware.booted_by_systemd_boot() {
        return Ok(vec!["archix: this boot didn't come through systemd-boot, so Limine stays first; the next switch has the firmware try systemd-boot again".to_string()]);
    }
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
    migration.armed = None;
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
                armed: None,
                missed: Vec::new(),
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

    const OTHER_PARTUUID: [u8; 16] = [9, 9, 9, 9, 8, 8, 7, 7, 0x90, 0x02, 6, 5, 4, 3, 2, 1];
    const LIMINE_LOADER: &str = "\\EFI\\limine\\limine_x64.efi";

    /// A load option as efibootmgr writes it: a GPT hard drive node, then the file.
    fn option(label: &str, partuuid: &[u8; 16], file: &str) -> Vec<u8> {
        let path: Vec<u8> = file.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
        let mut list = vec![4u8, 1, 42, 0, 1, 0, 0, 0];
        list.extend_from_slice(&2048u64.to_le_bytes());
        list.extend_from_slice(&8388608u64.to_le_bytes());
        list.extend_from_slice(partuuid);
        list.extend_from_slice(&[2, 2, 4, 4]);
        list.extend_from_slice(&((path.len() + 4) as u16).to_le_bytes());
        list.extend_from_slice(&path);
        list.extend_from_slice(&[0x7f, 0xff, 4, 0]);
        let mut option = 1u32.to_le_bytes().to_vec();
        option.extend_from_slice(&(list.len() as u16).to_le_bytes());
        option.extend(label.encode_utf16().chain([0]).flat_map(u16::to_le_bytes));
        option.extend_from_slice(&list);
        option
    }

    /// A machine Limine boots: its entry 0000 alone in the boot order, systemd-boot's file on the ESP.
    struct Machine {
        _root: tempfile::TempDir,
        paths: Paths,
        esp: PathBuf,
        firmware: Firmware,
    }

    impl Machine {
        fn new() -> Machine {
            let root = tempfile::tempdir().unwrap();
            let paths = Paths::under(root.path());
            let esp = root.path().join("efi");
            let vars = root.path().join("efivars");
            fs::create_dir_all(esp.join("EFI/limine")).unwrap();
            fs::create_dir_all(esp.join("EFI/systemd")).unwrap();
            fs::create_dir_all(&vars).unwrap();
            fs::write(esp.join("EFI/limine/limine_x64.efi"), "limine").unwrap();
            fs::write(esp.join(LOADER), "systemd-boot").unwrap();
            let firmware = Firmware { dir: vars, test: true };
            firmware
                .write_test("Boot0000", &option("Limine", &TEST_PARTUUID, LIMINE_LOADER))
                .unwrap();
            firmware.set_order(&[0]).unwrap();
            Machine {
                _root: root,
                paths,
                esp,
                firmware,
            }
        }

        fn arm(&self, boot: &str) -> Result<String> {
            arm(&self.paths, &self.firmware, &self.esp, boot)
        }

        /// A boot the firmware made: BootNext is used up, and LoaderInfo is the loader's.
        fn boot(&self, boot: &str, loader: Option<&str>) -> Option<String> {
            let _ = fs::remove_file(self.firmware.var("BootNext"));
            let info = self.firmware.dir.join(format!("LoaderInfo-{}", boot::LOADER_GUID));
            match loader {
                Some(loader) => {
                    let mut bytes = vec![6u8, 0, 0, 0];
                    bytes.extend(loader.encode_utf16().chain([0]).flat_map(u16::to_le_bytes));
                    fs::write(info, bytes).unwrap();
                }
                None => {
                    let _ = fs::remove_file(info);
                }
            }
            record_boot(&self.paths, &self.firmware, boot)
        }
    }

    #[test]
    fn a_load_option_gives_its_partition_by_gpt_partuuid_and_its_file() {
        let mut bytes = vec![7u8, 0, 0, 0];
        bytes.extend(option("Limine", &TEST_PARTUUID, LIMINE_LOADER));
        let entry = parse_entry(3, &bytes).unwrap();
        assert_eq!(entry.label, "Limine");
        assert!(entry.active);
        assert_eq!(
            entry.partuuid.as_deref(),
            Some("d4c3b2a1-2211-4433-8001-010203040506"),
            "the first three fields are little-endian, as blkid prints them"
        );
        assert_eq!(entry.file.as_deref(), Some("/EFI/limine/limine_x64.efi"));
    }

    #[test]
    fn the_trial_is_armed_through_bootnext_with_the_entry_after_limines_and_read_back() {
        let machine = Machine::new();
        let said = machine.arm("boot-1").unwrap();
        assert!(said.contains("Limine stays first"), "{said}");
        assert_eq!(machine.firmware.order(), vec![0, 1]);
        assert_eq!(machine.firmware.next(), Some(1));
        let entry = machine.firmware.entry(1).unwrap();
        assert!(entry.starts(LOADER, &guid_string(&TEST_PARTUUID)), "{entry:?}");
        assert_eq!(
            machine.firmware.problems(&machine.esp, 1, Method::Next),
            Vec::<String>::new()
        );
        let migration = load(&machine.paths).unwrap();
        assert_eq!(
            migration.armed,
            Some(Armed {
                boot: "boot-1".into(),
                method: Method::Next
            })
        );
        assert!(
            fs::read_to_string(machine.esp.join(CHAIN_ENTRY))
                .unwrap()
                .contains("efi /EFI/limine/limine_x64.efi"),
            "systemd-boot's menu leads back to Limine"
        );
    }

    #[test]
    fn an_entry_for_systemd_boot_on_another_partition_is_not_the_one_tried() {
        let machine = Machine::new();
        let stale = "\\EFI\\systemd\\systemd-bootx64.efi";
        machine
            .firmware
            .write_test("Boot0001", &option("Linux Boot Manager", &OTHER_PARTUUID, stale))
            .unwrap();
        machine
            .firmware
            .write_test("Boot0002", &option(LABEL, &OTHER_PARTUUID, stale))
            .unwrap();
        machine.firmware.set_order(&[0, 1, 2]).unwrap();
        machine.arm("boot-1").unwrap();
        let tried = machine.firmware.next().unwrap();
        let entry = machine.firmware.entry(tried).unwrap();
        assert_eq!(
            entry.partuuid,
            Some(guid_string(&TEST_PARTUUID)),
            "BootNext names an entry on the ESP's own partition, not one an earlier install left: {entry:?}"
        );
        assert_eq!(entry.label, LABEL);
        let labels: Vec<String> = machine.firmware.entries().into_iter().map(|e| e.label).collect();
        assert_eq!(
            labels.iter().filter(|label| *label == LABEL).count(),
            1,
            "archix's own stale entry is deleted: {labels:?}"
        );
        assert!(
            labels.contains(&"Linux Boot Manager".to_string()),
            "others' entries stay"
        );
        assert_eq!(machine.firmware.order().first(), Some(&0), "Limine stays first");
    }

    #[test]
    fn an_entry_the_boot_order_lacks_is_put_back_in_it() {
        let machine = Machine::new();
        machine.arm("boot-1").unwrap();
        machine.firmware.set_order(&[0]).unwrap();
        machine.arm("boot-1").unwrap();
        assert_eq!(machine.firmware.order(), vec![0, 1]);
    }

    #[test]
    fn what_the_firmware_holds_is_checked_against_what_was_set() {
        let machine = Machine::new();
        machine.arm("boot-1").unwrap();
        let problems = |method| machine.firmware.problems(&machine.esp, 1, method).join("; ");
        assert!(problems(Method::First).contains("isn't first in the boot order"));
        fs::remove_file(machine.firmware.var("BootNext")).unwrap();
        assert!(problems(Method::Next).contains("BootNext isn't set to 0001"));
        machine.firmware.set_order(&[0]).unwrap();
        assert!(problems(Method::Next).contains("isn't in the boot order (0000)"));
        fs::remove_file(machine.esp.join(LOADER)).unwrap();
        assert!(problems(Method::Next).contains("isn't on the ESP"));
        machine
            .firmware
            .write_test("Boot0001", &option(LABEL, &OTHER_PARTUUID, "\\EFI\\BOOT\\BOOTX64.EFI"))
            .unwrap();
        let said = problems(Method::Next);
        assert!(
            said.contains("names partition 09090909-0808-0707-9002-060504030201")
                && said.contains("starts /EFI/BOOT/BOOTX64.EFI"),
            "{said}"
        );
        fs::remove_file(machine.firmware.var("Boot0001")).unwrap();
        assert!(problems(Method::Next).contains("holds no entry 0001"));
    }

    #[test]
    fn a_trial_limine_started_is_recorded_and_the_next_one_puts_systemd_boot_first() {
        let machine = Machine::new();
        machine.arm("boot-1").unwrap();
        assert_eq!(
            machine
                .boot("boot-1", None)
                .map(|note| note.contains("The next boot tries systemd-boot")),
            Some(true),
            "the boot that armed the trial isn't the trial"
        );
        assert_eq!(load(&machine.paths).unwrap().missed, Vec::new());

        let note = machine
            .boot("boot-2", None)
            .expect("a notice for a boot Limine started");
        assert!(
            note.contains("this boot came through Limine, not archix's systemd-boot")
                && note.contains(UNPROTECTED)
                && note.contains("ignored BootNext"),
            "{note}"
        );
        let migration = load(&machine.paths).unwrap();
        assert_eq!((migration.missed.clone(), migration.armed), (vec![Method::Next], None));
        assert!(!out_of_methods(&machine.paths));

        let said = machine.arm("boot-2").unwrap();
        assert!(said.contains("first in the boot order"), "{said}");
        assert_eq!(
            machine.firmware.order(),
            vec![1, 0],
            "systemd-boot first, Limine second"
        );
        assert_eq!(machine.firmware.next(), Some(1));

        let note = machine
            .boot("boot-3", None)
            .expect("a notice for a boot Limine started");
        assert!(
            note.contains("firmware's boot menu") && note.contains(ALLOW_FLAG),
            "{note}"
        );
        assert_eq!(
            machine.firmware.order(),
            vec![0, 1],
            "a boot Limine started puts Limine first again"
        );
        assert_eq!(load(&machine.paths).unwrap().missed, vec![Method::Next, Method::First]);
        assert!(out_of_methods(&machine.paths));
    }

    #[test]
    fn a_trial_systemd_boot_started_is_no_miss_and_gets_no_notice() {
        let machine = Machine::new();
        machine.arm("boot-1").unwrap();
        assert_eq!(machine.boot("boot-2", Some("systemd-boot 258.2-2-arch")), None);
        let migration = load(&machine.paths).unwrap();
        assert_eq!(
            (migration.missed, migration.armed, migration.phase),
            (Vec::new(), None, Phase::Trying)
        );
        assert_eq!(machine.firmware.order(), vec![0, 1]);
    }

    #[test]
    fn a_boot_limine_started_before_any_trial_was_armed_is_no_miss() {
        let machine = Machine::new();
        let entry = machine.firmware.create(&machine.esp, LOADER).unwrap();
        save(
            &machine.paths,
            &Migration {
                phase: Phase::Trying,
                entry,
                armed: None,
                missed: Vec::new(),
            },
        )
        .unwrap();
        let note = machine.boot("boot-2", Some("Limine 9.3.2")).unwrap();
        assert!(note.contains("through Limine, not archix's systemd-boot"), "{note}");
        assert_eq!(load(&machine.paths).unwrap().missed, Vec::new());
    }
}

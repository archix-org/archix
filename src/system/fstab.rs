use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::{get, map, str_of, strings};
use crate::luagen::Lua;
use crate::value::Value;

/// One line of a deployment's /etc/fstab that file_systems or swap declares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FstabLine {
    pub device: String,
    /// The mount point, or none for swap.
    pub point: String,
    pub fs_type: String,
    pub options: String,
    #[serde(default)]
    pub passno: u32,
    /// A swap file archix-swapfile makes at boot, in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

impl FstabLine {
    pub fn line(&self) -> String {
        format!(
            "{} {} {} {} 0 {}",
            escape(&self.device),
            escape(&self.point),
            escape(&self.fs_type),
            escape(&self.options),
            self.passno
        )
    }

    fn is_swap(&self) -> bool {
        self.fs_type == "swap"
    }

    /// The mount point, or a swap line's device or file.
    fn key(&self) -> &str {
        if self.is_swap() { &self.device } else { &self.point }
    }

    fn describe(&self) -> String {
        if self.is_swap() {
            format!("swap on {}", self.device)
        } else {
            format!("{} ({})", self.point, self.device)
        }
    }
}

/// What file_systems, swap and zram render: the declared fstab lines and the files that go with them, each with the option that writes it.
#[derive(Debug, Default)]
pub struct Declared {
    pub lines: Vec<FstabLine>,
    pub files: Vec<(String, String, &'static str)>,
}

/// The first line of every fstab archix writes.
pub const HEADER: &str = "# Written by archix at each switch: the root's and the ESP's lines as the deployment had them, then file_systems and swap from the config.";

/// Where archix-swapfile runs from at boot, the deployment's own archix.
const ARCHIX: &str = "/usr/bin/archix";

/// How long a nofail mount's device is waited for unless its options say otherwise.
pub const DEVICE_TIMEOUT: &str = "x-systemd.device-timeout=30s";

fn escape(field: &str) -> String {
    field
        .replace('\\', "\\134")
        .replace(' ', "\\040")
        .replace('\t', "\\011")
}

fn unescape(field: &str) -> String {
    field
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\134", "\\")
}

fn under(path: &str, dir: &str) -> bool {
    path == dir || path.strip_prefix(dir).is_some_and(|rest| rest.starts_with('/'))
}

fn normal(point: &str) -> &str {
    match point.trim_end_matches('/') {
        "" => "/",
        trimmed => trimmed,
    }
}

/// Why `point` can't be a mount point file_systems declares, with the ESP at `esp`: archix writes the root's and the ESP's lines itself, and the trees each deployment keeps, the pacman database and DKMS state it binds at boot, and /archix must stay on the root filesystem, as the first switch's layout requires.
pub fn refused_point(point: &str, esp: &str) -> Option<String> {
    let esp = normal(esp);
    if point == "/" {
        return Some("archix keeps the root's line as the deployment has it".into());
    }
    if under(point, esp) {
        return Some(format!(
            "archix writes the ESP's line at {esp} itself, mounted on access, fscked and readable by root only"
        ));
    }
    if under(point, "/var/usrlocal") {
        return Some("/var/usrlocal is the shared /usr/local, bound in the initramfs before fstab mounts anything; mount the filesystem at /usr/local instead".into());
    }
    if under(point, "/usr") && !under(point, "/usr/local") {
        return Some("each deployment keeps its own /usr on the root filesystem".into());
    }
    for tree in ["/opt", "/boot", "/etc"] {
        if under(point, tree) {
            return Some(format!("each deployment keeps its own {tree} on the root filesystem"));
        }
    }
    if under(point, "/archix") {
        return Some("deployments live in /archix, which the initramfs finds only on the root filesystem".into());
    }
    if point == "/var" || point == "/var/lib" || under(point, "/var/lib/pacman") || under(point, "/var/lib/dkms") {
        return Some("a separate filesystem there would cover the pacman database and DKMS state each deployment keeps on the root filesystem".into());
    }
    None
}

/// Why a swap file can't be at `file`: never in /archix or a tree a deployment keeps, nor where the boot can't keep it.
fn refused_swap_file(file: &str, esp: &str) -> Option<String> {
    let esp = normal(esp);
    if under(file, esp) {
        return Some(format!("it's on the ESP at {esp}"));
    }
    for (dir, why) in [
        ("/archix", "deployments live in /archix"),
        (
            "/usr",
            "each deployment keeps its own /usr, and /usr/local is shared through /var/usrlocal",
        ),
        ("/var/usrlocal", "it's the shared /usr/local"),
        ("/opt", "each deployment keeps its own /opt"),
        ("/boot", "each deployment keeps its own /boot"),
        ("/etc", "each deployment keeps its own /etc"),
        ("/var/lib/pacman", "each deployment keeps its own pacman database"),
        ("/var/lib/dkms", "each deployment keeps its own DKMS state"),
        ("/proc", "it isn't on a disk"),
        ("/sys", "it isn't on a disk"),
        ("/dev", "it isn't on a disk"),
        ("/run", "it isn't on a disk"),
        ("/tmp", "/tmp may be emptied or a tmpfs"),
    ] {
        if under(file, dir) {
            return Some(why.to_string());
        }
    }
    None
}

fn check_path(path: &str) -> std::result::Result<(), String> {
    if !path.starts_with('/') || path.chars().any(char::is_control) {
        return Err("must be an absolute path".into());
    }
    if path != "/" && path.ends_with('/') {
        return Err("must not end in /".into());
    }
    if path
        .split('/')
        .skip(1)
        .any(|part| part.is_empty() || part == "." || part == "..")
        && path != "/"
    {
        return Err("must be a plain absolute path, without //, . or ..".into());
    }
    Ok(())
}

/// A size such as 8G, 512M or 1073741824, in bytes; K, M, G and T are powers of 1024.
pub fn size_bytes(text: &str) -> Option<u64> {
    let text = text.trim();
    let digits = text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len());
    let number: u64 = text[..digits].parse().ok()?;
    let unit = match text[digits..].trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "K" | "KB" | "KIB" => 1 << 10,
        "M" | "MB" | "MIB" => 1 << 20,
        "G" | "GB" | "GIB" => 1 << 30,
        "T" | "TB" | "TIB" => 1 << 40,
        _ => return None,
    };
    number.checked_mul(unit)
}

/// Whether `device` names a block device that udev has to bring up, so a missing one is waited for.
fn device_backed(device: &str) -> bool {
    ["UUID=", "LABEL=", "PARTUUID=", "PARTLABEL=", "ID="]
        .iter()
        .any(|prefix| device.starts_with(prefix))
        || (device.starts_with("/dev/") && !device.starts_with("/dev/mapper/"))
}

fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// systemd-escape --path: the unit name stem for `path`.
pub fn escape_path(path: &str) -> String {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return "-".to_string();
    }
    let mut out = String::new();
    for (i, byte) in trimmed.bytes().enumerate() {
        match byte {
            b'/' => out.push('-'),
            b'.' if i == 0 => out.push_str("\\x2e"),
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b':' | b'_' | b'.' => out.push(byte as char),
            _ => out.push_str(&format!("\\x{byte:02x}")),
        }
    }
    out
}

fn words(value: Option<&Value>, what: &str) -> Result<Vec<String>> {
    let items = strings(value);
    if let Some(bad) = items
        .iter()
        .find(|item| item.is_empty() || item.contains(char::is_whitespace))
    {
        bail!("{what}: {bad:?} isn't an option; give each one as its own string, without spaces");
    }
    Ok(items)
}

fn bool_of(spec: &Value, key: &str) -> bool {
    get(spec, key) == Some(&Value::Bool(true))
}

/// The fstab lines and files that file_systems, swap and zram declare, with the ESP at `esp`.
pub fn render(config: &Value, esp: &str) -> Result<Declared> {
    let mut out = Declared::default();
    let mut crypttab: BTreeMap<String, String> = BTreeMap::new();
    for (point, spec) in map(config, "file_systems").into_iter().flatten() {
        let what = format!("file_systems[{point:?}]");
        check_path(point).map_err(|why| anyhow::anyhow!("{what}: the mount point {why}"))?;
        if let Some(why) = refused_point(point, esp) {
            bail!("{what}: {point} can't be declared: {why}");
        }
        let device = str_of(spec, "device").unwrap_or_default();
        if device.trim().is_empty() || device.chars().any(char::is_control) {
            bail!("{what}: device must name what to mount, such as \"UUID=…\"");
        }
        let mut options = words(get(spec, "options"), &format!("{what}.options"))?;
        let bind = options.iter().any(|option| option == "bind" || option == "rbind");
        let mut fs_type = str_of(spec, "fs_type").unwrap_or("auto").to_string();
        if fs_type.is_empty() || fs_type.contains(char::is_whitespace) {
            bail!("{what}: fs_type {fs_type:?} isn't a filesystem type");
        }
        if bind {
            check_path(device).map_err(|why| {
                anyhow::anyhow!("{what}: a bind mount's device is the directory to bind, which {why}")
            })?;
            if fs_type == "auto" {
                fs_type = "none".to_string();
            }
        }
        let check = bool_of(spec, "check");
        if check && (bind || fs_type == "tmpfs") {
            bail!("{what}: check = true needs a filesystem on a device for fsck to check");
        }
        let required = bool_of(spec, "required");
        if required && options.iter().any(|option| option == "nofail") {
            bail!("{what}: required = true and the nofail option contradict each other");
        }
        if !required {
            if !options.iter().any(|option| option == "nofail") {
                options.push("nofail".into());
            }
            if device_backed(device)
                && !options
                    .iter()
                    .any(|option| option.starts_with("x-systemd.device-timeout="))
            {
                options.push(DEVICE_TIMEOUT.into());
            }
        }
        if let Some(encrypted) = get(spec, "encrypted").filter(|value| !matches!(value, Value::Null)) {
            let name = device
                .strip_prefix("/dev/mapper/")
                .filter(|name| !name.is_empty() && !name.contains(['/', ' ']))
                .with_context(|| {
                    format!("{what}: encrypted needs device = \"/dev/mapper/<name>\", the name its crypttab line opens it as")
                })?;
            let source = str_of(encrypted, "device").unwrap_or_default();
            if source.trim().is_empty() || source.contains(char::is_whitespace) {
                bail!("{what}.encrypted: device must name the LUKS partition, such as \"UUID=…\"");
            }
            let key = match str_of(encrypted, "key_file") {
                Some(key) => {
                    check_path(key).map_err(|why| anyhow::anyhow!("{what}.encrypted: key_file {why}"))?;
                    if key.contains(' ') {
                        bail!("{what}.encrypted: key_file can't have spaces");
                    }
                    key.to_string()
                }
                None => "none".to_string(),
            };
            let mut crypt = words(get(encrypted, "options"), &format!("{what}.encrypted.options"))?;
            if !crypt
                .iter()
                .any(|option| ["luks", "plain", "tcrypt", "bitlk"].contains(&option.as_str()))
            {
                crypt.insert(0, "luks".into());
            }
            if !required && !crypt.iter().any(|option| option == "nofail") {
                crypt.push("nofail".into());
            }
            let line = format!("{name} {source} {key} {}", crypt.join(","));
            match crypttab.get(name) {
                Some(other) if *other != line => {
                    bail!("{what}: /dev/mapper/{name} is opened differently by another file_systems entry")
                }
                _ => crypttab.insert(name.to_string(), line),
            };
        }
        out.lines.push(FstabLine {
            device: device.to_string(),
            point: point.clone(),
            fs_type,
            options: if options.is_empty() {
                "defaults".into()
            } else {
                options.join(",")
            },
            passno: if check { 2 } else { 0 },
            size: None,
        });
    }
    if !crypttab.is_empty() {
        let mut text = "# Written by archix from file_systems' encrypted devices.\n".to_string();
        for line in crypttab.values() {
            text.push_str(line);
            text.push('\n');
        }
        out.files
            .push(("/etc/crypttab".into(), text, "file_systems' encrypted devices"));
    }
    if let Some(Value::List(swaps)) = get(config, "swap") {
        for (i, spec) in swaps.iter().enumerate() {
            render_swap(&mut out, spec, &format!("swap[{}]", i + 1), esp)?;
        }
    }
    let swap_keys: Vec<&str> = out
        .lines
        .iter()
        .filter(|line| line.is_swap())
        .map(FstabLine::key)
        .collect();
    if let Some(dup) = swap_keys
        .iter()
        .enumerate()
        .find_map(|(i, key)| swap_keys[..i].contains(key).then_some(key))
    {
        bail!("swap: {dup} is declared twice");
    }
    if let Some(zram) = get(config, "zram")
        && bool_of(zram, "enable")
    {
        let mut text = "# Written by archix from zram.\n[zram0]\n".to_string();
        match get(zram, "size") {
            Some(Value::Str(size)) if !size.trim().is_empty() && !size.contains('\n') => {
                text.push_str(&format!("zram-size = {}\n", size.trim()))
            }
            Some(Value::Int(mib)) if *mib > 0 => text.push_str(&format!("zram-size = {mib}\n")),
            None | Some(Value::Null) => {}
            Some(other) => bail!("zram.size: {other} is neither an expression such as \"ram / 2\" nor a number of MiB"),
        }
        if let Some(algorithm) = str_of(zram, "algorithm") {
            if algorithm.is_empty() || algorithm.contains(char::is_whitespace) {
                bail!("zram.algorithm: {algorithm:?} isn't a compression algorithm");
            }
            text.push_str(&format!("compression-algorithm = {algorithm}\n"));
        }
        if let Some(Value::Int(priority)) = get(zram, "priority") {
            text.push_str(&format!("swap-priority = {priority}\n"));
        }
        out.files.push((ZRAM_CONF.into(), text, "zram"));
    }
    Ok(out)
}

/// zram-generator's config in /etc.
pub const ZRAM_CONF: &str = "/etc/systemd/zram-generator.conf";

fn render_swap(out: &mut Declared, spec: &Value, what: &str, esp: &str) -> Result<()> {
    let (device, file) = (str_of(spec, "device"), str_of(spec, "file"));
    let required = bool_of(spec, "required");
    let mut options = words(get(spec, "options"), &format!("{what}.options"))?;
    if let Some(Value::Int(priority)) = get(spec, "priority") {
        options.retain(|option| !option.starts_with("pri="));
        options.push(format!("pri={priority}"));
    }
    if !required && !options.iter().any(|option| option == "nofail") {
        options.push("nofail".into());
    }
    let size = str_of(spec, "size");
    let (what_swaps, size) = match (device, file) {
        (Some(device), None) => {
            if device.trim().is_empty() || device.chars().any(char::is_control) {
                bail!("{what}: device must name a swap partition, such as \"UUID=…\"");
            }
            if size.is_some() {
                bail!("{what}: size is for a swap file, which archix makes; a partition has its own");
            }
            if !required && device_backed(device) && !options.iter().any(|o| o.starts_with("x-systemd.device-timeout="))
            {
                options.push(DEVICE_TIMEOUT.into());
            }
            (device.to_string(), None)
        }
        (None, Some(file)) => {
            check_path(file).map_err(|why| anyhow::anyhow!("{what}: file {why}"))?;
            if file == "/" || file.contains(char::is_whitespace) {
                bail!("{what}: file {file:?} must be a path without spaces");
            }
            if let Some(why) = refused_swap_file(file, esp) {
                bail!("{what}: a swap file can't be at {file}: {why}");
            }
            let bytes = match size {
                Some(size) => {
                    let bytes = size_bytes(size)
                        .with_context(|| format!("{what}: size {size:?} isn't a size such as \"8G\" or \"512M\""))?;
                    if bytes < 1 << 20 {
                        bail!("{what}: size {size:?} is less than a MiB");
                    }
                    Some(bytes)
                }
                None => None,
            };
            (file.to_string(), bytes)
        }
        _ => bail!("{what}: give one of device or file"),
    };
    if let Some(bytes) = size {
        let stem = escape_path(&what_swaps);
        let service = format!("archix-swapfile-{stem}.service");
        let parent = Path::new(&what_swaps)
            .parent()
            .unwrap_or(Path::new("/"))
            .display()
            .to_string();
        out.files.push((
            format!("/etc/systemd/system/{service}"),
            format!(
                "[Unit]\nDescription=archix: make the swap file {what_swaps}\nDefaultDependencies=no\n\
                 After=systemd-remount-fs.service\nRequiresMountsFor={parent}\nBefore={stem}.swap shutdown.target\nConflicts=shutdown.target\n\n\
                 [Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart={ARCHIX} swapfile {what_swaps} {bytes}\nStandardOutput=journal+console\n"
            ),
            "swap",
        ));
        out.files.push((
            format!("/etc/systemd/system/{stem}.swap.d/archix-swapfile.conf"),
            format!("[Unit]\nRequires={service}\nAfter={service}\n"),
            "swap",
        ));
    }
    out.lines.push(FstabLine {
        device: what_swaps,
        point: "none".into(),
        fs_type: "swap".into(),
        options: if options.is_empty() {
            "defaults".into()
        } else {
            options.join(",")
        },
        passno: 0,
        size,
    });
    Ok(())
}

/// The fields of an fstab line that isn't a comment, unescaped.
fn fields(line: &str) -> Option<Vec<String>> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    Some(line.split_whitespace().map(unescape).collect())
}

/// Whether an fstab line mounts the root or the ESP at `esp`, the lines archix keeps.
fn kept(fields: &[String], esp: &str) -> bool {
    fields.len() >= 2 && (normal(&fields[1]) == "/" || normal(&fields[1]) == normal(esp))
}

/// The fstab archix writes into a deployment whose fstab was `old`: its root and ESP lines, the ESP's with its protections or added for the filesystem `uuid` names, then the declared lines; with what it changed on the ESP's.
pub fn compose(
    old: &str,
    esp: &str,
    uuid: Option<&str>,
    declared: &[FstabLine],
) -> (String, Option<super::boot::EspFstab>) {
    let keep: String = old
        .lines()
        .filter(|line| fields(line).is_some_and(|fields| kept(&fields, esp)))
        .map(|line| format!("{line}\n"))
        .collect();
    let (keep, changed) = match super::boot::esp_fstab(&keep, esp, uuid) {
        Some((text, changed)) => (text, Some(changed)),
        None => (keep, None),
    };
    let mut text = format!("{HEADER}\n{keep}");
    for line in declared {
        text.push_str(&line.line());
        text.push('\n');
    }
    (text, changed)
}

/// Writes a staged deployment's /etc/fstab under `etc` whole: its root line, the ESP's at `esp` with archix's protections, then the declared lines.
pub fn write_in(etc: &Path, esp: &str, declared: &[FstabLine]) -> Result<()> {
    let path = etc.join("fstab");
    let old = fs::read_to_string(&path).unwrap_or_default();
    let uuid = super::boot::disk_link_of(Path::new(esp), "by-uuid");
    let (text, changed) = compose(&old, esp, uuid.as_deref(), declared);
    if text != old {
        crate::durable::write(&path, text.as_bytes())?;
    }
    if let Some(changed) = changed {
        super::boot::report_esp_fstab(esp, &changed);
    }
    Ok(())
}

/// The lines of an fstab other than the root's and the ESP's, as file_systems and swap would declare them.
fn others(fstab: &str, esp: &str) -> Vec<FstabLine> {
    fstab
        .lines()
        .filter_map(fields)
        .filter(|fields| fields.len() >= 3 && !kept(fields, esp))
        .map(|fields| FstabLine {
            device: fields[0].clone(),
            point: fields[1].clone(),
            fs_type: fields[2].clone(),
            options: fields.get(3).cloned().unwrap_or_else(|| "defaults".into()),
            passno: fields.get(5).and_then(|n| n.parse().ok()).unwrap_or(0),
            size: None,
        })
        .collect()
}

/// What the next boot mounts differently from the running deployment's fstab `current`, one line each, for the switch's list of changes.
pub fn describe(current: &str, esp: &str, declared: &[FstabLine]) -> Vec<String> {
    let old = others(current, esp);
    let mut lines = Vec::new();
    for line in declared {
        match old
            .iter()
            .find(|o| o.key() == line.key() && o.is_swap() == line.is_swap())
        {
            None => lines.push(format!(
                "fstab      {} at the next boot",
                if line.is_swap() {
                    format!("swap on {}", line.device)
                } else {
                    format!("mount {}", line.describe())
                }
            )),
            Some(o)
                if (&o.device, &o.fs_type, &o.options, o.passno)
                    != (&line.device, &line.fs_type, &line.options, line.passno) =>
            {
                lines.push(format!("fstab      change {} at the next boot", line.describe()))
            }
            Some(_) => {}
        }
    }
    for o in &old {
        if !declared
            .iter()
            .any(|line| line.key() == o.key() && line.is_swap() == o.is_swap())
        {
            lines.push(format!(
                "fstab      drop {} at the next boot, as neither file_systems nor swap declares it",
                o.describe()
            ));
        }
    }
    lines
}

/// boot.esp of an evaluated config.
pub fn esp_of(config: &Value) -> &str {
    get(config, "boot")
        .and_then(|boot| str_of(boot, "esp"))
        .unwrap_or("/efi")
}

/// `describe` against the fstab of the deployment running at `here`, for a config.
pub fn describe_at(here: &Path, config: &Value, declared: &[FstabLine]) -> Vec<String> {
    describe(
        &fs::read_to_string(here.join("etc/fstab")).unwrap_or_default(),
        esp_of(config),
        declared,
    )
}

/// The /dev path a device spec names, if it's attached now.
fn resolve(device: &str) -> Option<PathBuf> {
    let link = |kind: &str, name: &str| PathBuf::from("/dev/disk").join(kind).join(name.replace('/', "\\x2f"));
    let path = if let Some(uuid) = device.strip_prefix("UUID=") {
        link("by-uuid", uuid)
    } else if let Some(label) = device.strip_prefix("LABEL=") {
        link("by-label", label)
    } else if let Some(uuid) = device.strip_prefix("PARTUUID=") {
        link("by-partuuid", &uuid.to_ascii_lowercase())
    } else if let Some(label) = device.strip_prefix("PARTLABEL=") {
        link("by-partlabel", label)
    } else if let Some(id) = device.strip_prefix("ID=") {
        link("by-id", id)
    } else {
        PathBuf::from(device)
    };
    fs::canonicalize(path).ok()
}

/// Whether the device-mapper device `name` is active now as an LVM volume, which no crypttab line opens.
fn lvm_volume(name: &str) -> bool {
    fs::read_dir("/sys/class/block")
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            let dm = entry.path().join("dm");
            fs::read_to_string(dm.join("name")).is_ok_and(|found| found.trim() == name)
                && fs::read_to_string(dm.join("uuid")).is_ok_and(|uuid| uuid.starts_with("LVM-"))
        })
}

/// Whether the crypttab text, or a kernel command line's rd.luks.name, opens /dev/mapper/`name`.
fn opened(name: &str, crypttab: &str, cmdline: &str) -> bool {
    crypttab.lines().filter_map(fields).any(|fields| fields[0] == name)
        || cmdline
            .split_whitespace()
            .filter_map(|param| param.strip_prefix("rd.luks.name="))
            .any(|value| value.split_once('=').is_some_and(|(_, opened)| opened == name))
}

/// Whether `dir` is a btrfs subvolume of its own: a subvolume's root, on a device other than its parent's.
fn own_subvolume(dir: &Path) -> bool {
    let (Ok(meta), Ok(parent)) = (fs::metadata(dir), fs::metadata(dir.join(".."))) else {
        return false;
    };
    meta.ino() == 256 && meta.dev() != parent.dev()
}

fn nearest_existing(path: &Path) -> &Path {
    path.ancestors().find(|dir| dir.exists()).unwrap_or(Path::new("/"))
}

/// Checks the declared lines against the system under `root` before a switch: refuses a /dev/mapper mount nothing opens (with the new deployment's `crypttab`, crypttab.initramfs and kernel command line `cmdline`) and a swap file archix would make on btrfs outside a subvolume of its own; returns warnings for devices that aren't attached or hold another filesystem.
pub fn check(root: &Path, lines: &[FstabLine], crypttab: &str, cmdline: &str) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    for line in lines {
        if let Some(name) = line.device.strip_prefix("/dev/mapper/") {
            if !opened(name, crypttab, cmdline) && !lvm_volume(name) {
                bail!(
                    "{} mounts {}, which no /etc/crypttab line opens, so its boot would wait for a device nothing creates; add encrypted = {{ device = \"UUID=…\" }} to its file_systems entry, or a crypttab line naming {name}; nothing was changed",
                    line.key(),
                    line.device
                );
            }
            continue;
        }
        if line.is_swap() && line.device.starts_with('/') && !line.device.starts_with("/dev/") {
            let file = root.join(line.device.trim_start_matches('/'));
            let dir = file.parent().unwrap_or(root);
            let on_btrfs = crate::capture::is_btrfs(nearest_existing(dir));
            let shared = dir.exists() && on_btrfs && !own_subvolume(dir);
            match (line.size, shared) {
                (Some(_), true) => bail!(
                    "swap: {} would be made on btrfs in {}, which isn't a subvolume of its own, and btrfs can't snapshot a subvolume holding an active swap file; put it in a directory archix creates as a subvolume, such as /swap/swapfile (the directory must not exist yet), or make {} a subvolume first; nothing was changed",
                    line.device,
                    dir.display(),
                    dir.display()
                ),
                (None, true) => warnings.push(format!(
                    "warning: the swap file {} is on btrfs in {}, which isn't a subvolume of its own, so a snapshot of that subvolume fails while it's active; give it a size in swap so archix makes it in a subvolume, such as /swap/swapfile",
                    line.device,
                    dir.display()
                )),
                (None, false) if !file.exists() => warnings.push(format!(
                    "warning: the swap file {} doesn't exist; give it a size in swap so archix makes it at boot",
                    line.device
                )),
                _ => {}
            }
            continue;
        }
        if !device_backed(&line.device) {
            continue;
        }
        let required = !line.options.split(',').any(|option| option == "nofail");
        let Some(dev) = resolve(&line.device) else {
            warnings.push(if required {
                format!(
                    "warning: {} for {} isn't attached now, and it's required, so a boot without it stops in emergency mode",
                    line.device,
                    line.key()
                )
            } else {
                format!(
                    "warning: {} for {} isn't attached now; boots without it go on without mounting it (nofail)",
                    line.device,
                    line.key()
                )
            });
            continue;
        };
        let want = if line.is_swap() { "swap" } else { line.fs_type.as_str() };
        if want == "auto" {
            continue;
        }
        let found = Command::new("blkid")
            .args(["-o", "value", "-s", "TYPE"])
            .arg(&dev)
            .output()
            .ok()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .unwrap_or_default();
        if !found.is_empty() && found != want {
            warnings.push(format!(
                "warning: {} ({}) for {} holds {found}, not {want}",
                line.device,
                dev.display(),
                line.key()
            ));
        }
    }
    Ok(warnings)
}

/// Whether `file` has mkswap's SWAPSPACE2 signature at the end of its first page.
fn has_swap_signature(file: &Path) -> bool {
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
    let mut signature = [0u8; 10];
    fs::File::open(file)
        .and_then(|mut f| {
            f.seek(SeekFrom::Start(page - 10))?;
            f.read_exact(&mut signature)
        })
        .is_ok()
        && &signature == b"SWAPSPACE2"
}

fn active_swap(file: &Path) -> bool {
    fs::read_to_string("/proc/swaps")
        .unwrap_or_default()
        .lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().next())
        .any(|path| Path::new(&unescape(path)) == file)
}

fn run(cmd: &mut Command, what: &str) -> Result<()> {
    let output = cmd.output().with_context(|| format!("running {what}"))?;
    if !output.status.success() {
        bail!("{what} failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

/// archix-swapfile at boot: makes the swap file `file` of `bytes` unless it's there with that size, on btrfs with `btrfs filesystem mkswapfile` in a subvolume created for its missing directory, elsewhere preallocated, 0600 and mkswap'd, under a temporary name renamed into place.
pub fn make_swapfile(file: &Path, bytes: u64) -> Result<()> {
    if active_swap(file) {
        return Ok(());
    }
    if let Ok(meta) = fs::symlink_metadata(file) {
        if !meta.is_file() {
            bail!("{} exists and isn't a file", file.display());
        }
        if meta.len() == bytes && has_swap_signature(file) {
            return Ok(());
        }
        println!(
            "archix: making the swap file {} again, as {bytes} bytes",
            file.display()
        );
        fs::remove_file(file).with_context(|| format!("removing {}", file.display()))?;
    }
    let dir = file.parent().context("a swap file needs a directory")?;
    if !dir.exists() {
        if crate::capture::is_btrfs(nearest_existing(dir)) {
            if let Some(parent) = dir.parent() {
                fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
            }
            run(
                Command::new("btrfs").args(["-q", "subvolume", "create"]).arg(dir),
                &format!("btrfs subvolume create {}", dir.display()),
            )?;
        } else {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
    }
    let name = file.file_name().context("a swap file needs a name")?.to_string_lossy();
    let temp = dir.join(format!(".{name}.archix-new"));
    let _ = fs::remove_file(&temp);
    if crate::capture::is_btrfs(dir) {
        if !own_subvolume(dir) {
            bail!(
                "{} is on btrfs in {}, which isn't a subvolume of its own, and btrfs can't snapshot a subvolume holding an active swap file; move it to a directory archix can create as a subvolume",
                file.display(),
                dir.display()
            );
        }
        run(
            Command::new("btrfs")
                .args(["filesystem", "mkswapfile", "--size", &bytes.to_string()])
                .arg(&temp),
            "btrfs filesystem mkswapfile",
        )?;
    } else {
        let handle = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .with_context(|| format!("creating {}", temp.display()))?;
        let length = libc::off_t::try_from(bytes).context("swap file size")?;
        if unsafe { libc::fallocate(std::os::fd::AsRawFd::as_raw_fd(&handle), 0, 0, length) } != 0 {
            let error = std::io::Error::last_os_error();
            let _ = fs::remove_file(&temp);
            bail!("preallocating {} bytes for {}: {error}", bytes, file.display());
        }
        handle.sync_all()?;
        drop(handle);
        if let Err(e) = run(Command::new("mkswap").arg(&temp), "mkswap") {
            let _ = fs::remove_file(&temp);
            return Err(e);
        }
    }
    fs::rename(&temp, file).with_context(|| format!("renaming {} to {}", temp.display(), file.display()))?;
    crate::durable::sync_dir(dir)?;
    println!("archix: made the swap file {} ({bytes} bytes)", file.display());
    Ok(())
}

/// What `archix init` takes from a machine's fstab, crypttab and zram-generator.conf into file_systems, swap and zram, with notes on what it leaves out and why.
#[derive(Debug, Default)]
pub struct Captured {
    pub file_systems: Vec<(String, Lua)>,
    pub swap: Vec<Lua>,
    pub zram: Option<Lua>,
    /// crypttab's every line went into encrypted, so /etc/crypttab isn't offered as a file.
    pub crypttab_taken: bool,
    pub zram_taken: bool,
    pub summary: Vec<String>,
    pub notes: Vec<String>,
}

/// file_systems, swap and zram for a machine whose fstab, crypttab and zram-generator.conf read so, with the ESP at `esp`; the root's and ESP's lines stay archix's.
pub fn capture(fstab: &str, crypttab: Option<&str>, zram: Option<&str>, esp: &str) -> Captured {
    let mut out = Captured::default();
    let crypt: Vec<Vec<String>> = crypttab.unwrap_or_default().lines().filter_map(fields).collect();
    let mut used = Vec::new();
    for fields in fstab.lines().filter_map(fields) {
        if fields.len() < 3 || kept(&fields, esp) {
            continue;
        }
        let (device, point, fs_type) = (&fields[0], &fields[1], &fields[2]);
        let options: Vec<String> = fields
            .get(3)
            .map(|o| {
                o.split(',')
                    .filter(|o| !o.is_empty() && *o != "defaults")
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let nofail = options.iter().any(|o| o == "nofail");
        let rest: Vec<String> = options
            .iter()
            .filter(|o| *o != "nofail" && !(nofail && o.as_str() == DEVICE_TIMEOUT))
            .cloned()
            .collect();
        if fs_type == "swap" {
            let mut table = vec![];
            if device.starts_with('/') && !device.starts_with("/dev/") {
                if let Some(why) = refused_swap_file(device, esp) {
                    out.notes.push(format!(
                        "swap: {device} is left out: a swap file can't be there, as {why}"
                    ));
                    continue;
                }
                table.push(("file".to_string(), Lua::str(device)));
            } else {
                table.push(("device".to_string(), Lua::str(device)));
            }
            let mut extra = Vec::new();
            for option in &rest {
                match option.strip_prefix("pri=").map(str::parse::<i64>) {
                    Some(Ok(priority)) => table.push(("priority".into(), Lua::Int(priority))),
                    _ => extra.push(option.clone()),
                }
            }
            if !extra.is_empty() {
                table.push(("options".into(), Lua::strings(&extra)));
            }
            if !nofail {
                table.push(("required".into(), Lua::Bool(true)));
            }
            out.swap.push(Lua::Table(table));
            continue;
        }
        if let Err(why) = check_path(point) {
            out.notes.push(format!(
                "file_systems: the line for {point:?} is left out, as its mount point {why}"
            ));
            continue;
        }
        if let Some(why) = refused_point(point, esp) {
            out.notes
                .push(format!("file_systems: the line for {point} is left out: {why}"));
            continue;
        }
        let bind = rest.iter().any(|o| o == "bind" || o == "rbind");
        let mut table = vec![("device".to_string(), Lua::str(device))];
        if fs_type != "auto" && !(bind && fs_type == "none") {
            table.push(("fs_type".into(), Lua::str(fs_type)));
        }
        if !rest.is_empty() {
            table.push(("options".into(), Lua::strings(&rest)));
        }
        if fields.get(5).and_then(|n| n.parse::<u32>().ok()).is_some_and(|n| n > 0) && !bind && fs_type != "tmpfs" {
            table.push(("check".into(), Lua::Bool(true)));
        }
        if !nofail {
            table.push(("required".into(), Lua::Bool(true)));
        }
        if let Some(name) = device.strip_prefix("/dev/mapper/")
            && let Some(line) = crypt.iter().find(|line| line[0] == name)
        {
            used.push(name.to_string());
            let mut encrypted = vec![("device".to_string(), Lua::str(line.get(1).cloned().unwrap_or_default()))];
            if let Some(key) = line.get(2).filter(|key| !["none", "-", ""].contains(&key.as_str())) {
                encrypted.push(("key_file".into(), Lua::str(key)));
            }
            let crypt_options: Vec<String> = line
                .get(3)
                .map(|o| {
                    o.split(',')
                        .filter(|o| !o.is_empty() && *o != "luks" && *o != "nofail")
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            if !crypt_options.is_empty() {
                encrypted.push(("options".into(), Lua::strings(&crypt_options)));
            }
            table.push(("encrypted".into(), Lua::Table(encrypted)));
        }
        out.file_systems.push((point.clone(), Lua::Table(table)));
    }
    let unused: Vec<&str> = crypt
        .iter()
        .map(|line| line[0].as_str())
        .filter(|name| !used.iter().any(|u| u == name))
        .collect();
    if !crypt.is_empty() && unused.is_empty() {
        out.crypttab_taken = true;
    } else if !used.is_empty() {
        out.notes.push(format!(
            "file_systems: /etc/crypttab opens {}, which no mount declares, so it's offered as a file and the mounts on /dev/mapper keep relying on it instead of encrypted",
            unused.join(", ")
        ));
        for (_, table) in &mut out.file_systems {
            if let Lua::Table(fields) = table {
                fields.retain(|(key, _)| key != "encrypted");
            }
        }
    }
    if let Some(zram) = zram {
        match zram_from(zram) {
            Ok(table) => {
                out.zram = Some(Lua::Table(table));
                out.zram_taken = true;
                out.summary.push("zram".into());
            }
            Err(why) => out.notes.push(format!(
                "zram: /etc/systemd/zram-generator.conf {why}, so it's offered as a file instead"
            )),
        }
    }
    if !out.file_systems.is_empty() {
        out.summary.push(format!(
            "{} in file_systems",
            count(out.file_systems.len(), "mount", "mounts")
        ));
    }
    if !out.swap.is_empty() {
        out.summary
            .push(format!("{} in swap", count(out.swap.len(), "swap line", "swap lines")));
    }
    out
}

/// The zram option for this zram-generator.conf, or why it can't say what the file does.
fn zram_from(text: &str) -> std::result::Result<Vec<(String, Lua)>, String> {
    let mut table = vec![("enable".to_string(), Lua::Bool(true))];
    let mut section = None;
    for line in text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with(['#', ';']))
    {
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            if section.is_some() || name != "zram0" {
                return Err(format!("sets up [{name}], and the zram option only [zram0]"));
            }
            section = Some(name.to_string());
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("has the line {line:?}"));
        };
        let value = value.trim();
        match key.trim() {
            "zram-size" => table.push((
                "size".into(),
                value.parse::<i64>().map_or_else(|_| Lua::str(value), Lua::Int),
            )),
            "compression-algorithm" if !value.contains(char::is_whitespace) => {
                table.push(("algorithm".into(), Lua::str(value)))
            }
            "swap-priority" => match value.parse() {
                Ok(priority) => table.push(("priority".into(), Lua::Int(priority))),
                Err(_) => return Err(format!("sets swap-priority to {value:?}")),
            },
            other => return Err(format!("sets {other}, which the zram option doesn't")),
        }
    }
    if section.is_none() {
        return Err("sets up no device".into());
    }
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(json: serde_json::Value) -> Value {
        match json {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(b),
            serde_json::Value::Number(n) => Value::Int(n.as_i64().unwrap()),
            serde_json::Value::String(s) => Value::Str(s),
            serde_json::Value::Array(items) => Value::List(items.into_iter().map(config).collect()),
            serde_json::Value::Object(map) => Value::Map(map.into_iter().map(|(k, v)| (k, config(v))).collect()),
        }
    }

    fn lines(declared: &Declared) -> Vec<String> {
        declared.lines.iter().map(FstabLine::line).collect()
    }

    fn error(json: serde_json::Value) -> String {
        render(&config(json), "/efi")
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default()
    }

    #[test]
    fn mounts_are_nofail_with_a_short_device_wait_unless_required() {
        let declared = render(
            &config(serde_json::json!({
                "file_systems": {
                    "/mnt/data": { "device": "UUID=1234", "fs_type": "ext4", "options": ["noatime"], "check": true },
                    "/mnt/data/sub": { "device": "LABEL=sub", "fs_type": "btrfs", "options": ["subvol=@sub", "x-systemd.device-timeout=5s"] },
                    "/home": { "device": "/dev/vdb1", "fs_type": "ext4", "required": true, "check": true },
                    "/srv/www": { "device": "/var/www", "options": ["bind"] },
                    "/mnt/my disk": { "device": "tmpfs", "fs_type": "tmpfs", "options": ["size=16M", "mode=1777"] },
                    "/mnt/nas": { "device": "nas:/export", "fs_type": "nfs" },
                }
            })),
            "/efi",
        )
        .unwrap();
        assert_eq!(
            lines(&declared),
            [
                "/dev/vdb1 /home ext4 defaults 0 2",
                "UUID=1234 /mnt/data ext4 noatime,nofail,x-systemd.device-timeout=30s 0 2",
                "LABEL=sub /mnt/data/sub btrfs subvol=@sub,x-systemd.device-timeout=5s,nofail 0 0",
                "tmpfs /mnt/my\\040disk tmpfs size=16M,mode=1777,nofail 0 0",
                "nas:/export /mnt/nas nfs nofail 0 0",
                "/var/www /srv/www none bind,nofail 0 0",
            ]
        );
        assert!(declared.files.is_empty());
    }

    #[test]
    fn mount_points_archix_lays_out_or_writes_itself_are_refused() {
        for (point, why) in [
            ("/", "root's line"),
            ("/efi", "ESP's line"),
            ("/efi/EFI", "ESP's line"),
            ("/usr", "own /usr"),
            ("/usr/share", "own /usr"),
            ("/opt", "own /opt"),
            ("/boot", "own /boot"),
            ("/etc", "own /etc"),
            ("/archix", "/archix"),
            ("/var", "pacman database"),
            ("/var/lib", "pacman database"),
            ("/var/lib/pacman", "pacman database"),
            ("/var/lib/dkms", "DKMS"),
            ("/var/usrlocal", "/usr/local instead"),
            ("relative", "absolute"),
            ("/mnt/../etc", "plain absolute"),
            ("/mnt/", "end in /"),
        ] {
            let found = error(serde_json::json!({ "file_systems": { point: { "device": "UUID=1" } } }));
            assert!(found.contains(why), "{point}: {found}");
        }
        for point in ["/usr/local", "/var/lib/docker", "/var/log", "/home", "/boot2", "/efi2"] {
            assert!(
                render(
                    &config(serde_json::json!({ "file_systems": { point: { "device": "UUID=1" } } })),
                    "/efi"
                )
                .is_ok(),
                "{point}"
            );
        }
        for (spec, why) in [
            (
                serde_json::json!({ "device": "data", "options": ["bind"] }),
                "directory to bind",
            ),
            (
                serde_json::json!({ "device": "/a", "options": ["bind"], "check": true }),
                "fsck",
            ),
            (
                serde_json::json!({ "device": "UUID=1", "required": true, "options": ["nofail"] }),
                "contradict",
            ),
            (
                serde_json::json!({ "device": "UUID=1", "options": ["a b"] }),
                "without spaces",
            ),
            (
                serde_json::json!({ "device": "UUID=1", "encrypted": { "device": "UUID=2" } }),
                "/dev/mapper/<name>",
            ),
            (serde_json::json!({ "device": "" }), "device must name"),
        ] {
            let found = error(serde_json::json!({ "file_systems": { "/mnt/x": spec } }));
            assert!(found.contains(why), "{spec}: {found}");
        }
    }

    #[test]
    fn encrypted_mounts_get_crypttab_lines() {
        let declared = render(
            &config(serde_json::json!({
                "file_systems": {
                    "/mnt/secret": { "device": "/dev/mapper/secret", "fs_type": "ext4", "encrypted": { "device": "UUID=luks", "key_file": "/etc/keys/secret.key", "options": ["discard"] } },
                    "/mnt/vault": { "device": "/dev/mapper/vault", "required": true, "encrypted": { "device": "/dev/vdc" } },
                }
            })),
            "/efi",
        )
        .unwrap();
        assert_eq!(
            declared.files,
            [(
                "/etc/crypttab".to_string(),
                "# Written by archix from file_systems' encrypted devices.\nsecret UUID=luks /etc/keys/secret.key luks,discard,nofail\nvault /dev/vdc none luks\n".to_string(),
                "file_systems' encrypted devices"
            )]
        );
        assert_eq!(
            declared.lines[0].options, "nofail",
            "a mapper device isn't waited for by udev"
        );
    }

    #[test]
    fn swap_files_with_a_size_are_made_at_boot_and_never_in_a_deployment() {
        let declared = render(
            &config(serde_json::json!({
                "swap": [
                    { "file": "/swap/swapfile", "size": "8G", "priority": 10 },
                    { "device": "UUID=abc" },
                    { "file": "/var/swap.img", "required": true, "options": ["discard"] },
                ]
            })),
            "/efi",
        )
        .unwrap();
        assert_eq!(
            lines(&declared),
            [
                "/swap/swapfile none swap pri=10,nofail 0 0",
                "UUID=abc none swap nofail,x-systemd.device-timeout=30s 0 0",
                "/var/swap.img none swap discard 0 0",
            ]
        );
        assert_eq!(declared.lines[0].size, Some(8 << 30));
        let files: Vec<&str> = declared.files.iter().map(|(path, _, _)| path.as_str()).collect();
        assert_eq!(
            files,
            [
                "/etc/systemd/system/archix-swapfile-swap-swapfile.service",
                "/etc/systemd/system/swap-swapfile.swap.d/archix-swapfile.conf"
            ]
        );
        assert!(
            declared.files[0]
                .1
                .contains("ExecStart=/usr/bin/archix swapfile /swap/swapfile 8589934592\n")
        );
        assert!(
            declared.files[0]
                .1
                .contains("Before=swap-swapfile.swap shutdown.target\n")
        );
        for (spec, why) in [
            (
                serde_json::json!({ "file": "/archix/swapfile", "size": "1G" }),
                "deployments live in /archix",
            ),
            (
                serde_json::json!({ "file": "/usr/local/swapfile", "size": "1G" }),
                "own /usr",
            ),
            (serde_json::json!({ "file": "/etc/swapfile" }), "own /etc"),
            (serde_json::json!({ "file": "/var/lib/pacman/swap" }), "pacman database"),
            (serde_json::json!({ "file": "/efi/swap" }), "ESP"),
            (
                serde_json::json!({ "file": "/swapfile", "size": "lots" }),
                "isn't a size",
            ),
            (
                serde_json::json!({ "file": "/swapfile", "size": "100K" }),
                "less than a MiB",
            ),
            (
                serde_json::json!({ "device": "UUID=1", "size": "1G" }),
                "size is for a swap file",
            ),
            (
                serde_json::json!({ "device": "UUID=1", "file": "/swapfile" }),
                "one of device or file",
            ),
        ] {
            let found = error(serde_json::json!({ "swap": [spec] }));
            assert!(found.contains(why), "{spec}: {found}");
        }
        assert!(
            error(serde_json::json!({ "swap": [{ "device": "UUID=1" }, { "device": "UUID=1" }] })).contains("twice")
        );
    }

    #[test]
    fn zram_writes_zram_generator_conf() {
        let declared =
            render(&config(serde_json::json!({ "zram": { "enable": true, "size": "ram / 2", "algorithm": "zstd", "priority": 5 } })), "/efi")
                .unwrap();
        assert_eq!(
            declared.files,
            [(
                ZRAM_CONF.to_string(),
                "# Written by archix from zram.\n[zram0]\nzram-size = ram / 2\ncompression-algorithm = zstd\nswap-priority = 5\n".to_string(),
                "zram"
            )]
        );
        let mib = render(
            &config(serde_json::json!({ "zram": { "enable": true, "size": 2048 } })),
            "/efi",
        )
        .unwrap();
        assert!(mib.files[0].1.contains("zram-size = 2048\n"));
        assert!(
            render(
                &config(serde_json::json!({ "zram": { "enable": false, "size": 2048 } })),
                "/efi"
            )
            .unwrap()
            .files
            .is_empty()
        );
        assert!(error(serde_json::json!({ "zram": { "enable": true, "size": true } })).contains("zram.size"));
    }

    #[test]
    fn the_fstab_keeps_the_root_and_esp_lines_with_the_esps_protections_and_drops_the_rest() {
        let old = "# /dev/vda2\nUUID=root / btrfs rw,subvol=/@ 0 0\n\n# esp\nUUID=b /efi vfat defaults 0 0\nUUID=home /home ext4 defaults 0 2\n/swapfile none swap defaults 0 0\n";
        let declared = [FstabLine {
            device: "UUID=data".into(),
            point: "/mnt/data".into(),
            fs_type: "ext4".into(),
            options: "nofail".into(),
            passno: 2,
            size: None,
        }];
        let (text, changed) = compose(old, "/efi", None, &declared);
        assert_eq!(
            text,
            format!(
                "{HEADER}\nUUID=root / btrfs rw,subvol=/@ 0 0\nUUID=b /efi vfat defaults,x-systemd.automount,x-systemd.idle-timeout=60,fmask=0077,dmask=0077 0 2\nUUID=data /mnt/data ext4 nofail 0 2\n"
            )
        );
        assert!(changed.is_some_and(|c| c.automount && c.fsck && c.private));
        let (again, changed) = compose(&text, "/efi", None, &declared);
        assert_eq!(
            (again, changed),
            (text.clone(), None),
            "writing it again changes nothing"
        );
        let (added, changed) = compose("UUID=root / ext4 rw 0 1\n", "/efi", Some("AB12-CD34"), &[]);
        assert_eq!(
            added,
            format!(
                "{HEADER}\nUUID=root / ext4 rw 0 1\nUUID=AB12-CD34 /efi vfat umask=0077,x-systemd.automount,x-systemd.idle-timeout=60 0 2\n"
            )
        );
        assert!(changed.is_some_and(|c| c.added));
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("fstab"), old).unwrap();
        write_in(dir.path(), "/efi", &declared).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("fstab")).unwrap(), text);
        assert_eq!(
            describe(old, "/efi", &declared),
            [
                "fstab      mount /mnt/data (UUID=data) at the next boot",
                "fstab      drop /home (UUID=home) at the next boot, as neither file_systems nor swap declares it",
                "fstab      drop swap on /swapfile at the next boot, as neither file_systems nor swap declares it",
            ]
        );
        assert!(describe(&text, "/efi", &declared).is_empty());
    }

    #[test]
    fn unit_names_escape_paths_as_systemd_does() {
        assert_eq!(escape_path("/swap/swapfile"), "swap-swapfile");
        assert_eq!(escape_path("/var/my-swap/.file"), "var-my\\x2dswap-.file");
        assert_eq!(escape_path("/.swap"), "\\x2eswap");
        assert_eq!(size_bytes("512M"), Some(512 << 20));
        assert_eq!(size_bytes("1GiB"), Some(1 << 30));
        assert_eq!(size_bytes("4096"), Some(4096));
        assert_eq!(size_bytes("-1G"), None);
    }

    #[test]
    fn a_mapper_mount_needs_a_crypttab_line_or_an_initramfs_that_opens_it() {
        let line = FstabLine {
            device: "/dev/mapper/archix-test-nothing".into(),
            point: "/mnt/x".into(),
            fs_type: "ext4".into(),
            options: "nofail".into(),
            passno: 0,
            size: None,
        };
        let root = Path::new("/");
        assert!(
            check(root, std::slice::from_ref(&line), "", "")
                .unwrap_err()
                .to_string()
                .contains("no /etc/crypttab line")
        );
        assert!(
            check(
                root,
                std::slice::from_ref(&line),
                "archix-test-nothing UUID=1 none luks\n",
                ""
            )
            .is_ok()
        );
        assert!(check(root, &[line], "", "rd.luks.name=1234=archix-test-nothing quiet").is_ok());
    }

    #[test]
    fn init_captures_fstab_lines_crypttab_and_zram_and_says_what_it_leaves() {
        let fstab = "# <file system> <dir> <type> <options> <dump> <pass>\n\
                     UUID=root / ext4 rw,relatime 0 1\n\
                     UUID=ESP /efi vfat rw,fmask=0077 0 2\n\
                     UUID=home /home ext4 rw,relatime 0 2\n\
                     LABEL=media /mnt/media\\040disk auto nofail,x-systemd.device-timeout=30s 0 0\n\
                     /dev/mapper/secret /mnt/secret ext4 defaults,nofail 0 0\n\
                     /srv/a /srv/b none bind 0 0\n\
                     UUID=var /var ext4 defaults 0 2\n\
                     /swapfile none swap defaults,pri=5 0 0\n\
                     UUID=sw none swap defaults,nofail 0 0\n";
        let crypttab = "secret UUID=luks /etc/keys/s.key luks,discard\n";
        let zram = "[zram0]\nzram-size = ram / 2\ncompression-algorithm = zstd\n";
        let captured = capture(fstab, Some(crypttab), Some(zram), "/efi");
        let rendered: Vec<String> = captured
            .file_systems
            .iter()
            .map(|(point, table)| {
                let flat = table.render(0).split_whitespace().collect::<Vec<_>>().join(" ");
                format!("{point} {}", flat.replace(", }", " }"))
            })
            .collect();
        assert_eq!(
            rendered,
            [
                "/home { device = \"UUID=home\", fs_type = \"ext4\", options = { \"rw\", \"relatime\" }, check = true, required = true }",
                "/mnt/media disk { device = \"LABEL=media\" }",
                "/mnt/secret { device = \"/dev/mapper/secret\", fs_type = \"ext4\", encrypted = { device = \"UUID=luks\", key_file = \"/etc/keys/s.key\", options = { \"discard\" } } }",
                "/srv/b { device = \"/srv/a\", options = { \"bind\" }, required = true }",
            ]
        );
        assert!(captured.crypttab_taken && captured.zram_taken);
        assert_eq!(
            captured
                .zram
                .unwrap()
                .render(0)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            "{ enable = true, size = \"ram / 2\", algorithm = \"zstd\" }"
        );
        assert_eq!(captured.swap.len(), 2);
        assert!(
            captured.notes.iter().any(|note| note.contains("/var is left out")),
            "{:?}",
            captured.notes
        );
        let partial = capture(
            fstab,
            Some("secret UUID=luks none\nother UUID=x none\n"),
            Some("[zram1]\n"),
            "/efi",
        );
        assert!(!partial.crypttab_taken && !partial.zram_taken);
        assert!(
            partial.notes.iter().any(|note| note.contains("opens other")),
            "{:?}",
            partial.notes
        );
        assert!(
            partial.notes.iter().any(|note| note.contains("[zram1]")),
            "{:?}",
            partial.notes
        );
    }

    #[test]
    fn captured_lines_render_back_to_the_same_mounts() {
        let fstab = "UUID=root / ext4 rw 0 1\nUUID=home /home ext4 rw,relatime 0 2\nLABEL=m /mnt/m auto nofail,x-systemd.device-timeout=30s 0 0\n/srv/a /srv/b none bind 0 0\n/swapfile none swap pri=5 0 0\n";
        let captured = capture(fstab, None, None, "/efi");
        fn json(lua: &Lua) -> serde_json::Value {
            match lua {
                Lua::Str(s) => s.clone().into(),
                Lua::Int(n) => (*n).into(),
                Lua::Bool(b) => (*b).into(),
                Lua::List(items) => items.iter().map(json).collect(),
                Lua::Table(fields) => fields
                    .iter()
                    .map(|(k, v)| (k.clone(), json(v)))
                    .collect::<serde_json::Map<_, _>>()
                    .into(),
            }
        }
        let file_systems: serde_json::Map<String, serde_json::Value> = captured
            .file_systems
            .iter()
            .map(|(point, table)| (point.clone(), json(table)))
            .collect();
        let swap: Vec<serde_json::Value> = captured.swap.iter().map(json).collect();
        let declared = render(
            &config(serde_json::json!({ "file_systems": file_systems, "swap": swap })),
            "/efi",
        )
        .unwrap();
        let (text, _) = compose(fstab, "/efi", None, &declared.lines);
        assert_eq!(
            text,
            format!(
                "{HEADER}\nUUID=root / ext4 rw 0 1\nUUID=home /home ext4 rw,relatime 0 2\nLABEL=m /mnt/m auto nofail,x-systemd.device-timeout=30s 0 0\n/srv/a /srv/b none bind 0 0\n/swapfile none swap pri=5 0 0\n"
            )
        );
    }
}

//! Reads an existing Arch install into the pieces `archix init` turns into a config.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CStr;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use md5::{Digest, Md5};

use crate::pacman::parse_desc;
use crate::repos::{Custom, is_official};

/// Directories whose files no package owns are still config someone wrote, so init offers to capture them.
pub const CONFIG_DIRS: &[&str] = &[
    "/etc/systemd/system",
    "/etc/systemd/network",
    "/etc/systemd/logind.conf.d",
    "/etc/systemd/journald.conf.d",
    "/etc/systemd/resolved.conf.d",
    "/etc/sudoers.d",
    "/etc/modprobe.d",
    "/etc/modules-load.d",
    "/etc/sysctl.d",
    "/etc/udev/rules.d",
    "/etc/X11/xorg.conf.d",
    "/etc/tmpfiles.d",
    "/etc/mkinitcpio.conf.d",
    "/etc/environment.d",
];

/// Files archix or other tools own, which a config must never take over.
const NEVER: &[&str] = &[
    "/etc/pacman.conf",
    "/etc/passwd",
    "/etc/passwd-",
    "/etc/group",
    "/etc/group-",
    "/etc/shadow",
    "/etc/shadow-",
    "/etc/gshadow",
    "/etc/gshadow-",
    "/etc/machine-id",
    "/etc/resolv.conf",
    "/etc/mtab",
    "/etc/ld.so.cache",
    "/etc/kernel/cmdline",
    "/etc/sysctl.d/90-archix.conf",
    "/etc/modules-load.d/archix.conf",
];

const NEVER_UNDER: &[&str] = &[
    "/etc/pacman.d/gnupg/",
    "/etc/ca-certificates/extracted/",
    "/etc/ssl/certs/",
];

/// Paths that typically hold credentials; init only copies them into the config when asked to.
const SECRET_HINTS: &[&str] = &[
    "/etc/ssh/ssh_host_",
    "/etc/NetworkManager/system-connections/",
    "/etc/wpa_supplicant/",
    "/etc/iwd/",
    "/etc/wireguard/",
    "/etc/ssl/private/",
    "/etc/krb5.keytab",
    "secret",
    "password",
    ".key",
    ".pem",
];

#[derive(Debug, Clone, PartialEq)]
pub struct FoundFile {
    pub path: String,
    pub mode: u32,
    pub owner: String,
    pub group: String,
    /// Not world-readable or named like a credential.
    pub secret: bool,
    /// Edited from what its package shipped, rather than written from scratch.
    pub edited: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Account {
    pub uid: u32,
    /// None when it's the user's own group.
    pub group: Option<String>,
    pub groups: Vec<String>,
    pub shell: String,
    /// None when it's /home/<name>.
    pub home: Option<String>,
    pub description: String,
}

#[derive(Debug, Default)]
pub struct Accounts {
    pub users: BTreeMap<String, Account>,
    /// Groups in the regular id range that aren't some captured user's own group.
    pub groups: BTreeMap<String, u32>,
}

/// One package's entry in the local database, as far as init cares.
#[derive(Debug, Default)]
pub struct Installed {
    pub build_date: Option<u64>,
    pub files: Vec<String>,
    /// Backup-listed files and the md5 pacman installed them with.
    pub backups: Vec<(String, String)>,
}

pub fn read_installed(local_db: &Path) -> Result<BTreeMap<String, Installed>> {
    let mut out = BTreeMap::new();
    for entry in fs::read_dir(local_db).with_context(|| format!("reading {}", local_db.display()))? {
        let dir = entry?.path();
        let Ok(desc) = fs::read_to_string(dir.join("desc")) else {
            continue;
        };
        let desc = parse_desc(&desc);
        let Some(name) = desc.get("NAME").and_then(|v| v.first()) else {
            continue;
        };
        let files = parse_desc(&fs::read_to_string(dir.join("files")).unwrap_or_default());
        let backups = files
            .get("BACKUP")
            .into_iter()
            .flatten()
            .filter_map(|line| line.split_once('\t'))
            .map(|(path, md5)| (format!("/{path}"), md5.to_string()))
            .collect();
        out.insert(
            name.clone(),
            Installed {
                build_date: desc
                    .get("BUILDDATE")
                    .and_then(|v| v.first())
                    .and_then(|d| d.parse().ok()),
                files: files.get("FILES").cloned().unwrap_or_default(),
                backups,
            },
        );
    }
    Ok(out)
}

pub fn md5(path: &Path) -> Result<String> {
    let mut hasher = Md5::new();
    let mut file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut buf = vec![0; 1 << 16];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn excluded(path: &str, skip: &BTreeSet<String>) -> bool {
    NEVER.contains(&path) || NEVER_UNDER.iter().any(|prefix| path.starts_with(prefix)) || skip.contains(path)
}

pub fn looks_secret(path: &str, mode: u32) -> bool {
    mode & 0o004 == 0 || SECRET_HINTS.iter().any(|hint| path.contains(hint))
}

fn found(path: &str, edited: bool) -> Result<Option<FoundFile>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => meta,
        _ => return Ok(None),
    };
    let mode = meta.mode() & 0o7777;
    Ok(Some(FoundFile {
        path: path.to_string(),
        mode,
        owner: user_name(meta.uid()).unwrap_or_else(|| meta.uid().to_string()),
        group: group_name(meta.gid()).unwrap_or_else(|| meta.gid().to_string()),
        secret: looks_secret(path, mode),
        edited,
    }))
}

/// Backup files edited since their package installed them, plus files nobody owns in [`CONFIG_DIRS`].
pub fn config_files(installed: &BTreeMap<String, Installed>, skip: &BTreeSet<String>) -> Result<Vec<FoundFile>> {
    let mut out = BTreeMap::new();
    for pkg in installed.values() {
        for (path, sum) in &pkg.backups {
            if excluded(path, skip) || !Path::new(path).is_file() {
                continue;
            }
            if md5(Path::new(path)).is_ok_and(|now| now != *sum)
                && let Some(file) = found(path, true)?
            {
                out.insert(path.clone(), file);
            }
        }
    }
    let owned: BTreeSet<String> = installed
        .values()
        .flat_map(|pkg| pkg.files.iter().map(|f| format!("/{f}")))
        .collect();
    for dir in CONFIG_DIRS {
        let mut stack = vec![Path::new(dir).to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(kind) = entry.file_type() else { continue };
                if kind.is_dir() {
                    stack.push(path);
                    continue;
                }
                let text = path.to_string_lossy().into_owned();
                if !kind.is_file() || owned.contains(&text) || excluded(&text, skip) || out.contains_key(&text) {
                    continue;
                }
                if let Some(file) = found(&text, false)? {
                    out.insert(text, file);
                }
            }
        }
    }
    Ok(out.into_values().collect())
}

fn user_name(uid: u32) -> Option<String> {
    let entry = unsafe { libc::getpwuid(uid) };
    (!entry.is_null()).then(|| {
        unsafe { CStr::from_ptr((*entry).pw_name) }
            .to_string_lossy()
            .into_owned()
    })
}

fn group_name(gid: u32) -> Option<String> {
    let entry = unsafe { libc::getgrgid(gid) };
    (!entry.is_null()).then(|| {
        unsafe { CStr::from_ptr((*entry).gr_name) }
            .to_string_lossy()
            .into_owned()
    })
}

/// Official repos in order, and third-party ones with their servers (Include'd mirrorlists read through `read`).
pub fn parse_pacman_conf(text: &str, read: impl Fn(&str) -> Option<String>) -> (Vec<String>, Vec<(String, Custom)>) {
    let mut official = Vec::new();
    let mut custom: Vec<(String, Custom)> = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines().map(str::trim) {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            current = (name != "options").then(|| name.to_string());
            match &current {
                Some(name) if is_official(name) => official.push(name.clone()),
                Some(name) => custom.push((
                    name.clone(),
                    Custom {
                        servers: Vec::new(),
                        keys: Vec::new(),
                        keyserver: "hkps://keyserver.ubuntu.com".into(),
                        sig_level: String::new(),
                        before_official: official.is_empty(),
                    },
                )),
                None => {}
            }
            continue;
        }
        let Some(name) = &current else { continue };
        let Some((_, repo)) = custom.iter_mut().find(|(n, _)| n == name) else {
            continue;
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "Server" => repo.servers.push(value.to_string()),
            "SigLevel" => repo.sig_level = value.to_string(),
            "Include" => {
                for line in read(value).unwrap_or_default().lines().map(str::trim) {
                    if let Some((key, server)) = line.split_once('=')
                        && key.trim() == "Server"
                    {
                        repo.servers.push(server.trim().to_string());
                    }
                }
            }
            _ => {}
        }
    }
    for (_, repo) in &mut custom {
        if repo.sig_level.is_empty() {
            repo.sig_level = "Required DatabaseOptional".into();
        }
    }
    (official, custom)
}

/// Units whose enabled state differs from their preset, from `systemctl list-unit-files --no-legend`.
pub fn parse_unit_files(text: &str) -> BTreeMap<String, bool> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [unit, state, preset, ..] = fields[..] else {
            continue;
        };
        if unit.contains("@.") {
            continue;
        }
        match (state, preset) {
            ("enabled", "disabled") => out.insert(unit.to_string(), true),
            ("disabled", "enabled") => out.insert(unit.to_string(), false),
            _ => None,
        };
    }
    out
}

pub fn changed_services() -> Result<BTreeMap<String, bool>> {
    let output = Command::new("systemctl")
        .args([
            "list-unit-files",
            "--no-legend",
            "--no-pager",
            "--type=service,socket,timer,path",
        ])
        .output()
        .context("running systemctl")?;
    Ok(parse_unit_files(&String::from_utf8_lossy(&output.stdout)))
}

/// The kernel command line minus what the boot loader and archix add themselves.
pub fn kernel_params(cmdline: &str) -> Vec<String> {
    cmdline
        .split_whitespace()
        .filter(|p| !p.starts_with("BOOT_IMAGE=") && !p.starts_with("initrd=") && !p.starts_with("archix."))
        .map(str::to_string)
        .collect()
}

/// Regular users (uid 1000 to 59999) and the regular groups that aren't one of their own groups.
pub fn parse_accounts(passwd: &str, group: &str) -> Accounts {
    let groups: Vec<(String, u32, Vec<String>)> = group
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(':').collect();
            let [name, _, gid, members] = fields[..] else {
                return None;
            };
            let members = members
                .split(',')
                .filter(|m| !m.is_empty())
                .map(str::to_string)
                .collect();
            Some((name.to_string(), gid.parse().ok()?, members))
        })
        .collect();
    let regular = |id: u32| (1000..60000).contains(&id);
    let mut accounts = Accounts::default();
    for line in passwd.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        let [name, _, uid, gid, gecos, home, shell] = fields[..] else {
            continue;
        };
        let (Ok(uid), Ok(gid)) = (uid.parse::<u32>(), gid.parse::<u32>()) else {
            continue;
        };
        if !regular(uid) {
            continue;
        }
        let primary = groups.iter().find(|(_, id, _)| *id == gid).map(|(n, _, _)| n.clone());
        let mut supplementary: Vec<String> = groups
            .iter()
            .filter(|(_, _, members)| members.iter().any(|m| m == name))
            .map(|(n, _, _)| n.clone())
            .collect();
        supplementary.sort();
        accounts.users.insert(
            name.to_string(),
            Account {
                uid,
                group: primary.filter(|g| g != name),
                groups: supplementary,
                shell: shell.to_string(),
                home: (home != format!("/home/{name}")).then(|| home.to_string()),
                description: gecos.to_string(),
            },
        );
    }
    for (name, gid, _) in &groups {
        let own = accounts.users.get(name).is_some_and(|user| user.group.is_none());
        if regular(*gid) && !own {
            accounts.groups.insert(name.clone(), *gid);
        }
    }
    accounts
}

/// The packages `pacman -Qq<flags>` lists; an empty list makes pacman exit 1, which isn't an error here.
pub fn pacman_query(flags: &str) -> Result<Vec<String>> {
    let output = Command::new("pacman")
        .arg(format!("-Qq{flags}"))
        .output()
        .context("running pacman")?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() && !stderr.trim().is_empty() {
        bail!("pacman -Qq{flags} failed: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect())
}

pub fn is_btrfs(path: &Path) -> bool {
    const BTRFS_SUPER_MAGIC: i64 = 0x9123_683e;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    unsafe { libc::statfs(c_path.as_ptr(), &mut stat) == 0 && stat.f_type as i64 == BTRFS_SUPER_MAGIC }
}

/// The ESP when systemd-boot is installed on it.
pub fn systemd_boot_esp() -> Option<String> {
    let installed = Command::new("bootctl")
        .arg("is-installed")
        .output()
        .is_ok_and(|o| o.status.success());
    if !installed {
        return None;
    }
    let output = Command::new("bootctl").arg("--print-esp-path").output().ok()?;
    let esp = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !esp.is_empty()).then_some(esp)
}

/// Home entries worth offering as dotfiles: everything in ~/.config plus top-level dotfiles, minus caches and keys.
pub fn dotfile_candidates(home: &Path) -> Vec<String> {
    const SKIP: &[&str] = &[
        ".cache",
        ".local",
        ".ssh",
        ".gnupg",
        ".pki",
        ".mozilla",
        ".config",
        ".cargo",
        ".rustup",
        ".npm",
        ".var",
        ".steam",
        ".wine",
        ".java",
        ".dbus",
        ".Xauthority",
        ".ICEauthority",
        ".lesshst",
        ".viminfo",
        ".python_history",
        ".node_repl_history",
        ".sudo_as_admin_successful",
    ];
    let mut out = Vec::new();
    for entry in fs::read_dir(home.join(".config")).into_iter().flatten().flatten() {
        out.push(format!(".config/{}", entry.file_name().to_string_lossy()));
    }
    for entry in fs::read_dir(home).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') && !SKIP.contains(&name.as_str()) && !name.contains("history") {
            out.push(name);
        }
    }
    out.retain(|name| !looks_secret(name, 0o644));
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pacman_conf_repos_and_mirrorlists() {
        let conf = "[options]\nSigLevel = Required\n\n[cachyos]\nInclude = /etc/pacman.d/cachyos-mirrorlist\n\n\
                    [core]\nInclude = /etc/pacman.d/mirrorlist\n\n[extra]\nInclude = /etc/pacman.d/mirrorlist\n\n\
                    #[multilib]\n#Include = /etc/pacman.d/mirrorlist\n\n[chaotic-aur]\nSigLevel = Optional\n\
                    Server = https://a.example/$repo/$arch\n";
        let (official, custom) = parse_pacman_conf(conf, |path| {
            (path == "/etc/pacman.d/cachyos-mirrorlist")
                .then(|| "# Server = https://off.example\nServer = https://on.example/$arch/$repo\n".to_string())
        });
        assert_eq!(official, ["core", "extra"]);
        assert_eq!(custom[0].0, "cachyos");
        assert!(custom[0].1.before_official);
        assert_eq!(custom[0].1.servers, ["https://on.example/$arch/$repo"]);
        assert_eq!(custom[0].1.sig_level, "Required DatabaseOptional");
        assert_eq!(custom[1].0, "chaotic-aur");
        assert!(!custom[1].1.before_official);
        assert_eq!(custom[1].1.sig_level, "Optional");
    }

    #[test]
    fn services_that_differ_from_their_preset() {
        let text = "sshd.service enabled disabled\nsystemd-networkd.service disabled enabled\n\
                    getty@.service enabled enabled\nfoo@.service enabled disabled\nstatic.service static -\n\
                    bar.timer enabled enabled\n";
        assert_eq!(
            parse_unit_files(text),
            BTreeMap::from([
                ("sshd.service".to_string(), true),
                ("systemd-networkd.service".to_string(), false)
            ])
        );
    }

    #[test]
    fn kernel_params_drop_loader_additions() {
        assert_eq!(
            kernel_params(
                "BOOT_IMAGE=/vmlinuz-linux initrd=\\initramfs.img root=UUID=x rw archix.generation=3 quiet\n"
            ),
            ["root=UUID=x", "rw", "quiet"]
        );
    }

    #[test]
    fn accounts_skip_system_users_and_private_groups() {
        let passwd = "root:x:0:0::/root:/bin/bash\nme:x:1000:1000:Me:/home/me:/usr/bin/zsh\n\
                      other:x:1001:100::/srv/other:/bin/bash\nnobody:x:65534:65534::/:/usr/bin/nologin\n";
        let group = "root:x:0:\nwheel:x:998:me,other\nusers:x:100:\nme:x:1000:\nshared:x:1500:me\n";
        let accounts = parse_accounts(passwd, group);
        assert_eq!(accounts.users.keys().collect::<Vec<_>>(), ["me", "other"]);
        let me = &accounts.users["me"];
        assert_eq!((me.group.as_deref(), me.home.as_deref()), (None, None));
        assert_eq!(me.groups, ["shared", "wheel"]);
        let other = &accounts.users["other"];
        assert_eq!(other.group.as_deref(), Some("users"));
        assert_eq!(other.home.as_deref(), Some("/srv/other"));
        assert_eq!(accounts.groups, BTreeMap::from([("shared".to_string(), 1500)]));
    }

    #[test]
    fn secrets_by_mode_or_name() {
        assert!(looks_secret(
            "/etc/NetworkManager/system-connections/home.nmconnection",
            0o600
        ));
        assert!(looks_secret("/etc/sudoers", 0o440));
        assert!(!looks_secret("/etc/fstab", 0o644));
    }
}

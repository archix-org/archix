//! Reads an existing Arch install into the pieces `archix init` turns into a config.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CStr;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use md5::{Digest, Md5};
use serde::Deserialize;

use crate::pacman::parse_desc;
use crate::repos::{Custom, is_official};

/// Which config files init scans for, never touches, treats as secrets and recommends; kept in capture.json.
#[derive(Debug, Deserialize)]
pub struct Lists {
    /// Directories whose files no package owns are config someone wrote.
    pub scan: Vec<String>,
    /// Files archix or other tools own; an entry ending in `/` covers everything under it.
    pub never: Vec<String>,
    /// Any path containing one of these holds credentials and is never copied.
    pub secret: Vec<String>,
    /// Text that only shows up in credentials; a file containing any of it is treated as a secret too.
    pub secret_content: Vec<String>,
    /// Worth managing by default; an entry ending in `/` covers everything under it.
    pub recommended: Vec<String>,
    pub dotfiles: DotfileLists,
}

/// Names relative to a home; one `*` stands for any run of characters.
#[derive(Debug, Deserialize)]
pub struct DotfileLists {
    /// Ticked by default.
    pub recommended: Vec<String>,
    /// Hold credentials: never offered, only counted.
    pub secret: Vec<String>,
    /// Caches, app state and the like: never offered.
    pub skip: Vec<String>,
    /// Single files their app replaces on save, which turns a live link back into a plain file.
    pub replaced: Vec<String>,
}

fn matches_name(list: &[String], name: &str) -> bool {
    list.iter().any(|entry| match entry.split_once('*') {
        Some((prefix, suffix)) => {
            name.len() >= prefix.len() + suffix.len() && name.starts_with(prefix) && name.ends_with(suffix)
        }
        None => entry == name,
    })
}

/// A home entry init can offer to bring into the config.
#[derive(Debug, Clone, PartialEq)]
pub struct Dotfile {
    pub name: String,
    pub recommended: bool,
    /// Its app replaces the file on save, so a live link wouldn't last.
    pub replaced: bool,
}

pub static LISTS: LazyLock<Lists> =
    LazyLock::new(|| serde_json::from_str(include_str!("capture.json")).expect("capture.json is valid"));

fn listed(list: &[String], path: &str) -> bool {
    list.iter().any(|entry| {
        if entry.ends_with('/') {
            path.starts_with(entry.as_str())
        } else {
            entry == path
        }
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct FoundFile {
    pub path: String,
    pub mode: u32,
    pub owner: String,
    pub group: String,
    /// On the recommended list, so managed unless deselected.
    pub recommended: bool,
    /// Edited from what its package shipped, rather than written from scratch.
    pub edited: bool,
}

/// Config files init can offer, and the secret ones it leaves out without asking.
#[derive(Debug, Default)]
pub struct Found {
    pub files: Vec<FoundFile>,
    pub secrets: Vec<String>,
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

pub fn md5_bytes(bytes: &[u8]) -> String {
    Md5::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
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
    listed(&LISTS.never, path) || skip.contains(path)
}

pub fn is_secret(path: &str) -> bool {
    LISTS.secret.iter().any(|hint| path.contains(hint.as_str()))
}

/// Whether a file, or any file in a directory, contains text that only credentials have; big files and trees are sampled, not read whole.
pub fn holds_secret(path: &Path) -> bool {
    const MAX_FILE: u64 = 1024 * 1024;
    const MAX_FILES: usize = 200;
    let mut stack = vec![path.to_path_buf()];
    let mut seen = 0;
    while let Some(path) = stack.pop() {
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            stack.extend(fs::read_dir(&path).into_iter().flatten().flatten().map(|e| e.path()));
            continue;
        }
        if !meta.is_file() || meta.len() > MAX_FILE {
            continue;
        }
        seen += 1;
        if seen > MAX_FILES {
            break;
        }
        if let Ok(bytes) = fs::read(&path) {
            let text = String::from_utf8_lossy(&bytes);
            if LISTS.secret_content.iter().any(|marker| text.contains(marker.as_str())) {
                return true;
            }
        }
    }
    false
}

fn found(path: &str, edited: bool) -> Result<Option<FoundFile>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => meta,
        _ => return Ok(None),
    };
    Ok(Some(FoundFile {
        path: path.to_string(),
        mode: meta.mode() & 0o7777,
        owner: user_name(meta.uid()).unwrap_or_else(|| meta.uid().to_string()),
        group: group_name(meta.gid()).unwrap_or_else(|| meta.gid().to_string()),
        recommended: listed(&LISTS.recommended, path),
        edited,
    }))
}

/// Backup files edited since their package installed them, plus files nobody owns in the scanned directories.
pub fn config_files(installed: &BTreeMap<String, Installed>, skip: &BTreeSet<String>) -> Result<Found> {
    let mut candidates: BTreeMap<String, bool> = BTreeMap::new();
    for pkg in installed.values() {
        for (path, sum) in &pkg.backups {
            if Path::new(path).is_file() && md5(Path::new(path)).is_ok_and(|now| now != *sum) {
                candidates.insert(path.clone(), true);
            }
        }
    }
    let owned: BTreeSet<String> = installed
        .values()
        .flat_map(|pkg| pkg.files.iter().map(|f| format!("/{f}")))
        .collect();
    for dir in &LISTS.scan {
        let mut stack = vec![Path::new(dir).to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(kind) = entry.file_type() else { continue };
                if kind.is_dir() {
                    stack.push(path);
                } else if kind.is_file() {
                    let text = path.to_string_lossy().into_owned();
                    if !owned.contains(&text) {
                        candidates.entry(text).or_insert(false);
                    }
                }
            }
        }
    }
    let mut out = Found::default();
    for (path, edited) in candidates {
        if excluded(&path, skip) {
            continue;
        }
        if is_secret(&path) || holds_secret(Path::new(&path)) {
            out.secrets.push(path);
        } else if let Some(file) = found(&path, edited)? {
            out.files.push(file);
        }
    }
    Ok(out)
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

/// Where a unit's file lives, per systemd.
pub fn unit_path(unit: &str) -> Option<String> {
    let output = Command::new("systemctl")
        .args(["show", "--property=FragmentPath", "--value", unit])
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !path.is_empty()).then_some(path)
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

/// What in a home is worth offering as dotfiles (~/.config entries and top-level dotfiles), and how many hold secrets.
pub fn dotfiles(home: &Path) -> (Vec<Dotfile>, usize) {
    let lists = &LISTS.dotfiles;
    let mut names = Vec::new();
    for entry in fs::read_dir(home.join(".config")).into_iter().flatten().flatten() {
        names.push(format!(".config/{}", entry.file_name().to_string_lossy()));
    }
    for entry in fs::read_dir(home).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            names.push(name);
        }
    }
    names.sort();
    let mut secrets = 0;
    let mut out = Vec::new();
    for name in names {
        if matches_name(&lists.secret, &name) || is_secret(&name) {
            secrets += 1;
            continue;
        }
        if matches_name(&lists.skip, &name) {
            continue;
        }
        if holds_secret(&home.join(&name)) {
            secrets += 1;
            continue;
        }
        let recommended = matches_name(&lists.recommended, &name);
        let replaced = !home.join(&name).is_dir() && matches_name(&lists.replaced, &name);
        out.push(Dotfile {
            name,
            recommended,
            replaced,
        });
    }
    (out, secrets)
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
    fn lists_parse_and_sort_files() {
        assert!(
            LISTS
                .scan
                .iter()
                .all(|dir| dir.starts_with("/etc/") && !dir.ends_with('/'))
        );
        assert!(
            LISTS
                .never
                .iter()
                .chain(&LISTS.recommended)
                .all(|path| path.starts_with("/etc/"))
        );
        assert!(is_secret("/etc/NetworkManager/system-connections/home.nmconnection"));
        assert!(is_secret("/etc/ssh/ssh_host_ed25519_key"));
        assert!(!is_secret("/etc/sudoers"));
        assert!(listed(&LISTS.recommended, "/etc/fstab"));
        assert!(listed(&LISTS.recommended, "/etc/sysctl.d/99-swappiness.conf"));
        assert!(!listed(&LISTS.recommended, "/etc/shells"));
        assert!(listed(&LISTS.never, "/etc/pacman.d/gnupg/pubring.gpg"));
    }

    #[test]
    fn content_marks_secrets_that_names_dont() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".config/someapp")).unwrap();
        fs::write(
            home.path().join(".config/someapp/settings.json"),
            "{\"theme\": \"dark\"}",
        )
        .unwrap();
        fs::create_dir_all(home.path().join(".config/sometool")).unwrap();
        fs::write(
            home.path().join(".config/sometool/auth.yml"),
            "token: ghp_abcdefghijklmnop\n",
        )
        .unwrap();
        fs::write(home.path().join(".deploy-key"), "-----BEGIN OPENSSH PRIVATE KEY-----\n").unwrap();
        let (offered, secrets) = dotfiles(home.path());
        let names: Vec<&str> = offered.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&".config/someapp"), "{names:?}");
        assert!(
            !names.contains(&".config/sometool") && !names.contains(&".deploy-key"),
            "{names:?}"
        );
        assert_eq!(secrets, 2);
    }

    #[test]
    fn dotfiles_are_recommended_skipped_or_kept_secret() {
        let home = tempfile::tempdir().unwrap();
        for dir in [
            ".config/nvim",
            ".config/gh",
            ".config/chromium",
            ".config/kwinrc.d",
            ".ssh",
            ".cache",
        ] {
            fs::create_dir_all(home.path().join(dir)).unwrap();
        }
        for file in [
            ".pulse-cookie",
            ".steampid",
            ".bashrc",
            ".bash_history",
            ".claude.json",
            ".claude.json.backup",
            ".config/kdeglobals",
            ".config/plasmashellrc",
        ] {
            fs::write(home.path().join(file), "").unwrap();
        }
        let (offered, secrets) = dotfiles(home.path());
        let replaced: Vec<&str> = offered.iter().filter(|d| d.replaced).map(|d| d.name.as_str()).collect();
        assert_eq!(
            replaced,
            [".config/kdeglobals", ".config/plasmashellrc"],
            "single rc files are replaced on save; directories and .bashrc aren't"
        );
        let names: Vec<(&str, bool)> = offered.iter().map(|d| (d.name.as_str(), d.recommended)).collect();
        assert_eq!(
            names,
            [
                (".bashrc", true),
                (".config/kdeglobals", false),
                (".config/kwinrc.d", false),
                (".config/nvim", true),
                (".config/plasmashellrc", false),
            ]
        );
        assert_eq!(
            secrets, 5,
            ".ssh, .config/gh, .claude.json, its backup and .pulse-cookie"
        );
    }
}

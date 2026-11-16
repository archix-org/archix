//! What `archix init` reads from the machine as it stands: explicit packages, repos, services and accounts.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::pacman::parse_desc;
use crate::repos::{Custom, is_official};

#[derive(Debug, Clone, PartialEq)]
pub struct Account {
    pub uid: u32,
    /// Below uid 1000 and made by hand, not by a sysusers.d file.
    pub system: bool,
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
    /// Groups in the regular id range, and system ones no sysusers.d file makes, that aren't some captured user's own group.
    pub groups: BTreeMap<String, u32>,
    /// Which of `groups` are system groups.
    pub system_groups: BTreeSet<String>,
}

/// `path`, absolute on the machine, inside `root`.
fn on_disk(root: &Path, path: &str) -> std::path::PathBuf {
    root.join(path.trim_start_matches('/'))
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

/// The kinds of unit `services` can enable; targets too, such as nfs-client.target.
pub const UNIT_TYPES: &str = "service,socket,timer,path,target,mount,automount,swap";

/// What `systemctl list-unit-files` says init should keep.
#[derive(Debug, Default, PartialEq)]
pub struct UnitStates {
    /// Units whose enabled state differs from their preset.
    pub changed: BTreeMap<String, bool>,
    /// Units masked in /etc (not just for this boot).
    pub masked: Vec<String>,
}

/// Reads `systemctl list-unit-files --no-legend`.
pub fn parse_unit_files(text: &str) -> UnitStates {
    let mut out = UnitStates::default();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [unit, state, preset, ..] = fields[..] else {
            continue;
        };
        if unit.contains("@.") {
            continue;
        }
        match (state, preset) {
            ("enabled", "disabled") => {
                out.changed.insert(unit.to_string(), true);
            }
            ("disabled", "enabled") => {
                out.changed.insert(unit.to_string(), false);
            }
            ("masked", _) => out.masked.push(unit.to_string()),
            _ => {}
        }
    }
    out
}

pub fn unit_states() -> Result<UnitStates> {
    let output = Command::new("systemctl")
        .args(["list-unit-files", "--no-legend", "--no-pager"])
        .arg(format!("--type={UNIT_TYPES}"))
        .output()
        .context("running systemctl")?;
    Ok(parse_unit_files(&String::from_utf8_lossy(&output.stdout)))
}

/// The users and groups sysusers.d lines (`u`, `g`, `m`) make, which packages bring along.
pub fn parse_sysusers(text: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        match fields[..] {
            [kind, name, ..] if kind.starts_with('u') || kind.starts_with('g') => {
                names.insert(name.to_string());
            }
            [kind, user, group, ..] if kind.starts_with('m') => {
                names.extend([user.to_string(), group.to_string()]);
            }
            _ => {}
        }
    }
    names
}

/// The users and groups every sysusers.d file under `root` makes.
pub fn sysusers(root: &Path) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for dir in ["/usr/lib/sysusers.d", "/etc/sysusers.d", "/run/sysusers.d"] {
        for entry in fs::read_dir(on_disk(root, dir)).into_iter().flatten().flatten() {
            if entry.file_name().to_string_lossy().ends_with(".conf") {
                names.extend(parse_sysusers(&fs::read_to_string(entry.path()).unwrap_or_default()));
            }
        }
    }
    names
}

/// Regular users (uid 1000 to 59999), system ones (uid 1 to 999) that `sysusers` doesn't make, and the groups of either kind that aren't one of their own groups.
pub fn parse_accounts(passwd: &str, group: &str, sysusers: &BTreeSet<String>) -> Accounts {
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
    let hand_made = |id: u32, name: &str| (1..1000).contains(&id) && !sysusers.contains(name);
    let mut accounts = Accounts::default();
    for line in passwd.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        let [name, _, uid, gid, gecos, home, shell] = fields[..] else {
            continue;
        };
        let (Ok(uid), Ok(gid)) = (uid.parse::<u32>(), gid.parse::<u32>()) else {
            continue;
        };
        let system = hand_made(uid, name);
        if !regular(uid) && !system {
            continue;
        }
        let primary = groups.iter().find(|(_, id, _)| *id == gid).map(|(n, _, _)| n.clone());
        if system && primary.is_none() {
            continue;
        }
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
                system,
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
        if own {
            continue;
        }
        if regular(*gid) {
            accounts.groups.insert(name.clone(), *gid);
        } else if hand_made(*gid, name) {
            accounts.groups.insert(name.clone(), *gid);
            accounts.system_groups.insert(name.clone());
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

/// Each installed package's build date, from pacman's local database.
pub fn build_dates(local_db: &Path) -> Result<BTreeMap<String, u64>> {
    let mut out = BTreeMap::new();
    for entry in fs::read_dir(local_db).with_context(|| format!("reading {}", local_db.display()))? {
        let dir = entry.with_context(|| format!("reading {}", local_db.display()))?.path();
        let Ok(desc) = fs::read_to_string(dir.join("desc")) else {
            continue;
        };
        let desc = parse_desc(&desc);
        let name = desc.get("NAME").and_then(|v| v.first());
        let date = desc.get("BUILDDATE").and_then(|v| v.first()).and_then(|d| d.parse().ok());
        if let (Some(name), Some(date)) = (name, date) {
            out.insert(name.clone(), date);
        }
    }
    Ok(out)
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
            parse_unit_files(text).changed,
            BTreeMap::from([
                ("sshd.service".to_string(), true),
                ("systemd-networkd.service".to_string(), false)
            ])
        );
    }

    #[test]
    fn targets_count_as_services_and_masks_are_kept() {
        assert!(UNIT_TYPES.split(',').any(|kind| kind == "target"), "{UNIT_TYPES}");
        let states = parse_unit_files(
            "nfs-client.target enabled disabled\nsleep.target masked disabled\nfoo.service masked-runtime disabled\n",
        );
        assert_eq!(
            states.changed,
            BTreeMap::from([("nfs-client.target".to_string(), true)])
        );
        assert_eq!(states.masked, ["sleep.target"]);
    }

    #[test]
    fn accounts_skip_system_users_and_private_groups() {
        let passwd = "root:x:0:0::/root:/bin/bash\nme:x:1000:1000:Me:/home/me:/usr/bin/zsh\n\
                      other:x:1001:100::/srv/other:/bin/bash\nnobody:x:65534:65534::/:/usr/bin/nologin\n";
        let group = "root:x:0:\nwheel:x:998:me,other\nusers:x:100:\nme:x:1000:\nshared:x:1500:me\n";
        let accounts = parse_accounts(
            passwd,
            group,
            &BTreeSet::from(["wheel".to_string(), "users".to_string()]),
        );
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
    fn system_users_and_groups_made_by_hand_are_kept() {
        let passwd = "root:x:0:0::/root:/bin/bash\npostgres:x:975:975::/var/lib/postgres:/usr/bin/bash\n\
                      scanner:x:950:950::/opt/scanner:/bin/false\nmetrics:x:220:220::/:/usr/bin/nologin\n\
                      me:x:1000:1000::/home/me:/bin/bash\n";
        let group =
            "root:x:0:\npostgres:x:975:\nscanner:x:950:\nmetrics:x:220:\ni2c:x:961:me\ndocker:x:972:me\nme:x:1000:\n";
        let sysusers = parse_sysusers(
            "# made by packages\nu postgres 975 - /var/lib/postgres\ng docker 972\nu! root 0\nm me wheel\n",
        );
        let accounts = parse_accounts(passwd, group, &sysusers);
        assert_eq!(accounts.users.keys().collect::<Vec<_>>(), ["me", "metrics", "scanner"]);
        let scanner = &accounts.users["scanner"];
        assert!(scanner.system && scanner.uid == 950 && scanner.group.is_none());
        assert_eq!(
            (scanner.home.as_deref(), scanner.shell.as_str()),
            (Some("/opt/scanner"), "/bin/false")
        );
        assert!(!accounts.users["me"].system);
        assert_eq!(accounts.users["me"].groups, ["docker", "i2c"]);
        assert_eq!(accounts.groups, BTreeMap::from([("i2c".to_string(), 961)]));
        assert_eq!(accounts.system_groups, BTreeSet::from(["i2c".to_string()]));
    }

    #[test]
    fn build_dates_come_from_the_local_database() {
        let db = tempfile::tempdir().unwrap();
        for (dir, desc) in [
            ("bash-5.2-1", "%NAME%\nbash\n\n%BUILDDATE%\n1700000000\n"),
            ("odd-1-1", "%NAME%\nodd\n"),
        ] {
            fs::create_dir_all(db.path().join(dir)).unwrap();
            fs::write(db.path().join(dir).join("desc"), desc).unwrap();
        }
        assert_eq!(
            build_dates(db.path()).unwrap(),
            BTreeMap::from([("bash".to_string(), 1_700_000_000)])
        );
    }
}

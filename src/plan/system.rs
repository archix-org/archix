use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use super::model::{AccountAction, FileAction, FileItem, SecretStatus, ServiceAction, ServiceItem, UserItem};
use super::risk;
use super::udiff;
use crate::fetch::sha256_bytes;
use crate::secrets::{self, Manifest};
use crate::system::apply::Accounts;
use crate::system::{ManagedFile, SystemState, User};

/// Units a switch never restarts live (SCOPE 3.1); sshd only over ssh.
pub fn at_reboot(unit: &str, ssh_session: bool, display_manager: Option<&str>) -> bool {
    let base = unit.trim_end_matches(".service");
    matches!(
        base,
        "dbus"
            | "dbus-broker"
            | "systemd-logind"
            | "NetworkManager"
            | "systemd-networkd"
            | "user@"
            | "systemd-journald"
    ) || base.starts_with("user@")
        || (ssh_session && base == "sshd")
        || display_manager.is_some_and(|dm| dm.trim_end_matches(".service") == base)
        || matches!(
            base,
            "display-manager" | "gdm" | "sddm" | "lightdm" | "ly" | "greetd" | "lxdm"
        )
}

pub struct SystemInputs<'a> {
    pub root: &'a Path,
    pub to: &'a SystemState,
    pub blobs: &'a BTreeMap<String, Vec<u8>>,
    pub from: Option<&'a SystemState>,
    /// Whether a unit is enabled now (system, or global user when the flag is set); None when that can't be told.
    pub enabled: &'a dyn Fn(&str, bool) -> Option<bool>,
    /// HMACs of the new secret values under the machine key; None when the key can't be read (no root).
    pub secret_hmacs: Option<&'a BTreeMap<String, String>>,
    pub last_hmacs: &'a BTreeMap<String, String>,
    pub ssh_session: bool,
    pub display_manager: Option<&'a str>,
    /// `visudo -cf` on a candidate sudoers text: Some(false) when it fails, None when it can't run.
    pub visudo: &'a dyn Fn(&[u8]) -> Option<bool>,
}

#[derive(Debug, Default)]
pub struct SystemGroup {
    pub files: Vec<FileItem>,
    pub services: Vec<ServiceItem>,
    pub users: Vec<UserItem>,
    pub refusals: Vec<String>,
    pub unchecked: Vec<String>,
}

fn secret_status(name: &str, now: Option<&BTreeMap<String, String>>, last: &BTreeMap<String, String>) -> SecretStatus {
    match now.and_then(|now| now.get(name)) {
        None => SecretStatus::NotChecked,
        Some(new) if last.get(name) == Some(new) => SecretStatus::Unchanged,
        Some(_) => SecretStatus::Changed,
    }
}

fn combine(statuses: impl IntoIterator<Item = SecretStatus>) -> SecretStatus {
    let all: Vec<SecretStatus> = statuses.into_iter().collect();
    if all.contains(&SecretStatus::NotChecked) {
        SecretStatus::NotChecked
    } else if all.contains(&SecretStatus::Changed) {
        SecretStatus::Changed
    } else {
        SecretStatus::Unchanged
    }
}

/// The secrets whose values a managed path's content comes from; None when it isn't secret-derived.
fn secret_names(path: &str, file: &ManagedFile, manifest: Option<&Manifest>) -> Option<Vec<String>> {
    let all = || {
        manifest
            .map(|m| m.secrets.keys().cloned().collect())
            .unwrap_or_default()
    };
    if path.starts_with(&format!("{}/", secrets::DEPLOY_DIR)) {
        return Some(all());
    }
    let target = file.link.as_deref().filter(|t| secrets::is_secret_link(t))?;
    let rest = target
        .strip_prefix(&format!("{}/", secrets::RUN_DIR))
        .unwrap_or_default();
    match rest.strip_prefix("files/") {
        Some(id) => Some(
            manifest
                .and_then(|m| m.templates.get(id))
                .map(|t| t.secrets.clone())
                .unwrap_or_else(all),
        ),
        None => Some(vec![rest.to_string()]),
    }
}

enum Disk {
    Missing,
    Unreadable,
    Link(String),
    File {
        content: Vec<u8>,
        mode: u32,
        uid: u32,
        gid: u32,
    },
    Other,
}

fn read_disk(path: &Path) -> Disk {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return Disk::Missing;
    };
    if meta.file_type().is_symlink() {
        return fs::read_link(path).map_or(Disk::Unreadable, |t| Disk::Link(t.to_string_lossy().into_owned()));
    }
    if !meta.is_file() {
        return Disk::Other;
    }
    match fs::read(path) {
        Ok(content) => Disk::File {
            content,
            mode: meta.mode() & 0o7777,
            uid: meta.uid(),
            gid: meta.gid(),
        },
        Err(_) => Disk::Unreadable,
    }
}

fn item(action: FileAction, path: &str, reason: impl Into<String>) -> FileItem {
    FileItem {
        action,
        path: path.to_string(),
        reason: reason.into(),
        diff: None,
        secret: None,
        hand_edited: false,
        risk: risk::file_risks(path),
    }
}

fn files(inputs: &SystemInputs, accounts: &Accounts, group: &mut SystemGroup) {
    let manifest = inputs.to.secrets.as_ref();
    let from_files = inputs.from.map(|f| &f.files);
    for (path, want) in &inputs.to.files {
        let disk = read_disk(&inputs.root.join(path.trim_start_matches('/')));
        let previous = from_files.and_then(|f| f.get(path));
        if let Some(names) = secret_names(path, want, manifest) {
            let status = combine(
                names
                    .iter()
                    .map(|n| secret_status(n, inputs.secret_hmacs, inputs.last_hmacs)),
            );
            let in_place = match (&disk, &want.link) {
                (Disk::Link(target), Some(link)) => target == link,
                (Disk::File { content, .. }, None) => sha256_bytes(content) == want.sha256,
                _ => false,
            };
            let action = match (&disk, in_place) {
                (Disk::Missing, _) => Some(FileAction::Create),
                (_, false) => Some(FileAction::Modify),
                (_, true) if status != SecretStatus::Unchanged => Some(FileAction::Modify),
                _ => None,
            };
            if let Some(action) = action {
                let reason = match status {
                    SecretStatus::Changed => "secret changed",
                    SecretStatus::Unchanged => "secret unchanged",
                    SecretStatus::NotChecked => "secret: not checked without root",
                };
                group.files.push(FileItem {
                    secret: Some(status),
                    ..item(action, path, reason)
                });
            }
            continue;
        }
        if let Some(merge) = &want.merge {
            let current = match &disk {
                Disk::File { content, .. } => Some(String::from_utf8_lossy(content).into_owned()),
                Disk::Missing => Some(String::new()),
                _ => None,
            };
            match current {
                Some(text) if merge.satisfied(&text) => {}
                Some(text) => {
                    let mut it = item(FileAction::Modify, path, "declared keys");
                    it.diff = merge
                        .apply(&text)
                        .ok()
                        .and_then(|new| udiff::unified(text.as_bytes(), new.as_bytes(), path));
                    group.files.push(it);
                }
                None => group.unchecked.push(format!("file {path}: not readable without root")),
            }
            continue;
        }
        if let Some(link) = &want.link {
            match &disk {
                Disk::Link(target) if target == link => {}
                Disk::Missing => group
                    .files
                    .push(item(FileAction::Create, path, format!("declared, link to {link}"))),
                _ => group
                    .files
                    .push(item(FileAction::Modify, path, format!("declared, link to {link}"))),
            }
            continue;
        }
        let new = inputs.blobs.get(&want.sha256);
        match &disk {
            Disk::Missing => {
                let mut it = item(FileAction::Create, path, "declared");
                it.diff = match new {
                    Some(new) => udiff::unified(b"", new, path),
                    None => Some("content downloaded at switch\n".into()),
                };
                group.files.push(it);
            }
            Disk::Unreadable => group.unchecked.push(format!("file {path}: not readable without root")),
            Disk::Link(_) | Disk::Other => {
                group
                    .files
                    .push(item(FileAction::Modify, path, "declared, replaces what's there"))
            }
            Disk::File {
                content,
                mode,
                uid,
                gid,
            } => {
                let sha = sha256_bytes(content);
                let hand_edited = previous.is_some_and(|p| p.link.is_none() && p.merge.is_none() && p.sha256 != sha);
                if sha != want.sha256 {
                    let mut it = item(
                        FileAction::Modify,
                        path,
                        if hand_edited {
                            "declared; the hand edit goes to .archix-save"
                        } else {
                            "declared"
                        },
                    );
                    it.hand_edited = hand_edited;
                    it.diff = match new {
                        Some(new) => udiff::unified(content, new, path),
                        None => Some("content downloaded at switch\n".into()),
                    };
                    group.files.push(it);
                } else if *mode != want.mode {
                    group.files.push(item(
                        FileAction::Mode,
                        path,
                        format!("mode {:04o} -> {:04o}", mode, want.mode),
                    ));
                } else if Some(*uid) != accounts.uid(&want.owner) || Some(*gid) != accounts.gid(&want.group) {
                    group.files.push(item(
                        FileAction::Owner,
                        path,
                        format!("owner {}:{}", want.owner, want.group),
                    ));
                }
            }
        }
    }
    for (path, prev) in from_files.into_iter().flatten() {
        if inputs.to.files.contains_key(path) {
            continue;
        }
        let disk = read_disk(&inputs.root.join(path.trim_start_matches('/')));
        let untouched = match (&disk, &prev.link) {
            (Disk::Link(t), Some(link)) => t == link,
            (Disk::File { content, .. }, None) => prev.merge.is_none() && sha256_bytes(content) == prev.sha256,
            _ => false,
        };
        match (&disk, untouched) {
            (Disk::Missing, _) => {}
            (_, true) => group.files.push(item(FileAction::Delete, path, "no longer declared")),
            (_, false) => group.files.push(item(
                FileAction::Unmanage,
                path,
                "no longer declared; edited by hand, so left in place",
            )),
        }
    }
    for it in group
        .files
        .iter()
        .filter(|it| risk::file_applies(risk::FileRule::Sudoers, &it.path))
    {
        let Some(want) = inputs.to.files.get(&it.path) else {
            continue;
        };
        let Some(content) = inputs.blobs.get(&want.sha256) else {
            continue;
        };
        match (inputs.visudo)(content) {
            Some(true) => {}
            Some(false) => group.refusals.push(format!("{} fails visudo -cf", it.path)),
            None => group.unchecked.push(format!("{}: visudo -cf could not run", it.path)),
        }
    }
    group.files.sort_by(|a, b| a.path.cmp(&b.path));
}

fn services(inputs: &SystemInputs, group: &mut SystemGroup) {
    let from = inputs.from;
    for (user, wanted, before) in [
        (false, &inputs.to.services, from.map(|f| &f.services)),
        (true, &inputs.to.user_services, from.map(|f| &f.user_services)),
    ] {
        let mut push = |unit: &str, enable: bool, reason: &str| {
            let reboot = at_reboot(unit, inputs.ssh_session, inputs.display_manager);
            group.services.push(ServiceItem {
                action: if enable {
                    ServiceAction::Enable
                } else {
                    ServiceAction::Disable
                },
                unit: unit.to_string(),
                user,
                at_reboot: reboot,
                reason: reason.to_string(),
                risk: if reboot {
                    vec!["applies at reboot".into()]
                } else {
                    Vec::new()
                },
            });
        };
        for (unit, enable) in wanted {
            match (inputs.enabled)(unit, user) {
                Some(now) if now == *enable => {}
                Some(_) => push(unit, *enable, "declared"),
                None => group.unchecked.push(format!("unit {unit}: enablement unknown")),
            }
        }
        for (unit, was) in before.into_iter().flatten() {
            if *was && !wanted.contains_key(unit) && (inputs.enabled)(unit, user) == Some(true) {
                push(unit, false, "no longer declared");
            }
        }
    }
}

fn user_changes(old: &User, new: &User) -> Vec<String> {
    let mut out = Vec::new();
    if old.uid != new.uid {
        out.push("uid".into());
    }
    if old.group != new.group {
        out.push("primary group".into());
    }
    if old.groups != new.groups {
        out.push(format!("groups {}", new.groups.join(",")));
    }
    if old.shell != new.shell {
        out.push(format!("login shell {} -> {}", old.shell, new.shell));
    }
    if old.home != new.home {
        out.push("home".into());
    }
    if old.description != new.description {
        out.push("description".into());
    }
    if old.hashed_password != new.hashed_password {
        out.push("password".into());
    }
    out
}

fn users(inputs: &SystemInputs, accounts: &Accounts, group: &mut SystemGroup) {
    let from_users = inputs.from.map(|f| &f.users);
    for (name, user) in &inputs.to.users {
        let secret = user
            .hashed_password_secret
            .as_ref()
            .map(|s| secret_status(s, inputs.secret_hmacs, inputs.last_hmacs));
        let previous = from_users.and_then(|u| u.get(name));
        let (action, mut changes) = match (accounts.uid(name), previous) {
            (None, _) => (Some(AccountAction::AddUser), Vec::new()),
            (Some(_), Some(prev)) => {
                let changes = user_changes(prev, user);
                ((!changes.is_empty()).then_some(AccountAction::ModifyUser), changes)
            }
            (Some(_), None) => (None, Vec::new()),
        };
        let secret_changed = secret.is_some_and(|s| s != SecretStatus::Unchanged);
        let Some(action) = action.or(secret_changed.then_some(AccountAction::ModifyUser)) else {
            continue;
        };
        if secret_changed {
            changes.push("password from a secret".into());
        }
        let shell_change = changes.iter().any(|c| c.starts_with("login shell"));
        group.users.push(UserItem {
            action,
            name: name.clone(),
            changes,
            reason: "declared".into(),
            secret,
            risk: if shell_change {
                vec!["login shell change".into()]
            } else {
                Vec::new()
            },
        });
    }
    for name in from_users.into_iter().flatten().map(|(n, _)| n) {
        if !inputs.to.users.contains_key(name) && accounts.uid(name).is_some() && name != crate::aur::build::BUILD_USER
        {
            group.users.push(UserItem {
                action: AccountAction::RemoveUser,
                name: name.clone(),
                changes: vec!["home kept".into()],
                reason: "no longer declared".into(),
                secret: None,
                risk: vec!["user removal".into()],
            });
        }
    }
    let from_groups = inputs.from.map(|f| &f.groups);
    for (name, want) in &inputs.to.groups {
        if accounts.gid(name).is_none() {
            group
                .users
                .push(account_line(AccountAction::AddGroup, name, "declared"));
        } else if want.gid.is_some_and(|id| Some(id) != accounts.gid(name)) {
            group
                .users
                .push(account_line(AccountAction::ModifyGroup, name, "declared gid"));
        }
    }
    for name in from_groups.into_iter().flatten().map(|(n, _)| n) {
        if !inputs.to.groups.contains_key(name) && accounts.gid(name).is_some() {
            group
                .users
                .push(account_line(AccountAction::RemoveGroup, name, "no longer declared"));
        }
    }
}

fn account_line(action: AccountAction, name: &str, reason: &str) -> UserItem {
    UserItem {
        action,
        name: name.to_string(),
        changes: Vec::new(),
        reason: reason.into(),
        secret: None,
        risk: Vec::new(),
    }
}

/// The files, services, and users and groups groups, from the rendered state against the system at `root`.
pub fn build(inputs: &SystemInputs) -> SystemGroup {
    let accounts = Accounts::read(&inputs.root.join("etc"));
    let mut group = SystemGroup::default();
    files(inputs, &accounts, &mut group);
    services(inputs, &mut group);
    users(inputs, &accounts, &mut group);
    let unchecked_secrets: BTreeSet<&str> = group
        .files
        .iter()
        .filter(|f| f.secret == Some(SecretStatus::NotChecked))
        .map(|f| f.path.as_str())
        .chain(
            group
                .users
                .iter()
                .filter(|u| u.secret == Some(SecretStatus::NotChecked))
                .map(|u| u.name.as_str()),
        )
        .collect();
    if !unchecked_secrets.is_empty() {
        group.unchecked.push(format!(
            "secret: not checked without root ({})",
            unchecked_secrets.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    group
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::Group;

    fn file(content: &[u8], blobs: &mut BTreeMap<String, Vec<u8>>) -> ManagedFile {
        let sha = sha256_bytes(content);
        blobs.insert(sha.clone(), content.to_vec());
        ManagedFile {
            sha256: sha,
            link: None,
            mode: 0o644,
            owner: "root".into(),
            group: "root".into(),
            merge: None,
        }
    }

    fn link(target: &str) -> ManagedFile {
        ManagedFile {
            sha256: String::new(),
            link: Some(target.into()),
            mode: 0o777,
            owner: "root".into(),
            group: "root".into(),
            merge: None,
        }
    }

    struct Fixture {
        dir: tempfile::TempDir,
        blobs: BTreeMap<String, Vec<u8>>,
        to: SystemState,
    }

    impl Fixture {
        fn new() -> Fixture {
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir_all(dir.path().join("etc")).unwrap();
            let me = unsafe { libc::geteuid() };
            fs::write(
                dir.path().join("etc/passwd"),
                format!("root:x:{me}:{me}::/root:/bin/bash\nalice:x:1000:1000::/home/alice:/bin/bash\n"),
            )
            .unwrap();
            fs::write(
                dir.path().join("etc/group"),
                format!("root:x:{me}:\nwheel:x:998:alice\n"),
            )
            .unwrap();
            Fixture {
                dir,
                blobs: BTreeMap::new(),
                to: SystemState::default(),
            }
        }

        fn write(&self, path: &str, content: &str) {
            let full = self.dir.path().join(path.trim_start_matches('/'));
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(&full, content).unwrap();
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&full, fs::Permissions::from_mode(0o644)).unwrap();
        }

        fn declare(&mut self, path: &str, content: &str) {
            let f = file(content.as_bytes(), &mut self.blobs);
            self.to.files.insert(path.into(), f);
        }

        fn run(
            &self,
            from: Option<&SystemState>,
            hmacs: Option<&BTreeMap<String, String>>,
            last: &BTreeMap<String, String>,
        ) -> SystemGroup {
            build(&SystemInputs {
                root: self.dir.path(),
                to: &self.to,
                blobs: &self.blobs,
                from,
                enabled: &|unit, _| Some(unit == "sshd.service"),
                secret_hmacs: hmacs,
                last_hmacs: last,
                ssh_session: true,
                display_manager: None,
                visudo: &|content| Some(!content.starts_with(b"broken")),
            })
        }
    }

    #[test]
    fn files_create_modify_with_diff_and_hand_edits() {
        let mut fx = Fixture::new();
        fx.declare("/etc/new.conf", "a\n");
        fx.declare("/etc/hosts", "127.0.0.1 localhost\n10.0.0.1 nas\n");
        fx.write("/etc/hosts", "127.0.0.1 localhost\n");
        fx.declare("/etc/same", "x\n");
        fx.write("/etc/same", "x\n");
        let mut from = SystemState::default();
        let mut old_blobs = BTreeMap::new();
        from.files.insert(
            "/etc/hosts".into(),
            file(b"127.0.0.1 localhost\n10.0.0.2 old\n", &mut old_blobs),
        );
        from.files.insert("/etc/gone".into(), file(b"bye\n", &mut old_blobs));
        fx.write("/etc/gone", "bye\n");
        let g = fx.run(Some(&from), None, &BTreeMap::new());
        let paths: Vec<_> = g.files.iter().map(|f| (f.path.as_str(), f.action)).collect();
        assert_eq!(
            paths,
            [
                ("/etc/gone", FileAction::Delete),
                ("/etc/hosts", FileAction::Modify),
                ("/etc/new.conf", FileAction::Create)
            ]
        );
        let hosts = &g.files[1];
        assert!(hosts.hand_edited);
        assert!(hosts.diff.as_ref().unwrap().contains("+10.0.0.1 nas"));
        assert!(!hosts.risk.is_empty(), "/etc/hosts is network config");
        assert!(g.unchecked.is_empty());
    }

    #[test]
    fn mode_changes_are_their_own_action() {
        let mut fx = Fixture::new();
        fx.declare("/etc/x", "x\n");
        fx.to.files.get_mut("/etc/x").unwrap().mode = 0o600;
        fx.write("/etc/x", "x\n");
        let g = fx.run(None, None, &BTreeMap::new());
        assert_eq!(g.files[0].action, FileAction::Mode);
    }

    #[test]
    fn secret_files_never_show_content_and_need_the_key() {
        let mut fx = Fixture::new();
        fx.to
            .files
            .insert("/etc/wifi.psk".into(), link(&secrets::link_target("wifi")));
        fx.write("/run/archix/secrets/placeholder", "");
        let last = BTreeMap::from([("wifi".to_string(), "h1".to_string())]);
        let unchecked = fx.run(None, None, &last);
        let item = &unchecked.files[0];
        assert_eq!(item.secret, Some(SecretStatus::NotChecked));
        assert!(item.diff.is_none());
        assert!(
            unchecked
                .unchecked
                .iter()
                .any(|u| u.starts_with("secret: not checked without root"))
        );

        std::os::unix::fs::symlink(secrets::link_target("wifi"), fx.dir.path().join("etc/wifi.psk")).unwrap();
        let same = BTreeMap::from([("wifi".to_string(), "h1".to_string())]);
        assert!(fx.run(None, Some(&same), &last).files.is_empty());
        let changed = BTreeMap::from([("wifi".to_string(), "h2".to_string())]);
        let g = fx.run(None, Some(&changed), &last);
        assert_eq!(g.files[0].secret, Some(SecretStatus::Changed));
        assert_eq!(g.files[0].reason, "secret changed");
        let json = serde_json::to_string(&g.files).unwrap();
        assert!(!json.contains("h2") && !json.contains("h1"), "{json}");
    }

    #[test]
    fn unreadable_files_are_unchecked() {
        let mut fx = Fixture::new();
        fx.declare("/etc/locked", "x\n");
        fx.write("/etc/locked", "y\n");
        let path = fx.dir.path().join("etc/locked");
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&path).is_ok() {
            return;
        }
        let g = fx.run(None, None, &BTreeMap::new());
        assert_eq!(g.unchecked, ["file /etc/locked: not readable without root"]);
    }

    #[test]
    fn a_failing_sudoers_refuses() {
        let mut fx = Fixture::new();
        fx.declare("/etc/sudoers.d/10-wheel", "broken\n");
        let g = fx.run(None, None, &BTreeMap::new());
        assert_eq!(g.refusals, ["/etc/sudoers.d/10-wheel fails visudo -cf"]);
    }

    #[test]
    fn services_and_reboot_only_units() {
        let mut fx = Fixture::new();
        fx.to.services.insert("sshd.service".into(), false);
        fx.to.services.insert("cups.service".into(), true);
        fx.to.services.insert("NetworkManager.service".into(), true);
        let g = fx.run(None, None, &BTreeMap::new());
        let lines: Vec<_> = g
            .services
            .iter()
            .map(|s| (s.unit.as_str(), s.action, s.at_reboot))
            .collect();
        assert_eq!(
            lines,
            [
                ("NetworkManager.service", ServiceAction::Enable, true),
                ("cups.service", ServiceAction::Enable, false),
                ("sshd.service", ServiceAction::Disable, true),
            ]
        );
    }

    fn user(shell: &str) -> User {
        User {
            uid: Some(1000),
            group: None,
            groups: vec!["wheel".into()],
            shell: shell.into(),
            home: None,
            description: String::new(),
            system: false,
            hashed_password: Some("$6$secret-hash".into()),
            initial_hashed_password: None,
            hashed_password_secret: None,
        }
    }

    #[test]
    fn users_add_modify_remove_without_hashes() {
        let mut fx = Fixture::new();
        fx.to.users.insert(
            "alice".into(),
            User {
                hashed_password: Some("$6$new-hash".into()),
                ..user("/usr/bin/zsh")
            },
        );
        fx.to.users.insert("bob".into(), user("/bin/bash"));
        fx.to.groups.insert(
            "media".into(),
            Group {
                gid: None,
                system: false,
            },
        );
        let mut from = SystemState::default();
        from.users.insert("alice".into(), user("/bin/bash"));
        from.users.insert("root".into(), user("/bin/bash"));
        from.users
            .insert(crate::aur::build::BUILD_USER.into(), user("/usr/bin/nologin"));
        let g = fx.run(Some(&from), None, &BTreeMap::new());
        assert!(
            !g.users.iter().any(|u| u.name == crate::aur::build::BUILD_USER),
            "archix's build user belongs to its package's sysusers.d file, never removed as undeclared"
        );
        let alice = g.users.iter().find(|u| u.name == "alice").unwrap();
        assert_eq!(alice.action, AccountAction::ModifyUser);
        assert!(!alice.risk.is_empty());
        assert!(
            g.users
                .iter()
                .any(|u| u.name == "bob" && u.action == AccountAction::AddUser)
        );
        let root = g.users.iter().find(|u| u.name == "root").unwrap();
        assert_eq!(root.action, AccountAction::RemoveUser);
        assert!(!root.risk.is_empty());
        assert!(
            g.users
                .iter()
                .any(|u| u.name == "media" && u.action == AccountAction::AddGroup)
        );
        let json = serde_json::to_string(&g.users).unwrap();
        assert!(!json.contains("hash"), "{json}");
    }
}

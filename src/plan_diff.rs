//! What a switch's evaluated plan changes from the current generation's: packages, files, units, users, hooks and mounts, as the prompt shows them; contents, secrets and password hashes never appear in it.
use std::collections::BTreeMap;

use crate::lock::Locked;
use crate::system::{Group, SystemState, User};

fn compare<'a, T: PartialEq>(
    from: &'a BTreeMap<String, T>,
    to: &'a BTreeMap<String, T>,
    mut added: impl FnMut(&str, &T),
    mut changed: impl FnMut(&str, &T, &T),
    mut removed: impl FnMut(&str),
) {
    for (name, want) in to {
        match from.get(name) {
            None => added(name, want),
            Some(have) if have != want => changed(name, have, want),
            Some(_) => {}
        }
    }
    for name in from.keys().filter(|name| !to.contains_key(*name)) {
        removed(name);
    }
}

fn user_fields(user: &User) -> String {
    let mut fields = Vec::new();
    if let Some(uid) = user.uid {
        fields.push(format!("uid {uid}"));
    }
    if !user.groups.is_empty() {
        fields.push(format!("groups {}", user.groups.join(",")));
    }
    if !user.shell.is_empty() {
        fields.push(format!("shell {}", user.shell));
    }
    if user.hashed_password.is_some() || user.hashed_password_secret.is_some() {
        fields.push("password set".to_string());
    }
    fields.join(", ")
}

fn user_changes(have: &User, want: &User) -> String {
    let mut fields = Vec::new();
    let mut note = |same: bool, what: &str| {
        if !same {
            fields.push(what.to_string());
        }
    };
    note(have.uid == want.uid, "uid");
    note(have.group == want.group, "group");
    note(have.groups == want.groups, "groups");
    note(have.shell == want.shell, "shell");
    note(have.home == want.home, "home");
    note(have.description == want.description, "description");
    note(
        have.hashed_password == want.hashed_password
            && have.initial_hashed_password == want.initial_hashed_password
            && have.hashed_password_secret == want.hashed_password_secret,
        "password",
    );
    fields.join(", ")
}

fn file_kind(file: &crate::system::ManagedFile) -> String {
    match &file.link {
        Some(target) => format!("-> {target}"),
        None => format!("{:04o} {}:{}", file.mode, file.owner, file.group),
    }
}

/// One line per change, ordered packages, files, units, users, groups, hooks, mounts, capabilities, secrets; each passes through the hash redaction too, as a description or a link could hold anything.
pub fn lines(
    from_packages: &BTreeMap<String, Locked>,
    from: &SystemState,
    to_packages: &BTreeMap<String, Locked>,
    to: &SystemState,
) -> Vec<String> {
    let mut out = Vec::new();
    let out_cell = std::cell::RefCell::new(&mut out);
    let say = |line: String| out_cell.borrow_mut().push(line);
    let versions = |packages: &BTreeMap<String, Locked>| -> BTreeMap<String, String> {
        packages
            .iter()
            .map(|(name, pkg)| (name.clone(), pkg.version.clone()))
            .collect()
    };
    compare(
        &versions(from_packages),
        &versions(to_packages),
        |name, version| say(format!("package + {name} {version}")),
        |name, have, want| say(format!("package ~ {name} {have} -> {want}")),
        |name| say(format!("package - {name}")),
    );
    let declared_file = |path: &&String| !path.starts_with(crate::secrets::DEPLOY_DIR);
    let only_declared =
        |files: &BTreeMap<String, crate::system::ManagedFile>| -> BTreeMap<String, crate::system::ManagedFile> {
            files
                .iter()
                .filter(|(path, _)| declared_file(path))
                .map(|(path, file)| (path.clone(), file.clone()))
                .collect()
        };
    compare(
        &only_declared(&from.files),
        &only_declared(&to.files),
        |path, file| say(format!("file + {path} ({})", file_kind(file))),
        |path, _, file| say(format!("file ~ {path} ({})", file_kind(file))),
        |path| say(format!("file - {path}")),
    );
    for (what, from_units, to_units) in [
        ("unit", &from.services, &to.services),
        ("user unit", &from.user_services, &to.user_services),
    ] {
        let state = |enabled: bool| if enabled { "enabled" } else { "disabled" };
        compare(
            from_units,
            to_units,
            |unit, enabled| say(format!("{what} + {unit} {}", state(*enabled))),
            |unit, _, enabled| say(format!("{what} ~ {unit} {}", state(*enabled))),
            |unit| say(format!("{what} - {unit}")),
        );
    }
    compare(
        &from.users,
        &to.users,
        |name, user| say(format!("user + {name} ({})", user_fields(user))),
        |name, have, want| say(format!("user ~ {name}: {}", user_changes(have, want))),
        |name| say(format!("user - {name}")),
    );
    compare(
        &from.groups,
        &to.groups,
        |name, group: &Group| {
            say(match group.gid {
                Some(gid) => format!("group + {name} (gid {gid})"),
                None => format!("group + {name}"),
            })
        },
        |name, _, _| say(format!("group ~ {name}")),
        |name| say(format!("group - {name}")),
    );
    compare(
        &from.hooks,
        &to.hooks,
        |name, _| say(format!("hook + {name}")),
        |name, _, _| say(format!("hook ~ {name}")),
        |name| say(format!("hook - {name}")),
    );
    compare(
        &from.on_change,
        &to.on_change,
        |path, _| say(format!("on_change + {path}")),
        |path, _, _| say(format!("on_change ~ {path}")),
        |path| say(format!("on_change - {path}")),
    );
    let mounts = |state: &SystemState| -> BTreeMap<String, String> {
        state.fstab.iter().map(|line| (line.line(), String::new())).collect()
    };
    compare(
        &mounts(from),
        &mounts(to),
        |line, _| say(format!("mount + {line}")),
        |_, _, _| {},
        |line| say(format!("mount - {line}")),
    );
    compare(
        &from.capabilities,
        &to.capabilities,
        |path, caps| say(format!("capability + {path} {caps}")),
        |path, _, caps| say(format!("capability ~ {path} {caps}")),
        |path| say(format!("capability - {path}")),
    );
    let secrets = |state: &SystemState| -> BTreeMap<String, String> {
        state
            .secrets
            .iter()
            .flat_map(|manifest| manifest.secrets.iter())
            .map(|(name, entry)| (name.clone(), entry.sha256.clone()))
            .collect()
    };
    compare(
        &secrets(from),
        &secrets(to),
        |name, _| say(format!("secret + {name}")),
        |name, _, _| say(format!("secret ~ {name}")),
        |name| say(format!("secret - {name}")),
    );
    out.iter().map(|line| crate::captured::redact_hashes(line)).collect()
}

/// At most `limit` of `lines`, then a line saying how many more there are.
pub fn capped(lines: &[String], limit: usize) -> Vec<String> {
    let mut shown: Vec<String> = lines.iter().take(limit).cloned().collect();
    if lines.len() > limit {
        shown.push(format!("... {} more", lines.len() - limit));
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::ManagedFile;

    fn pkg(version: &str) -> Locked {
        Locked {
            version: version.to_string(),
            ..Default::default()
        }
    }

    fn user(hash: Option<&str>) -> User {
        User {
            uid: Some(1000),
            group: None,
            groups: vec!["wheel".into()],
            shell: "/bin/zsh".into(),
            home: None,
            description: String::new(),
            system: false,
            hashed_password: hash.map(str::to_string),
            initial_hashed_password: None,
            hashed_password_secret: None,
        }
    }

    fn file(sha: &str) -> ManagedFile {
        ManagedFile {
            sha256: sha.into(),
            link: None,
            mode: 0o644,
            owner: "root".into(),
            group: "root".into(),
            merge: None,
        }
    }

    #[test]
    fn the_plan_diff_names_each_kind_of_change_and_never_a_hash() {
        let from_packages = BTreeMap::from([("nano".to_string(), pkg("8-1")), ("bash".to_string(), pkg("5.2-1"))]);
        let to_packages = BTreeMap::from([("vim".to_string(), pkg("9-1")), ("bash".to_string(), pkg("5.3-1"))]);
        let from = SystemState {
            files: BTreeMap::from([
                ("/etc/gone".to_string(), file("aaa")),
                ("/etc/same".to_string(), file("bbb")),
                ("/etc/edit".to_string(), file("ccc")),
            ]),
            services: BTreeMap::from([("off.service".to_string(), true)]),
            users: BTreeMap::from([
                ("bob".to_string(), user(None)),
                ("eve".to_string(), user(Some("$6$old$hashhashhash"))),
            ]),
            ..Default::default()
        };
        let to = SystemState {
            files: BTreeMap::from([
                ("/etc/new".to_string(), file("ddd")),
                ("/etc/same".to_string(), file("bbb")),
                ("/etc/edit".to_string(), file("eee")),
            ]),
            services: BTreeMap::from([("on.service".to_string(), true), ("off.service".to_string(), false)]),
            users: BTreeMap::from([
                ("alice".to_string(), user(Some("$y$j9T$new$secrethash"))),
                ("eve".to_string(), user(Some("$6$new$otherhash"))),
            ]),
            ..Default::default()
        };
        let lines = lines(&from_packages, &from, &to_packages, &to);
        assert_eq!(
            lines,
            [
                "package ~ bash 5.2-1 -> 5.3-1",
                "package + vim 9-1",
                "package - nano",
                "file ~ /etc/edit (0644 root:root)",
                "file + /etc/new (0644 root:root)",
                "file - /etc/gone",
                "unit ~ off.service disabled",
                "unit + on.service enabled",
                "user + alice (uid 1000, groups wheel, shell /bin/zsh, password set)",
                "user ~ eve: password",
                "user - bob",
            ]
        );
    }

    #[test]
    fn the_prompts_plan_is_capped_and_says_how_much_is_left_out() {
        let lines: Vec<String> = (0..5).map(|n| n.to_string()).collect();
        assert_eq!(capped(&lines, 2), ["0", "1", "... 3 more"]);
        assert_eq!(capped(&lines, 5), ["0", "1", "2", "3", "4"]);
    }
}

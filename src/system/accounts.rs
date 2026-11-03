use std::collections::{BTreeMap, BTreeSet};

/// The account files, merged line by line keyed by name rather than as whole files.
pub const FILES: [&str; 4] = ["passwd", "group", "shadow", "gshadow"];

/// The four files' texts in FILES order; None for one that's missing.
pub type Texts = [Option<String>; 4];

#[derive(Debug, Clone, PartialEq)]
enum Line {
    Entry(String, Vec<String>),
    Other(String),
}

fn parse(text: &str) -> Vec<Line> {
    text.lines()
        .map(|line| {
            if line.is_empty() || line.starts_with(['#', '+', '-']) || !line.contains(':') {
                return Line::Other(line.to_string());
            }
            let fields: Vec<String> = line.split(':').map(str::to_string).collect();
            Line::Entry(fields[0].clone(), fields)
        })
        .collect()
}

fn entries(text: &str) -> BTreeMap<String, Vec<String>> {
    parse(text)
        .into_iter()
        .filter_map(|line| match line {
            Line::Entry(name, fields) => Some((name, fields)),
            Line::Other(_) => None,
        })
        .collect()
}

fn render(lines: &[Line]) -> String {
    let mut out = String::new();
    for line in lines {
        match line {
            Line::Entry(_, fields) => out.push_str(&fields.join(":")),
            Line::Other(text) => out.push_str(text),
        }
        out.push('\n');
    }
    out
}

/// Fields holding comma-separated member lists, merged as sets.
fn member_fields(file: &str) -> &'static [usize] {
    match file {
        "group" => &[3],
        "gshadow" => &[2, 3],
        _ => &[],
    }
}

fn members(field: Option<&String>) -> Vec<String> {
    field
        .map(|f| f.split(',').filter(|m| !m.is_empty()).map(str::to_string).collect())
        .unwrap_or_default()
}

/// Theirs' members minus those live removed since base.
fn kept_members(base: Option<&String>, live: Option<&String>, theirs: Option<&String>) -> Vec<String> {
    let (base, live) = (members(base), members(live));
    members(theirs)
        .into_iter()
        .filter(|m| !base.contains(m) || live.contains(m))
        .collect()
}

/// Theirs' members, plus those live added and minus those live removed since base.
fn merge_members(base: Option<&String>, live: Option<&String>, theirs: Option<&String>) -> String {
    let mut out = kept_members(base, live, theirs);
    let base = members(base);
    for m in members(live) {
        if !base.contains(&m) && !out.contains(&m) {
            out.push(m);
        }
    }
    out.join(",")
}

fn union_members(a: Option<&String>, b: Option<&String>) -> String {
    let mut out = members(a);
    for m in members(b) {
        if !out.contains(&m) {
            out.push(m);
        }
    }
    out.join(",")
}

/// One entry three ways: each field live changed since base comes from live, the rest from theirs; with no base entry, live's line wins whole.
fn merge_fields(file: &str, base: Option<&Vec<String>>, live: &[String], theirs: &[String]) -> Vec<String> {
    let sets = member_fields(file);
    let len = live.len().max(theirs.len());
    (0..len)
        .map(|i| {
            if sets.contains(&i) {
                return match base {
                    Some(base) => merge_members(base.get(i), live.get(i), theirs.get(i)),
                    None => union_members(live.get(i), theirs.get(i)),
                };
            }
            match base {
                Some(base) if live.get(i) == base.get(i) => theirs.get(i).cloned().unwrap_or_default(),
                _ => live.get(i).cloned().unwrap_or_default(),
            }
        })
        .collect()
}

/// Merges one account file: live's changes since `base` go over `theirs` entry by entry, and entries live added are appended.
pub fn merge3(file: &str, base: &str, live: &str, theirs: &str) -> String {
    let (base, live_entries) = (entries(base), entries(live));
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for line in parse(theirs) {
        let Line::Entry(name, theirs_fields) = line else {
            out.push(line);
            continue;
        };
        seen.insert(name.clone());
        match (base.get(&name), live_entries.get(&name)) {
            (Some(_), None) => {}
            (None, None) => out.push(Line::Entry(name, theirs_fields)),
            (base_fields, Some(live_fields)) => {
                let fields = merge_fields(file, base_fields, live_fields, &theirs_fields);
                out.push(Line::Entry(name, fields));
            }
        }
    }
    for line in parse(live) {
        let Line::Entry(name, fields) = line else { continue };
        if seen.contains(&name) || base.get(&name) == Some(&fields) {
            continue;
        }
        out.push(Line::Entry(name, fields));
    }
    render(&out)
}

/// Names that share an ID in a passwd- or group-style text.
fn by_id(text: &str) -> BTreeMap<String, Vec<String>> {
    let mut ids: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, fields) in entries(text) {
        if let Some(id) = fields.get(2) {
            ids.entry(id.clone()).or_default().push(name);
        }
    }
    ids
}

/// Two names on one number in `merged` that `theirs` didn't already have, as one sentence each.
fn new_clashes(file: &str, what: &str, merged: &str, theirs: &str, target: u32) -> Vec<String> {
    let before = by_id(theirs);
    let theirs_entries = entries(theirs);
    by_id(merged)
        .into_iter()
        .filter(|(id, names)| names.len() > 1 && before.get(id) != Some(names))
        .map(|(id, names)| {
            let (theirs_names, live_names): (Vec<String>, Vec<String>) = names
                .into_iter()
                .partition(|name| theirs_entries.get(name).and_then(|f| f.get(2)) == Some(&id));
            format!(
                "{what} {id} would be both {}'s, added or changed on the running system since the switch, and {}'s in deployment {target}'s /etc/{file}",
                live_names.join(" and "),
                theirs_names.join(" and ")
            )
        })
        .collect()
}

/// Merges all four files three ways; Err names every UID or GID the result would give two names, in which case nothing should be written.
pub fn merge_all(base: &Texts, live: &Texts, theirs: &Texts, target: u32) -> Result<Texts, String> {
    let mut merged: Texts = Default::default();
    for (i, file) in FILES.iter().enumerate() {
        merged[i] = match (&live[i], &theirs[i]) {
            (Some(live), Some(theirs)) => {
                let base = base[i].as_deref().unwrap_or(theirs);
                Some(merge3(file, base, live, theirs))
            }
            (_, theirs) => theirs.clone(),
        };
    }
    let mut clashes = Vec::new();
    for (i, what) in [(0, "UID"), (1, "GID")] {
        if let (Some(merged), Some(theirs)) = (&merged[i], &theirs[i]) {
            clashes.extend(new_clashes(FILES[i], what, merged, theirs, target));
        }
    }
    if clashes.is_empty() {
        Ok(merged)
    } else {
        Err(clashes.join("; "))
    }
}

/// What still carries while an ID clash holds additions back: accounts deleted live, members and admins removed live, and live's password, lock and aging changes to accounts theirs has by the same name; new accounts and new members wait with the clash.
pub fn merge_held(base: &Texts, live: &Texts, theirs: &Texts) -> Texts {
    let mut out = theirs.clone();
    for (i, file) in FILES.iter().enumerate() {
        let (Some(live), Some(theirs)) = (&live[i], &theirs[i]) else {
            continue;
        };
        let (base, live) = (entries(base[i].as_deref().unwrap_or(theirs)), entries(live));
        let lines: Vec<Line> = parse(theirs)
            .into_iter()
            .filter(|line| !matches!(line, Line::Entry(name, _) if base.contains_key(name) && !live.contains_key(name)))
            .map(|line| match line {
                Line::Entry(name, fields) if live.contains_key(&name) => {
                    let (base, live) = (base.get(&name), &live[&name]);
                    let merged = merge_fields(file, base, live, &fields);
                    let fields = fields
                        .iter()
                        .enumerate()
                        .map(|(n, field)| match (*file, n) {
                            ("shadow", _) | ("gshadow", 1) => merged.get(n).cloned().unwrap_or_default(),
                            _ if member_fields(file).contains(&n) => {
                                kept_members(base.and_then(|b| b.get(n)), live.get(n), Some(field)).join(",")
                            }
                            _ => field.clone(),
                        })
                        .collect();
                    Line::Entry(name, fields)
                }
                line => line,
            })
            .collect();
        out[i] = Some(render(&lines));
    }
    out
}

/// Days since 1970, shadow's unit.
fn today() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| (since.as_secs() / 86_400) as i64)
}

/// Whether a shadow entry is locked as `passwd -l` or `usermod -L` leaves it (a hash behind "!"), or expired; a bare "!" is an account that never had a password, and "!*" a package's.
pub fn held(fields: &[String]) -> bool {
    let hash = fields.get(1).map_or("", String::as_str);
    let locked = hash.starts_with('!') && hash.len() > 1 && hash != "!*";
    let expired = fields
        .get(7)
        .and_then(|days| days.parse::<i64>().ok())
        .is_some_and(|days| days <= today());
    locked || expired
}

/// The rule every declared hash follows, at a switch or a boot: it may replace `user`'s password in `shadow` unless the account is locked or expired there, which only a declared lock ("!" or "*") may go over.
pub fn may_set_hash(shadow: &str, user: &str, hash: &str) -> bool {
    hash.starts_with(['!', '*']) || !entries(shadow).get(user).is_some_and(|fields| held(fields))
}

/// Whether an ID belongs to a person rather than a package: root and the regular range, not nobody.
fn regular(id: Option<&String>) -> bool {
    id.and_then(|id| id.parse::<u32>().ok())
        .is_some_and(|id| id == 0 || (1000..65534).contains(&id))
}

/// The result of carrying live accounts into an older or other deployment's files, and the names left out because of an ID clash.
pub struct Carried {
    pub texts: Texts,
    pub skipped: Vec<String>,
}

/// Carries the running system's accounts into a deployment it rolls back to: people's entries and passwords come from live, package accounts from the target with live's members added, and accounts only live has stay unless their ID is another name's there.
pub fn carry(live: &Texts, target: &Texts) -> Carried {
    let mut texts: Texts = target.clone();
    let mut skipped = Vec::new();
    let mut added: [BTreeSet<String>; 2] = Default::default();
    let mut people: [BTreeSet<String>; 2] = Default::default();
    for i in [0, 1] {
        let (Some(live_text), Some(target_text)) = (&live[i], &target[i]) else {
            continue;
        };
        let live_entries = entries(live_text);
        let mut ids: BTreeMap<String, String> = entries(target_text)
            .into_iter()
            .filter_map(|(name, fields)| Some((fields.get(2)?.clone(), name)))
            .collect();
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        for line in parse(target_text) {
            let Line::Entry(name, fields) = line else {
                out.push(line);
                continue;
            };
            seen.insert(name.clone());
            let Some(live_fields) = live_entries.get(&name) else {
                out.push(Line::Entry(name, fields));
                continue;
            };
            if regular(live_fields.get(2)) {
                people[i].insert(name.clone());
                out.push(Line::Entry(name, live_fields.clone()));
            } else {
                let mut fields = fields;
                for &m in member_fields(FILES[i]) {
                    if fields.len() > m {
                        fields[m] = union_members(fields.get(m), live_fields.get(m));
                    }
                }
                out.push(Line::Entry(name, fields));
            }
        }
        for line in parse(live_text) {
            let Line::Entry(name, fields) = line else { continue };
            if seen.contains(&name) {
                continue;
            }
            let id = fields.get(2).cloned().unwrap_or_default();
            if let Some(other) = ids.get(&id) {
                skipped.push(format!(
                    "{name} ({} {id}, which is {other}'s there)",
                    if i == 0 { "UID" } else { "GID" }
                ));
                continue;
            }
            ids.insert(id, name.clone());
            added[i].insert(name.clone());
            people[i].insert(name.clone());
            out.push(Line::Entry(name, fields));
        }
        texts[i] = Some(render(&out));
    }
    for (i, of) in [(2, 0), (3, 1)] {
        let (Some(live_text), Some(target_text)) = (&live[i], &target[i]) else {
            continue;
        };
        let live_entries = entries(live_text);
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        for line in parse(target_text) {
            let Line::Entry(name, fields) = line else {
                out.push(line);
                continue;
            };
            seen.insert(name.clone());
            match live_entries.get(&name) {
                Some(live_fields) if people[of].contains(&name) => out.push(Line::Entry(name, live_fields.clone())),
                Some(live_fields) => {
                    let mut fields = fields;
                    for &m in member_fields(FILES[i]) {
                        if fields.len() > m {
                            fields[m] = union_members(fields.get(m), live_fields.get(m));
                        }
                    }
                    out.push(Line::Entry(name, fields));
                }
                None => out.push(Line::Entry(name, fields)),
            }
        }
        for line in parse(live_text) {
            let Line::Entry(name, fields) = line else { continue };
            if !seen.contains(&name) && added[of].contains(&name) {
                out.push(Line::Entry(name, fields));
            }
        }
        texts[i] = Some(render(&out));
    }
    Carried { texts, skipped }
}

/// The shell an account a keep brings back as a placeholder gets.
pub const PLACEHOLDER_SHELL: &str = "/usr/bin/nologin";

/// The account files a keep makes, the people brought back as locked placeholders and the names left out for an ID clash.
pub struct Kept {
    pub texts: Texts,
    pub placeholders: Vec<String>,
    pub skipped: Vec<String>,
}

/// Group members for a keep: the replaced root's, but each user `declared` is a member only as the snapshot has it.
fn kept_group_members(snapshot: Option<&String>, replaced: Option<&String>, declared: &BTreeSet<String>) -> String {
    let mut out: Vec<String> = members(replaced)
        .into_iter()
        .filter(|m| !declared.contains(m))
        .collect();
    for m in members(snapshot) {
        if declared.contains(&m) && !out.contains(&m) {
            out.push(m);
        }
    }
    out.join(",")
}

/// The accounts of the root a keep makes from a snapshot whose files are `snapshot`, replacing the root whose files are `replaced`: the users `declared` (the snapshot's config's) keep the snapshot's passwd entries and group memberships; every other user and group created or changed since comes by name from `replaced`, as does every shadow and gshadow entry, so a revoked password stays revoked; a person (root or the regular ID range) or a declared user `replaced` no longer has comes back as a placeholder whose password is "!", expired, with a nologin shell, so its ID stays taken and nobody logs in as it.
pub fn keep(snapshot: &Texts, replaced: &Texts, declared: &BTreeSet<String>) -> Kept {
    let mut texts: Texts = snapshot.clone();
    let mut placeholders = Vec::new();
    let mut skipped = Vec::new();
    let mut added: [BTreeSet<String>; 2] = Default::default();
    for i in [0, 1] {
        let (Some(snapshot_text), Some(replaced_text)) = (&snapshot[i], &replaced[i]) else {
            continue;
        };
        let replaced_entries = entries(replaced_text);
        let mut ids: BTreeMap<String, String> = entries(snapshot_text)
            .into_iter()
            .filter_map(|(name, fields)| Some((fields.get(2)?.clone(), name)))
            .collect();
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        for line in parse(snapshot_text) {
            let Line::Entry(name, fields) = line else {
                out.push(line);
                continue;
            };
            seen.insert(name.clone());
            let fields = match (i, replaced_entries.get(&name)) {
                (0, Some(_)) if declared.contains(&name) => fields,
                (0, Some(now)) => now.clone(),
                (0, None) if declared.contains(&name) || regular(fields.get(2)) => {
                    placeholders.push(name.clone());
                    let mut fields = fields;
                    fields.resize(fields.len().max(7), String::new());
                    fields[6] = PLACEHOLDER_SHELL.to_string();
                    fields
                }
                (_, Some(now)) => {
                    let mut now = now.clone();
                    if now.len() > 3 {
                        now[3] = kept_group_members(fields.get(3), replaced_entries[&name].get(3), declared);
                    }
                    now
                }
                (_, None) => fields,
            };
            out.push(Line::Entry(name, fields));
        }
        for line in parse(replaced_text) {
            let Line::Entry(name, fields) = line else { continue };
            if seen.contains(&name) {
                continue;
            }
            let id = fields.get(2).cloned().unwrap_or_default();
            if let Some(other) = ids.get(&id) {
                skipped.push(format!(
                    "{name} ({} {id}, which is {other}'s in the snapshot)",
                    if i == 0 { "UID" } else { "GID" }
                ));
                continue;
            }
            ids.insert(id, name.clone());
            added[i].insert(name.clone());
            out.push(Line::Entry(name, fields));
        }
        texts[i] = Some(render(&out));
    }
    for (i, of) in [(2, 0), (3, 1)] {
        let (Some(snapshot_text), Some(replaced_text)) = (&snapshot[i], &replaced[i]) else {
            continue;
        };
        let replaced_entries = entries(replaced_text);
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        for line in parse(snapshot_text) {
            let Line::Entry(name, fields) = line else {
                out.push(line);
                continue;
            };
            seen.insert(name.clone());
            let mut fields = match replaced_entries.get(&name) {
                Some(now) => now.clone(),
                None => fields,
            };
            if of == 0 && placeholders.contains(&name) {
                fields.resize(fields.len().max(9), String::new());
                fields[1] = "!".to_string();
                fields[7] = "1".to_string();
            }
            if of == 1
                && let Some(now) = replaced_entries.get(&name)
                && now.len() > 3
            {
                fields[3] = kept_group_members(entries(snapshot_text)[&name].get(3), now.get(3), declared);
            }
            out.push(Line::Entry(name, fields));
        }
        for name in placeholders.iter().filter(|name| of == 0 && !seen.contains(*name)) {
            let fields: Vec<String> = [name.as_str(), "!", "", "", "", "", "", "1", ""]
                .map(str::to_string)
                .to_vec();
            out.push(Line::Entry(name.clone(), fields));
        }
        for line in parse(replaced_text) {
            let Line::Entry(name, fields) = line else { continue };
            if !seen.contains(&name) && added[of].contains(&name) {
                out.push(Line::Entry(name, fields));
            }
        }
        texts[i] = Some(render(&out));
    }
    Kept {
        texts,
        placeholders,
        skipped,
    }
}

/// Whether `name` is a placeholder a keep left in `passwd` and `shadow`: the nologin shell, "!" for a password and expired.
pub fn is_placeholder(passwd: &str, shadow: &str, name: &str) -> bool {
    let shell = entries(passwd).get(name).and_then(|fields| fields.get(6).cloned());
    let held = entries(shadow).get(name).is_some_and(|fields| {
        fields.get(1).map(String::as_str) == Some("!") && fields.get(7).map(String::as_str) == Some("1")
    });
    shell.as_deref() == Some(PLACEHOLDER_SHELL) && held
}

/// The UIDs and GIDs `texts` hands out, with their names.
pub fn ids(texts: &Texts) -> [Vec<(u32, String)>; 2] {
    let list = |text: &Option<String>| -> Vec<(u32, String)> {
        entries(text.as_deref().unwrap_or(""))
            .into_iter()
            .filter_map(|(name, fields)| Some((fields.get(2)?.parse().ok()?, name)))
            .collect()
    };
    [list(&texts[0]), list(&texts[1])]
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str =
        "root:x:0:0::/root:/bin/bash\nalpm:x:946:946::/:/usr/bin/nologin\nalice:x:1000:1000::/home/alice:/bin/bash\n";

    fn texts(passwd: &str, group: &str, shadow: &str, gshadow: &str) -> Texts {
        [passwd, group, shadow, gshadow].map(|t| Some(t.to_string()))
    }

    #[test]
    fn a_password_changed_live_goes_over_and_theirs_changes_to_other_entries_stay() {
        let base = "root:$old:20000::::::\nalice:$a1:20000:0:99999:7:::\n";
        let live = "root:$old:20000::::::\nalice:$a2:20100:0:99999:7:::\n";
        let theirs = "root:$new:20000::::::\nalice:$a1:20000:0:99999:7:::\ncups:!*:20050::::::\n";
        assert_eq!(
            merge3("shadow", base, live, theirs),
            "root:$new:20000::::::\nalice:$a2:20100:0:99999:7:::\ncups:!*:20050::::::\n"
        );
    }

    #[test]
    fn a_lock_made_after_the_switch_reaches_the_pending_deployment_over_its_declared_hash() {
        let base = "root:*:20000::::::\nalice:$old:20000:0:99999:7:::\n";
        let live = "root:*:20000::::::\nalice:!$old:20100:0:99999:7:::\n";
        let theirs = "root:*:20000::::::\nalice:$declared:20100:0:99999:7:::\n";
        let merged = merge3("shadow", base, live, theirs);
        assert!(merged.contains("alice:!$old:"), "{merged}");
        assert!(
            !may_set_hash(&merged, "alice", "$declared"),
            "nothing enforced later undoes it"
        );
        assert!(
            may_set_hash(&merged, "alice", "!"),
            "a declared lock may go over a lock"
        );
        let expired = "alice:$old:20000:0:99999:7::1:\n";
        assert!(!may_set_hash(expired, "alice", "$declared"));
        assert!(
            may_set_hash("alice:!:20000:0:99999:7:::\n", "alice", "$declared"),
            "never had a password"
        );
        assert!(may_set_hash(base, "alice", "$declared") && may_set_hash(base, "bob", "$declared"));
    }

    #[test]
    fn users_added_or_removed_live_follow_live_and_theirs_additions_stay() {
        let live = format!("{PASSWD}bob:x:1001:1001::/home/bob:/bin/zsh\n")
            .replace("alpm:x:946:946::/:/usr/bin/nologin\n", "");
        let theirs = format!("{PASSWD}cups:x:209:209::/:/usr/bin/nologin\n");
        assert_eq!(
            merge3("passwd", PASSWD, &live, &theirs),
            "root:x:0:0::/root:/bin/bash\nalice:x:1000:1000::/home/alice:/bin/bash\ncups:x:209:209::/:/usr/bin/nologin\nbob:x:1001:1001::/home/bob:/bin/zsh\n"
        );
    }

    #[test]
    fn group_members_merge_as_sets() {
        let base = "wheel:x:998:alice\nvideo:x:985:\n";
        let live = "wheel:x:998:alice,bob\nvideo:x:985:\n";
        let theirs = "wheel:x:998:alice,carol\nvideo:x:985:alice\n";
        assert_eq!(
            merge3("group", base, live, theirs),
            "wheel:x:998:alice,carol,bob\nvideo:x:985:alice\n"
        );
        let live_removed = "wheel:x:998:\nvideo:x:985:\n";
        assert_eq!(
            merge3("group", base, live_removed, theirs),
            "wheel:x:998:carol\nvideo:x:985:alice\n"
        );
    }

    #[test]
    fn a_user_added_live_on_an_id_theirs_gave_another_name_is_a_clash_and_nothing_merges() {
        let group = "root:x:0:\nalpm:x:946:\n";
        let base = texts(PASSWD, group, "", "");
        let live = texts(
            &format!("{PASSWD}bob:x:960:960::/home/bob:/bin/bash\n"),
            &format!("{group}bob:x:960:\n"),
            "bob:!:1::::::\n",
            "",
        );
        let theirs = texts(
            &format!("{PASSWD}pkguser:x:960:960::/:/usr/bin/nologin\n"),
            &format!("{group}pkguser:x:960:\n"),
            "",
            "",
        );
        let err = merge_all(&base, &live, &theirs, 3).unwrap_err();
        assert!(
            err.contains("UID 960 would be both bob's, added or changed on the running system since the switch, and pkguser's in deployment 3's /etc/passwd"),
            "{err}"
        );
        assert!(err.contains("GID 960"), "{err}");
        let fine = texts(
            &format!("{PASSWD}bob:x:961:961::/home/bob:/bin/bash\n"),
            &format!("{group}bob:x:961:\n"),
            "bob:!:1::::::\n",
            "",
        );
        let merged = merge_all(&base, &fine, &theirs, 3).unwrap();
        assert!(
            merged[0].as_ref().unwrap().contains("bob:x:961") && merged[0].as_ref().unwrap().contains("pkguser:x:960")
        );
        assert_eq!(merged[2].as_deref(), Some("bob:!:1::::::\n"));
    }

    #[test]
    fn a_clash_still_carries_the_passwords_and_aging_of_accounts_both_sides_have() {
        let group = "root:x:0:\nwheel:x:998:alice\nalice:x:1000:\n";
        let base = texts(
            PASSWD,
            group,
            "root:$r:20000::::::\nalpm:!*:20000::::::\nalice:$a1:20000:0:99999:7:::\n",
            "root:::\nwheel:::alice\nalice:!::\n",
        );
        let live = texts(
            &format!("{PASSWD}carol:x:960:960::/home/carol:/bin/bash\n"),
            &format!("{}carol:x:960:\n", group.replace("alice\n", "alice,carol\n")),
            "root:$r:20000::::::\nalpm:!*:20000::::::\nalice:$a2:20100:1:60:14:30:20500:\ncarol:$c:20100::::::\n",
            "root:::\nwheel:$w:root:alice,carol\nalice:!::\ncarol:!::\n",
        );
        let theirs = texts(
            &format!("{PASSWD}pkguser:x:960:960::/:/usr/bin/nologin\n"),
            &format!("{group}pkguser:x:960:\n"),
            "root:$r9:20050::::::\nalpm:!*:20000::::::\nalice:$a1:20000:0:99999:7:::\npkguser:!*:20050::::::\n",
            "root:::\nwheel:::alice\nalice:!::\npkguser:!::\n",
        );
        assert!(merge_all(&base, &live, &theirs, 3).is_err());
        let carried = merge_held(&base, &live, &theirs);
        assert_eq!(carried[0], theirs[0]);
        assert_eq!(carried[1], theirs[1]);
        assert_eq!(
            carried[2].as_deref(),
            Some(
                "root:$r9:20050::::::\nalpm:!*:20000::::::\nalice:$a2:20100:1:60:14:30:20500:\npkguser:!*:20050::::::\n"
            ),
            "live's change wins, theirs' own change stays, and carol waits with the clash"
        );
        assert_eq!(
            carried[3].as_deref(),
            Some("root:::\nwheel:$w::alice\nalice:!::\npkguser:!::\n"),
            "the group password carries; members and admins wait with the group file"
        );
    }

    #[test]
    fn a_clash_still_carries_revoked_access() {
        let passwd = format!("{PASSWD}mallory:x:1001:1001::/home/mallory:/bin/bash\n");
        let group = "root:x:0:\nwheel:x:998:alice,mallory\ndocker:x:970:alice\nalice:x:1000:\nmallory:x:1001:\n";
        let base = texts(
            &passwd,
            group,
            "root:$r:20000::::::\nalpm:!*:20000::::::\nalice:$a:20000::::::\nmallory:$m:20000::::::\n",
            "root:::\nwheel::alice:alice,mallory\ndocker:::alice\nalice:!::\nmallory:!::\n",
        );
        let live = texts(
            &format!("{PASSWD}carol:x:960:960::/home/carol:/bin/bash\n"),
            "root:x:0:\nwheel:x:998:\ndocker:x:970:alice,carol\nalice:x:1000:\ncarol:x:960:\n",
            "root:$r:20000:::::0:\nalpm:!*:20000::::::\nalice:!$a:20000::::::\ncarol:$c:20100::::::\n",
            "root:::\nwheel:::\ndocker:::alice,carol\nalice:!::\ncarol:!::\n",
        );
        let theirs = texts(
            &format!("{passwd}pkguser:x:960:960::/:/usr/bin/nologin\n"),
            &format!("{group}pkguser:x:960:\n"),
            "root:$r:20000::::::\nalpm:!*:20000::::::\nalice:$a:20000::::::\nmallory:$m:20000::::::\npkguser:!*:20050::::::\n",
            "root:::\nwheel::alice:alice,mallory\ndocker:::alice\nalice:!::\nmallory:!::\npkguser:!::\n",
        );
        assert!(merge_all(&base, &live, &theirs, 3).is_err());
        let held = merge_held(&base, &live, &theirs);
        assert_eq!(
            held[0].as_deref(),
            Some(format!("{PASSWD}pkguser:x:960:960::/:/usr/bin/nologin\n").as_str()),
            "mallory, deleted live, is deleted there too; carol waits"
        );
        assert_eq!(
            held[1].as_deref(),
            Some("root:x:0:\nwheel:x:998:\ndocker:x:970:alice\nalice:x:1000:\npkguser:x:960:\n"),
            "removals from wheel carry; carol's docker membership waits"
        );
        assert!(held[2].as_deref().unwrap().contains("alice:!$a:"), "a lock carries");
        assert!(
            held[2].as_deref().unwrap().contains("root:$r:20000:::::0:"),
            "an expiry (chage -E 0) carries"
        );
        assert!(!held[2].as_deref().unwrap().contains("mallory"));
        assert_eq!(
            held[3].as_deref(),
            Some("root:::\nwheel:::\ndocker:::alice\nalice:!::\npkguser:!::\n"),
            "a removed group admin carries"
        );
    }

    #[test]
    fn a_clash_theirs_already_had_is_not_the_merges_to_report() {
        let dup = format!("{PASSWD}twin:x:946:946::/:/usr/bin/nologin\n");
        let all = texts(&dup, "", "", "");
        assert!(merge_all(&all, &all, &all, 2).is_ok());
    }

    #[test]
    fn a_rollback_carries_people_and_passwords_and_keeps_the_targets_package_accounts() {
        let live = texts(
            "root:x:0:0::/root:/bin/zsh\nalpm:x:946:946:new gecos:/:/usr/bin/nologin\nalice:x:1000:1000::/home/alice:/bin/fish\nbob:x:1001:1001::/home/bob:/bin/bash\nnewpkg:x:950:950::/:/usr/bin/nologin\nclash:x:209:209::/:/usr/bin/nologin\n",
            "root:x:0:\nwheel:x:998:alice,bob\nalice:x:1000:\nbob:x:1001:\nnewpkg:x:950:\n",
            "root:$r2:1::::::\nalpm:!*:1::::::\nalice:$a2:1::::::\nbob:$b:1::::::\nnewpkg:!*:1::::::\nclash:!*:1::::::\n",
            "root:::\nwheel:::alice,bob\nalice:!::\nbob:!::\nnewpkg:!::\n",
        );
        let target = texts(
            "root:x:0:0::/root:/bin/bash\nalpm:x:946:946::/:/usr/bin/nologin\nalice:x:1000:1000::/home/alice:/bin/bash\ncups:x:209:209::/:/usr/bin/nologin\n",
            "root:x:0:\nwheel:x:998:alice\nalice:x:1000:\n",
            "root:$r1:1::::::\nalpm:!*:1::::::\nalice:$a1:1::::::\ncups:!*:1::::::\n",
            "root:::\nwheel:::alice\nalice:!::\n",
        );
        let carried = carry(&live, &target);
        let passwd = carried.texts[0].as_deref().unwrap();
        assert!(passwd.contains("root:x:0:0::/root:/bin/zsh\n"), "{passwd}");
        assert!(
            passwd.contains("alpm:x:946:946::/:/usr/bin/nologin\n"),
            "package accounts come from the target: {passwd}"
        );
        assert!(
            passwd.contains("alice:x:1000:1000::/home/alice:/bin/fish\n"),
            "{passwd}"
        );
        assert!(passwd.contains("bob:x:1001:1001::/home/bob:/bin/bash\n"), "{passwd}");
        assert!(
            passwd.contains("newpkg:x:950:"),
            "accounts the target doesn't know stay: {passwd}"
        );
        assert!(passwd.contains("cups:x:209:") && !passwd.contains("clash:"), "{passwd}");
        assert_eq!(carried.skipped, ["clash (UID 209, which is cups's there)"]);
        let group = carried.texts[1].as_deref().unwrap();
        assert!(group.contains("wheel:x:998:alice,bob\n"), "{group}");
        let shadow = carried.texts[2].as_deref().unwrap();
        assert!(
            shadow.contains("root:$r2:") && shadow.contains("alice:$a2:") && shadow.contains("bob:$b:"),
            "{shadow}"
        );
        assert!(shadow.contains("cups:!*") && !shadow.contains("clash:"), "{shadow}");
        let gshadow = carried.texts[3].as_deref().unwrap();
        assert!(
            gshadow.contains("wheel:::alice,bob\n") && gshadow.contains("bob:!::\n"),
            "{gshadow}"
        );
    }
    #[test]
    fn a_keep_takes_declared_users_from_the_snapshot_carries_the_rest_and_brings_people_deleted_since_back_locked() {
        let snapshot = texts(
            "root:x:0:0::/root:/bin/bash\nalice:x:1000:1000::/home/alice:/bin/bash\nbob:x:1001:1001::/home/bob:/bin/bash\ndave:x:1002:1002::/home/dave:/bin/bash\nerin:x:1003:1003::/home/erin:/bin/zsh\nold:x:950:950::/:/usr/bin/nologin\n",
            "root:x:0:\nwheel:x:998:bob,alice\nalice:x:1000:\nbob:x:1001:\ndave:x:1002:\nerin:x:1003:\nold:x:950:\n",
            "root:$r0:20000::::::\nalice:$a1:20000:0:99999:7:::\nbob:$b1:20000:0:99999:7:::\ndave:$d1:20000:0:99999:7:::\nold:!*:20000::::::\n",
            "root:::\nwheel:::bob,alice\nalice:!::\nbob:!::\ndave:!::\nerin:!::\nold:!::\n",
        );
        let replaced = texts(
            "root:x:0:0::/root:/bin/bash\nalice:x:1000:1000::/home/alice:/bin/fish\nbob:x:1001:1001::/home/bob:/bin/fish\ncarol:x:1004:1004::/home/carol:/bin/bash\nsvc:x:980:980::/var/lib/svc:/usr/bin/nologin\nfrank:x:1002:1005::/home/frank:/bin/bash\n",
            "root:x:0:\nwheel:x:998:alice,carol\nalice:x:1000:\nbob:x:1001:\ncarol:x:1004:\nsvc:x:980:\n",
            "root:$r2:20100::::::\nalice:!$a1:20100:0:99999:7:::\nbob:$b2:20100:0:99999:7:::\ncarol:$c1:20100:0:99999:7:::\nsvc:!*:20100::::::\nfrank:$f1:20100:0:99999:7:::\n",
            "root:::\nwheel:::alice,carol\nalice:!::\nbob:!::\ncarol:!::\nsvc:!::\n",
        );
        let declared = BTreeSet::from(["bob".to_string(), "erin".to_string()]);
        let kept = keep(&snapshot, &replaced, &declared);
        let [passwd, group, shadow, gshadow] = kept.texts.map(Option::unwrap);
        assert!(
            passwd.contains("alice:x:1000:1000::/home/alice:/bin/fish\n"),
            "a change since: {passwd}"
        );
        assert!(
            passwd.contains("bob:x:1001:1001::/home/bob:/bin/bash\n"),
            "a declared user follows the snapshot: {passwd}"
        );
        assert!(
            passwd.contains("carol:x:1004:") && passwd.contains("svc:x:980:"),
            "users created since: {passwd}"
        );
        assert!(
            passwd.contains("dave:x:1002:1002::/home/dave:/usr/bin/nologin\n"),
            "a person deleted since: {passwd}"
        );
        assert!(
            passwd.contains("erin:x:1003:1003::/home/erin:/usr/bin/nologin\n"),
            "a declared user deleted since: {passwd}"
        );
        assert!(
            passwd.contains("old:x:950:950::/:/usr/bin/nologin\n"),
            "a package's account as the snapshot has it"
        );
        assert!(!passwd.contains("frank"), "frank's UID is dave's there: {passwd}");
        assert_eq!(kept.placeholders, ["dave", "erin"]);
        assert_eq!(kept.skipped.len(), 1, "{:?}", kept.skipped);
        assert!(
            shadow.contains("alice:!$a1:"),
            "a revoked password stays revoked: {shadow}"
        );
        assert!(
            shadow.contains("bob:$b2:") && shadow.contains("root:$r2:"),
            "passwords are the newest: {shadow}"
        );
        assert!(
            shadow.contains("dave:!:20000:0:99999:7::1:\n"),
            "locked and expired: {shadow}"
        );
        assert!(
            shadow.contains("erin:!::::::1:\n"),
            "one with no shadow entry gets one: {shadow}"
        );
        assert!(shadow.contains("carol:$c1:") && !shadow.contains("frank"), "{shadow}");
        assert!(
            group.contains("wheel:x:998:alice,carol,bob\n"),
            "bob is in wheel as the snapshot has him: {group}"
        );
        assert!(gshadow.contains("wheel:::alice,carol,bob\n"), "{gshadow}");
        assert!(
            group.contains("carol:x:1004:") && group.contains("dave:x:1002:"),
            "{group}"
        );
        assert!(is_placeholder(&passwd, &shadow, "dave") && !is_placeholder(&passwd, &shadow, "alice"));
    }
}

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};

use super::model::HookItem;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Install,
    Upgrade,
    Remove,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Trigger {
    ops: Vec<Op>,
    paths: bool,
    targets: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Hook {
    triggers: Vec<Trigger>,
    pub description: String,
    pub when: String,
}

pub fn parse(text: &str) -> Hook {
    let mut hook = Hook::default();
    let mut section = String::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.to_string();
            if section == "Trigger" {
                hook.triggers.push(Trigger::default());
            }
            continue;
        }
        let (key, value) = line.split_once('=').map_or((line, ""), |(k, v)| (k.trim(), v.trim()));
        match (section.as_str(), key) {
            ("Trigger", "Operation") => {
                let op = match value {
                    "Install" => Op::Install,
                    "Upgrade" => Op::Upgrade,
                    _ => Op::Remove,
                };
                hook.triggers.last_mut().unwrap().ops.push(op);
            }
            ("Trigger", "Type") => hook.triggers.last_mut().unwrap().paths = value != "Package",
            ("Trigger", "Target") => hook.triggers.last_mut().unwrap().targets.push(value.to_string()),
            ("Action", "Description") => hook.description = value.to_string(),
            ("Action", "When") => hook.when = value.to_string(),
            _ => {}
        }
    }
    hook
}

fn fnmatch(pattern: &str, text: &str) -> bool {
    let (Ok(p), Ok(t)) = (CString::new(pattern), CString::new(text)) else {
        return false;
    };
    unsafe { libc::fnmatch(p.as_ptr(), t.as_ptr(), 0) == 0 }
}

/// pacman's pattern list: the last pattern that matches decides, and a `!` one excludes.
fn matches(targets: &[String], text: &str) -> bool {
    for pattern in targets.iter().rev() {
        let (negated, pattern) = match pattern.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, pattern.as_str()),
        };
        if fnmatch(pattern, text) {
            return !negated;
        }
    }
    false
}

/// One package in a transaction, with the files the hook paths are matched against.
#[derive(Debug, Clone)]
pub struct Change {
    pub op: Op,
    pub name: String,
    pub files: Vec<String>,
}

impl Hook {
    /// The packages that make this hook run in a transaction of `changes`.
    pub fn triggered_by(&self, changes: &[Change]) -> Vec<String> {
        let mut out = Vec::new();
        for change in changes {
            let hit = self.triggers.iter().any(|t| {
                t.ops.contains(&change.op)
                    && match t.paths {
                        false => matches(&t.targets, &change.name),
                        true => change.files.iter().any(|f| matches(&t.targets, f)),
                    }
            });
            if hit {
                out.push(change.name.clone());
            }
        }
        out
    }
}

/// The hooks in `dirs`, later dirs overriding a file of the same name, a link to /dev/null disabling it.
pub fn load(dirs: &[String]) -> BTreeMap<String, Hook> {
    let mut found: BTreeMap<String, PathBuf> = BTreeMap::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".hook") {
                found.insert(name, entry.path());
            }
        }
    }
    found
        .into_iter()
        .filter(|(_, path)| fs::read_link(path).map_or(true, |t| t != Path::new("/dev/null")))
        .filter_map(|(name, path)| {
            let text = fs::read_to_string(&path).ok()?;
            Some((name.trim_end_matches(".hook").to_string(), parse(&text)))
        })
        .collect()
}

pub fn items(hooks: &BTreeMap<String, Hook>, changes: &[Change]) -> Vec<HookItem> {
    let mut out = Vec::new();
    for (name, hook) in hooks {
        let by = hook.triggered_by(changes);
        if by.is_empty() {
            continue;
        }
        let shown: Vec<&str> = by.iter().take(3).map(String::as_str).collect();
        let more = if by.len() > 3 { format!(" and {} more", by.len() - 3) } else { String::new() };
        out.push(HookItem {
            name: name.clone(),
            description: hook.description.clone(),
            when: hook.when.clone(),
            reason: format!("{}{more}", shown.join(", ")),
            risk: Vec::new(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DKMS: &str = "[Trigger]
Operation = Install
Operation = Upgrade
Type = Path
Target = usr/src/*/dkms.conf
Target = usr/lib/modules/*/build/include/
Target = !usr/lib/modules/*/build/include/excluded/

[Trigger]
Operation = Upgrade
Type = Package
Target = linux*

[Action]
Description = Install DKMS modules
When = PostTransaction
Exec = /usr/lib/dkms/alpm-hook install
";

    fn change(op: Op, name: &str, files: &[&str]) -> Change {
        Change {
            op,
            name: name.into(),
            files: files.iter().map(|f| f.to_string()).collect(),
        }
    }

    #[test]
    fn package_and_path_triggers_match_like_pacman() {
        let hook = parse(DKMS);
        assert_eq!(hook.when, "PostTransaction");
        let changes = [
            change(Op::Install, "nvidia-dkms", &["usr/src/nvidia-560/dkms.conf"]),
            change(Op::Upgrade, "linux-lts", &[]),
            change(Op::Install, "linux-zen", &[]),
            change(Op::Remove, "zfs-dkms", &["usr/src/zfs/dkms.conf"]),
            change(Op::Install, "x", &["usr/lib/modules/6.1/build/include/excluded/"]),
        ];
        assert_eq!(hook.triggered_by(&changes), ["nvidia-dkms", "linux-lts"]);
    }

    #[test]
    fn later_dirs_override_and_dev_null_disables() {
        let tmp = tempfile::tempdir().unwrap();
        let (sys, etc) = (tmp.path().join("sys"), tmp.path().join("etc"));
        fs::create_dir_all(&sys).unwrap();
        fs::create_dir_all(&etc).unwrap();
        fs::write(sys.join("a.hook"), DKMS).unwrap();
        fs::write(sys.join("b.hook"), DKMS).unwrap();
        fs::write(etc.join("a.hook"), DKMS.replace("Install DKMS modules", "overridden")).unwrap();
        std::os::unix::fs::symlink("/dev/null", etc.join("b.hook")).unwrap();
        let hooks = load(&[sys.display().to_string(), etc.display().to_string()]);
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks["a"].description, "overridden");
        let items = items(&hooks, &[change(Op::Upgrade, "linux", &[])]);
        assert_eq!(items[0].reason, "linux");
    }
}

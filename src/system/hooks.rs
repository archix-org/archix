use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use super::Hook;
use super::apply::SystemChange;

const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/bin";

/// Hook names in run order: each after the hooks it lists, ties broken by name.
pub fn order(hooks: &BTreeMap<String, Hook>) -> Result<Vec<&str>> {
    for (name, hook) in hooks {
        if let Some(missing) = hook.after.iter().find(|dep| !hooks.contains_key(*dep)) {
            bail!("hooks.{name}.after names {missing:?}, which isn't a hook");
        }
    }
    let mut done: BTreeSet<&str> = BTreeSet::new();
    let mut ordered = Vec::new();
    while ordered.len() < hooks.len() {
        let ready: Vec<&str> = hooks
            .iter()
            .filter(|(name, hook)| {
                !done.contains(name.as_str()) && hook.after.iter().all(|dep| done.contains(dep.as_str()))
            })
            .map(|(name, _)| name.as_str())
            .collect();
        let Some(first) = ready.first() else {
            bail!("{}", cycle(hooks, &done));
        };
        done.insert(first);
        ordered.push(*first);
    }
    Ok(ordered)
}

/// Names one cycle among the hooks that can't run: every one of them waits on another that can't run either.
fn cycle(hooks: &BTreeMap<String, Hook>, done: &BTreeSet<&str>) -> String {
    let stuck_dep = |name: &str| {
        hooks[name]
            .after
            .iter()
            .find(|dep| !done.contains(dep.as_str()))
            .map(String::as_str)
    };
    let mut path: Vec<&str> = Vec::new();
    let mut next = hooks.keys().map(String::as_str).find(|name| !done.contains(name));
    while let Some(name) = next {
        if let Some(start) = path.iter().position(|seen| *seen == name) {
            let mut cycle = path[start..].to_vec();
            if cycle.len() == 1 {
                return format!("hook {name} lists itself in after");
            }
            cycle.push(name);
            return format!("hooks {} wait on each other in a cycle", cycle.join(" -> "));
        }
        path.push(name);
        next = stuck_dep(name);
    }
    "hooks wait on each other in a cycle".to_string()
}

/// The target's on_change scripts that are due: their file's entry differs from the previous generation, or the switch rewrote it on disk.
pub fn due(change: &SystemChange, rewritten: &BTreeSet<String>) -> BTreeMap<String, Hook> {
    let changed = change.changed_paths();
    change
        .to
        .on_change
        .iter()
        .filter(|(path, _)| changed.contains(path.as_str()) || rewritten.contains(*path))
        .map(|(path, hook)| (path.clone(), hook.clone()))
        .collect()
}

/// Remembers the on_change scripts a boot-time rollback skipped, as that generation had them, adding to any already waiting.
pub fn defer(path: &Path, due: &BTreeMap<String, Hook>) -> Result<()> {
    let mut pending = deferred(path);
    pending.extend(due.clone());
    if pending.is_empty() {
        return Ok(());
    }
    fs::write(path, serde_json::to_string_pretty(&pending)?).with_context(|| format!("writing {}", path.display()))
}

/// on_change scripts a boot-time rollback left for later, keyed by file.
pub fn deferred(path: &Path) -> BTreeMap<String, Hook> {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Runs the on_change scripts in `due`, then every hook of the target; failures are returned, not fatal.
pub fn run(change: &SystemChange, due: &BTreeMap<String, Hook>) -> Vec<String> {
    let mut failures = Vec::new();
    for (path, hook) in due {
        if let Err(e) = run_one(hook) {
            failures.push(format!("on_change of {path}: {e:#}"));
        }
    }
    let names = match order(&change.to.hooks) {
        Ok(names) => names,
        Err(e) => return vec![format!("{e:#}")],
    };
    for name in names {
        eprintln!("   hook {name}");
        if let Err(e) = run_one(&change.to.hooks[name]) {
            failures.push(format!("hook {name}: {e:#}"));
        }
    }
    failures
}

fn run_one(hook: &Hook) -> Result<()> {
    let script = ["bash", "-e", "-o", "pipefail", "-c", hook.run.as_str()];
    let mut cmd = match &hook.user {
        None => {
            let mut cmd = Command::new(script[0]);
            cmd.args(&script[1..]).env_clear().current_dir("/");
            cmd.env("HOME", "/root").env("USER", "root").env("LOGNAME", "root");
            cmd
        }
        Some(user) => user_command(user, &script)?,
    };
    cmd.env("PATH", PATH);
    if let Ok(lang) = std::env::var("LANG") {
        cmd.env("LANG", lang);
    }
    let status = cmd.status()?;
    if !status.success() {
        bail!("exited with {status}");
    }
    Ok(())
}

/// The script as `user` with a clean environment: their home, and their session bus when they're logged in.
fn user_command(user: &str, script: &[&str]) -> Result<Command> {
    let Some((uid, gid, home)) = account(user) else {
        bail!("user {user} doesn't exist");
    };
    // setpriv, not runuser: runuser opens a PAM session, which starts a systemd user manager that outlives the hook.
    let mut cmd = Command::new("setpriv");
    cmd.args([
        "--reuid",
        &uid.to_string(),
        "--regid",
        &gid.to_string(),
        "--init-groups",
        "--",
    ])
    .args(script)
    .env_clear();
    cmd.env("HOME", &home).env("USER", user).env("LOGNAME", user);
    cmd.current_dir(if Path::new(&home).is_dir() { home.as_str() } else { "/" });
    let runtime = format!("/run/user/{uid}");
    if Path::new(&runtime).is_dir() {
        cmd.env("XDG_RUNTIME_DIR", &runtime);
        if Path::new(&runtime).join("bus").exists() {
            cmd.env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={runtime}/bus"));
        }
    }
    Ok(cmd)
}

fn account(user: &str) -> Option<(u32, u32, String)> {
    let name = CString::new(user).ok()?;
    let entry = unsafe { libc::getpwnam(name.as_ptr()) };
    if entry.is_null() {
        return None;
    }
    let home = unsafe { std::ffi::CStr::from_ptr((*entry).pw_dir) }
        .to_string_lossy()
        .into_owned();
    Some((unsafe { (*entry).pw_uid }, unsafe { (*entry).pw_gid }, home))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(after: &[&str]) -> Hook {
        Hook {
            run: "true".into(),
            user: None,
            after: after.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn deferred_on_change_scripts_accumulate_until_run() {
        let dir = tempfile::tempdir().unwrap();
        let pending = dir.path().join("pending");
        assert!(deferred(&pending).is_empty());
        let script = |run: &str| Hook {
            run: run.into(),
            user: None,
            after: Vec::new(),
        };
        defer(&pending, &BTreeMap::from([("/etc/a".to_string(), script("old a"))])).unwrap();
        defer(
            &pending,
            &BTreeMap::from([
                ("/etc/a".to_string(), script("new a")),
                ("/etc/b".to_string(), script("b")),
            ]),
        )
        .unwrap();
        let waiting = deferred(&pending);
        assert_eq!(waiting.len(), 2);
        assert_eq!(
            waiting["/etc/a"].run, "new a",
            "a later boot rollback's script replaces an earlier one"
        );
        defer(&dir.path().join("untouched"), &BTreeMap::new()).unwrap();
        assert!(!dir.path().join("untouched").exists(), "nothing due writes nothing");
    }

    #[test]
    fn orders_by_after_then_name() {
        let hooks = BTreeMap::from([
            ("c".to_string(), hook(&[])),
            ("a".to_string(), hook(&["b"])),
            ("b".to_string(), hook(&["c"])),
            ("d".to_string(), hook(&[])),
        ]);
        assert_eq!(order(&hooks).unwrap(), ["c", "b", "a", "d"]);
    }

    #[test]
    fn rejects_unknown_names_and_cycles() {
        let unknown = BTreeMap::from([("a".to_string(), hook(&["nope"]))]);
        assert!(order(&unknown).unwrap_err().to_string().contains("\"nope\""));
        let cycle = BTreeMap::from([
            ("a".to_string(), hook(&["b"])),
            ("b".to_string(), hook(&["c"])),
            ("c".to_string(), hook(&["b"])),
            ("d".to_string(), hook(&[])),
        ]);
        assert_eq!(
            order(&cycle).unwrap_err().to_string(),
            "hooks b -> c -> b wait on each other in a cycle",
            "a only waits behind the cycle, so it isn't named"
        );
        let itself = BTreeMap::from([("a".to_string(), hook(&["a"]))]);
        assert_eq!(order(&itself).unwrap_err().to_string(), "hook a lists itself in after");
    }
}

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

/// Runs the on_change scripts in `due`, then every hook, on the system under `root`; failures are returned, not fatal.
pub fn run(root: &Path, due: &BTreeMap<String, Hook>, hooks: &BTreeMap<String, Hook>) -> Vec<String> {
    let mut failures = Vec::new();
    for (path, hook) in due {
        if let Err(e) = run_one(root, hook) {
            failures.push(format!("on_change of {path}: {e:#}"));
        }
    }
    let names = match order(hooks) {
        Ok(names) => names,
        Err(e) => return vec![format!("{e:#}")],
    };
    for name in names {
        eprintln!("   hook {name}");
        if let Err(e) = run_one(root, &hooks[name]) {
            failures.push(format!("hook {name}: {e:#}"));
        }
    }
    failures
}

fn run_one(root: &Path, hook: &Hook) -> Result<()> {
    let mut argv: Vec<String> = Vec::new();
    let mut env: Vec<(&str, String)> = vec![("PATH", PATH.to_string())];
    if let Ok(lang) = std::env::var("LANG") {
        env.push(("LANG", lang));
    }
    match &hook.user {
        None => env.extend([
            ("HOME", "/root".into()),
            ("USER", "root".into()),
            ("LOGNAME", "root".into()),
        ]),
        Some(user) => {
            let Some((uid, gid, home)) = account(root, user) else {
                bail!("user {user} doesn't exist");
            };
            // setpriv, not runuser: runuser opens a PAM session, which starts a systemd user manager that outlives the hook.
            argv.extend(
                [
                    "setpriv",
                    "--reuid",
                    &uid.to_string(),
                    "--regid",
                    &gid.to_string(),
                    "--init-groups",
                    "--",
                ]
                .map(String::from),
            );
            argv.extend(["env", "-C"].map(String::from));
            argv.push(if root.join(home.trim_start_matches('/')).is_dir() {
                home.clone()
            } else {
                "/".into()
            });
            env.extend([("HOME", home), ("USER", user.clone()), ("LOGNAME", user.clone())]);
            let runtime = format!("/run/user/{uid}");
            if root.join(runtime.trim_start_matches('/')).is_dir() {
                if root.join(runtime.trim_start_matches('/')).join("bus").exists() {
                    env.push(("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={runtime}/bus")));
                }
                env.push(("XDG_RUNTIME_DIR", runtime));
            }
        }
    }
    argv.extend(["bash", "-e", "-o", "pipefail", "-c", hook.run.as_str()].map(String::from));
    let mut cmd = if root == Path::new("/") {
        Command::new(&argv[0])
    } else {
        let mut cmd = Command::new("chroot");
        cmd.arg(root).arg(&argv[0]);
        cmd
    };
    cmd.args(&argv[1..]).env_clear().envs(env).current_dir("/");
    let status = cmd.status().with_context(|| format!("running {}", argv[0]))?;
    if !status.success() {
        bail!("exited with {status}");
    }
    Ok(())
}

/// A user's uid, gid and home: from the passwd file under `root`, or on the running system from NSS as well.
fn account(root: &Path, user: &str) -> Option<(u32, u32, String)> {
    let from_file = fs::read_to_string(root.join("etc/passwd")).ok().and_then(|text| {
        text.lines().find_map(|line| {
            let fields: Vec<&str> = line.split(':').collect();
            (fields.len() >= 6 && fields[0] == user)
                .then(|| Some((fields[2].parse().ok()?, fields[3].parse().ok()?, fields[5].to_string())))
                .flatten()
        })
    });
    if from_file.is_some() || root != Path::new("/") {
        return from_file;
    }
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

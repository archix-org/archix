use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

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

/// Splits due on_change scripts into those run right after the switch writes their file, on the live system under `root` (files outside the deployment trees whose user exists there), and those left for the deployment's first boot.
pub fn split_live(due: BTreeMap<String, Hook>, root: &Path) -> (BTreeMap<String, Hook>, BTreeMap<String, Hook>) {
    due.into_iter().partition(|(path, hook)| {
        super::apply::deployment_tree(path).is_none()
            && hook.user.as_ref().is_none_or(|user| account(root, user).is_some())
    })
}

/// The on_change scripts and hooks waiting for one deployment's boot, whose activation runs them once its users exist.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pending {
    #[serde(default)]
    pub on_change: BTreeMap<String, Hook>,
    #[serde(default)]
    pub hooks: BTreeMap<String, Hook>,
}

impl Pending {
    pub fn is_empty(&self) -> bool {
        self.on_change.is_empty() && self.hooks.is_empty()
    }
}

/// Where what deployment `number`'s boot runs is recorded.
pub fn pending_path(state: &Path, number: u32) -> PathBuf {
    state.join("pending-hooks").join(format!("{number}.json"))
}

/// What waits for deployment `number`'s boot; an unreadable record is set aside and runs nothing.
pub fn pending(state: &Path, number: u32) -> Pending {
    let path = pending_path(state, number);
    let Ok(text) = fs::read_to_string(&path) else {
        return Pending::default();
    };
    serde_json::from_str(&text).unwrap_or_else(|e| {
        let kept = path.with_extension("unreadable");
        eprintln!(
            "warning: {} is unreadable ({e}); its hooks won't run, the file is kept as {}",
            path.display(),
            kept.display()
        );
        let _ = fs::rename(&path, &kept);
        Pending::default()
    })
}

/// Records what deployment `number`'s boot runs: the `due` on_change scripts, added to any already waiting for it, and `hooks`.
pub fn defer(state: &Path, number: u32, due: &BTreeMap<String, Hook>, hooks: &BTreeMap<String, Hook>) -> Result<()> {
    let mut waiting = pending(state, number);
    waiting.on_change.extend(due.clone());
    waiting.hooks = hooks.clone();
    let path = pending_path(state, number);
    if waiting.is_empty() {
        return match fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("removing {}", path.display()))
            }
            _ => Ok(()),
        };
    }
    crate::durable::write(&path, serde_json::to_string_pretty(&waiting)?.as_bytes())
        .with_context(|| format!("writing {}", path.display()))
}

/// At boot, under `root`: runs what waits for deployment `number`, deleting its record only once all of it succeeded; returns the failures.
pub fn run_pending(root: &Path, state: &Path, number: u32) -> Vec<String> {
    let waiting = pending(state, number);
    if waiting.is_empty() {
        return Vec::new();
    }
    println!("archix: running deployment {number}'s hooks");
    let failures = run(root, &waiting.on_change, &waiting.hooks);
    if failures.is_empty() {
        // What the hooks wrote must be on disk before the record that they still have to run goes.
        if let Err(e) = crate::durable::sync_filesystems_checked(&[root, state]) {
            return vec![format!("flushing what the hooks wrote: {e:#}")];
        }
        let path = pending_path(state, number);
        if let Err(e) = fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return vec![format!("clearing {}: {e}", path.display())];
        }
    }
    failures
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
    let status = cmd.status()?;
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
    fn deferred_on_change_scripts_accumulate_and_hooks_are_replaced_until_run() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path();
        assert!(pending(state, 3).is_empty());
        let script = |run: &str| Hook {
            run: run.into(),
            user: None,
            after: Vec::new(),
        };
        let hooks = |run: &str| BTreeMap::from([("h".to_string(), script(run))]);
        defer(
            state,
            3,
            &BTreeMap::from([("/etc/a".to_string(), script("old a"))]),
            &hooks("first"),
        )
        .unwrap();
        defer(
            state,
            3,
            &BTreeMap::from([
                ("/etc/a".to_string(), script("new a")),
                ("/etc/b".to_string(), script("b")),
            ]),
            &hooks("second"),
        )
        .unwrap();
        let waiting = pending(state, 3);
        assert_eq!(waiting.on_change.len(), 2);
        assert_eq!(
            waiting.on_change["/etc/a"].run, "new a",
            "a later rollback's script replaces an earlier one"
        );
        assert_eq!(waiting.hooks["h"].run, "second");
        assert!(pending(state, 4).is_empty(), "each deployment has its own");
        defer(state, 5, &BTreeMap::new(), &BTreeMap::new()).unwrap();
        assert!(!pending_path(state, 5).exists(), "nothing due writes nothing");
    }

    #[test]
    fn pending_hooks_are_cleared_only_once_all_of_them_succeeded() {
        let dir = tempfile::tempdir().unwrap();
        let (state, log) = (dir.path().join("state"), dir.path().join("log"));
        let hook = |run: String| Hook {
            run,
            user: None,
            after: Vec::new(),
        };
        let flag = dir.path().join("fail");
        fs::write(&flag, "").unwrap();
        let hooks = BTreeMap::from([(
            "h".to_string(),
            hook(format!("echo ran >> {}; test ! -e {}", log.display(), flag.display())),
        )]);
        defer(&state, 7, &BTreeMap::new(), &hooks).unwrap();
        let failures = run_pending(Path::new("/"), &state, 7);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(
            pending_path(&state, 7).exists(),
            "a failed hook stays pending for the next boot"
        );
        fs::remove_file(&flag).unwrap();
        assert!(run_pending(Path::new("/"), &state, 7).is_empty());
        assert!(!pending_path(&state, 7).exists());
        assert_eq!(fs::read_to_string(&log).unwrap(), "ran\nran\n");
        assert!(run_pending(Path::new("/"), &state, 7).is_empty(), "nothing runs twice");
        assert_eq!(fs::read_to_string(&log).unwrap(), "ran\nran\n");
    }

    #[test]
    fn on_change_of_files_written_live_runs_now_unless_its_user_is_new() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("etc")).unwrap();
        fs::write(
            root.path().join("etc/passwd"),
            "root:x:0:0::/root:/bin/bash\nalice:x:1000:1000::/home/alice:/bin/bash\n",
        )
        .unwrap();
        let script = |user: Option<&str>| Hook {
            run: "true".into(),
            user: user.map(str::to_string),
            after: Vec::new(),
        };
        let due = BTreeMap::from([
            ("/etc/a.conf".to_string(), script(None)),
            ("/usr/local/bin/b".to_string(), script(None)),
            ("/srv/c".to_string(), script(None)),
            ("/home/alice/.config/d".to_string(), script(Some("alice"))),
            ("/home/bob/.config/e".to_string(), script(Some("bob"))),
        ]);
        let (live, at_boot) = split_live(due, root.path());
        assert_eq!(
            live.keys().collect::<Vec<_>>(),
            ["/home/alice/.config/d", "/srv/c"],
            "files written at the switch whose user exists run now"
        );
        assert_eq!(
            at_boot.keys().collect::<Vec<_>>(),
            ["/etc/a.conf", "/home/bob/.config/e", "/usr/local/bin/b"],
            "deployment files, and a user only the new deployment has, wait for the boot"
        );
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

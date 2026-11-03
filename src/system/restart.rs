use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CStr;
use std::fs;
use std::path::Path;
use std::process::Command;

use crate::aur::srcinfo::dep_name;
use crate::pacman::parse_desc;

/// Where interpreted code lives; a changed package with files here can affect a daemon that doesn't keep it mapped.
const INTERPRETED: &[&str] = &[
    "usr/lib/python3",
    "usr/share/perl5/",
    "usr/lib/perl5/",
    "usr/lib/node_modules/",
    "usr/lib/ruby/",
    "usr/share/lua/",
    "usr/lib/lua/",
];

/// System units whose restart would end the session or cut you off, so they're only reported, like NixOS's restartIfChanged = false.
const PROTECTED: &[&str] = &[
    "display-manager.service",
    "dbus.service",
    "dbus-broker.service",
    "systemd-logind.service",
    "NetworkManager.service",
    "systemd-networkd.service",
    "systemd-resolved.service",
    "iwd.service",
    "wpa_supplicant.service",
    "sshd.service",
    "getty@",
    "serial-getty@",
    "user@",
];

/// User units that are the desktop session itself, or that running apps hold streams to, such as audio and screen sharing.
const SESSION: &[&str] = &[
    "dbus",
    "pipewire",
    "wireplumber",
    "xdg-desktop-portal",
    "plasma-kwin",
    "plasma-plasmashell",
    "plasma-ksmserver",
    "plasma-workspace",
    "niri",
    "gnome-shell",
    "org.gnome.Shell",
    "hyprland",
    "sway",
    "wayland-wm",
];

/// What a switch has to restart, reexec or leave for a logout.
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    pub system: BTreeSet<String>,
    /// Units per user name.
    pub user: BTreeMap<String, BTreeSet<String>>,
    pub reexec: bool,
    pub user_reexec: BTreeSet<String>,
    /// Units left running on purpose, and programs in desktop sessions, that only a logout or reboot refreshes.
    pub left: BTreeSet<String>,
}

/// One package from pacman's local database, as far as restarts care.
struct Installed {
    depends: Vec<String>,
    optdepends: Vec<String>,
    provides: Vec<String>,
    files: Vec<String>,
}

fn read_db(db: &Path) -> BTreeMap<String, Installed> {
    let mut packages = BTreeMap::new();
    for entry in fs::read_dir(db).into_iter().flatten().flatten() {
        let Ok(desc) = fs::read_to_string(entry.path().join("desc")) else {
            continue;
        };
        let desc = parse_desc(&desc);
        let Some(name) = desc.get("NAME").and_then(|n| n.first()).cloned() else {
            continue;
        };
        let files = fs::read_to_string(entry.path().join("files"))
            .map(|text| parse_desc(&text).remove("FILES").unwrap_or_default())
            .unwrap_or_default();
        let list = |key: &str| desc.get(key).cloned().unwrap_or_default();
        packages.insert(
            name,
            Installed {
                depends: list("DEPENDS"),
                optdepends: list("OPTDEPENDS"),
                provides: list("PROVIDES"),
                files,
            },
        );
    }
    packages
}

/// Every package `roots` needs at runtime, through dependencies and installed optional dependencies.
fn closure(
    db: &BTreeMap<String, Installed>,
    providers: &BTreeMap<String, String>,
    roots: &[String],
) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut stack: Vec<String> = roots.to_vec();
    while let Some(name) = stack.pop() {
        let name = providers.get(&name).cloned().unwrap_or(name);
        if !seen.insert(name.clone()) {
            continue;
        }
        if let Some(pkg) = db.get(&name) {
            for dep in pkg.depends.iter().chain(&pkg.optdepends) {
                let dep = dep_name(dep.split(": ").next().unwrap_or(dep)).to_string();
                if db.contains_key(&dep) || providers.contains_key(&dep) {
                    stack.push(dep);
                }
            }
        }
    }
    seen
}

fn is_protected(unit: &str, user: bool, overrides: &BTreeMap<String, bool>, display_manager: &str) -> bool {
    if let Some(restart) = overrides.get(unit) {
        return !restart;
    }
    if user {
        return SESSION.iter().any(|prefix| unit.starts_with(prefix));
    }
    unit == display_manager
        || PROTECTED
            .iter()
            .any(|p| unit == *p || (p.ends_with('@') && unit.starts_with(p)))
}

/// Whether a process maps or holds open a file under /usr that was deleted, which is what pacman leaves when it replaces one.
fn uses_replaced(pid: &Path) -> bool {
    let replaced = |line: &str| line.ends_with(" (deleted)") && line.contains("/usr/");
    if fs::read_to_string(pid.join("maps")).is_ok_and(|maps| maps.lines().any(replaced)) {
        return true;
    }
    fs::read_dir(pid.join("fd")).into_iter().flatten().flatten().any(|fd| {
        fs::read_link(fd.path()).is_ok_and(|target| {
            let target = target.to_string_lossy();
            target.starts_with("/usr/") && target.ends_with(" (deleted)")
        })
    })
}

/// Where a cgroup path puts a process: pid 1, a user manager, a unit (with its user, if any) or a session scope.
#[derive(Debug, PartialEq)]
enum Owner {
    Init,
    UserManager(u32),
    Unit(Option<u32>, String),
    Session,
    Other,
}

fn owner(cgroup: &str) -> Owner {
    let parts: Vec<&str> = cgroup.trim_start_matches('/').split('/').collect();
    let uid = parts
        .iter()
        .find_map(|p| p.strip_prefix("user@").and_then(|p| p.strip_suffix(".service")))
        .and_then(|uid| uid.parse().ok());
    match parts.last() {
        Some(&"init.scope") if uid.is_some() => Owner::UserManager(uid.expect("checked")),
        Some(&"init.scope") => Owner::Init,
        _ => match parts.iter().rev().find(|p| p.ends_with(".service")) {
            Some(unit) if !unit.starts_with("user@") => Owner::Unit(uid, unit.to_string()),
            _ if parts.iter().any(|p| p.ends_with(".scope")) => Owner::Session,
            _ => Owner::Other,
        },
    }
}

fn user_name(uid: u32) -> Option<String> {
    let entry = unsafe { libc::getpwuid(uid) };
    if entry.is_null() {
        return None;
    }
    Some(
        unsafe { CStr::from_ptr((*entry).pw_name) }
            .to_string_lossy()
            .into_owned(),
    )
}

fn systemctl(user: Option<&str>) -> Command {
    let mut cmd = Command::new("systemctl");
    if let Some(user) = user {
        cmd.arg("--user").arg("-M").arg(format!("{user}@"));
    }
    cmd
}

fn display_manager() -> String {
    systemctl(None)
        .args(["show", "-P", "Id", "display-manager.service"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn running_units(user: Option<&str>) -> Vec<String> {
    systemctl(user)
        .args([
            "list-units",
            "--type=service",
            "--state=running",
            "--plain",
            "--no-legend",
        ])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|line| line.split_whitespace().next().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Absolute paths a unit runs: its unit file and every absolute path in ExecStart's argv.
fn unit_paths(unit: &str, user: Option<&str>) -> Vec<String> {
    let Ok(output) = systemctl(user)
        .args(["show", "-p", "FragmentPath", "-p", "ExecStart", unit])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .split([' ', '\n', '=', ';', '{', '}'])
        .filter(|word| word.starts_with('/') && word.len() > 1)
        .map(str::to_string)
        .collect()
}

/// What to restart after `changed` packages were installed, upgraded, downgraded or removed.
/// What to restart after a switch that began at unix time `started`: never the unit archix itself runs in, nor units that already (re)started since.
pub fn plan(db: &Path, changed: &BTreeSet<String>, overrides: &BTreeMap<String, bool>, started: u64) -> Plan {
    let mut plan = find(db, changed, overrides);
    let own = fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|c| c.lines().find_map(|l| l.strip_prefix("0::").map(str::to_string)))
        .map(|cgroup| owner(&cgroup));
    if let Some(Owner::Unit(uid, unit)) = own {
        match uid.and_then(user_name) {
            Some(user) => {
                plan.user.entry(user).or_default().remove(&unit);
            }
            None => {
                plan.system.remove(&unit);
            }
        }
    }
    plan.system.retain(|unit| !fresh(unit, None, started));
    for (user, units) in plan.user.iter_mut() {
        units.retain(|unit| !fresh(unit, Some(user), started));
    }
    plan.user.retain(|_, units| !units.is_empty());
    plan
}

/// Whether a unit became active at or after `started`, so it already runs what the switch installed.
fn fresh(unit: &str, user: Option<&str>, started: u64) -> bool {
    systemctl(user)
        .args(["show", "--timestamp=unix", "-P", "ActiveEnterTimestamp", unit])
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .trim_start_matches('@')
                .parse::<u64>()
                .ok()
        })
        .is_some_and(|at| at >= started)
}

fn find(db: &Path, changed: &BTreeSet<String>, overrides: &BTreeMap<String, bool>) -> Plan {
    let mut plan = Plan::default();
    let display_manager = display_manager();
    let mut users: BTreeSet<String> = BTreeSet::new();
    for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
        let pid = entry.path();
        if !entry.file_name().to_string_lossy().chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Some(cgroup) = fs::read_to_string(pid.join("cgroup"))
            .ok()
            .and_then(|c| c.lines().find_map(|l| l.strip_prefix("0::").map(str::to_string)))
        else {
            continue;
        };
        if let Owner::UserManager(uid) = owner(&cgroup)
            && let Some(name) = user_name(uid)
        {
            users.insert(name);
        }
        if !uses_replaced(&pid) {
            continue;
        }
        match owner(&cgroup) {
            Owner::Init => plan.reexec = true,
            Owner::UserManager(uid) => plan.user_reexec.extend(user_name(uid)),
            Owner::Unit(uid, unit) => add(&mut plan, uid.and_then(user_name), unit, overrides, &display_manager),
            Owner::Session => {
                let comm = fs::read_to_string(pid.join("comm")).unwrap_or_default();
                plan.left.insert(comm.trim().to_string());
            }
            Owner::Other => {}
        }
    }
    let db_packages = read_db(db);
    let interpreted: BTreeSet<&String> = changed
        .iter()
        .filter(|name| {
            db_packages.get(*name).is_some_and(|pkg| {
                pkg.files
                    .iter()
                    .any(|f| INTERPRETED.iter().any(|dir| f.starts_with(dir)))
            })
        })
        .collect();
    if interpreted.is_empty() {
        return plan;
    }
    let mut owners: BTreeMap<String, String> = BTreeMap::new();
    let mut providers: BTreeMap<String, String> = BTreeMap::new();
    for (name, pkg) in &db_packages {
        for file in &pkg.files {
            owners.insert(format!("/{}", file.trim_end_matches('/')), name.clone());
        }
        for provided in &pkg.provides {
            providers
                .entry(dep_name(provided).to_string())
                .or_insert_with(|| name.clone());
        }
    }
    let scopes = std::iter::once(None).chain(users.iter().map(|u| Some(u.as_str())));
    for user in scopes {
        for unit in running_units(user) {
            let roots: Vec<String> = unit_paths(&unit, user)
                .iter()
                .filter_map(|path| owners.get(path).cloned())
                .collect();
            if roots.is_empty() {
                continue;
            }
            let needs = closure(&db_packages, &providers, &roots);
            if interpreted.iter().any(|name| needs.contains(*name)) {
                add(&mut plan, user.map(str::to_string), unit, overrides, &display_manager);
            }
        }
    }
    plan
}

fn add(plan: &mut Plan, user: Option<String>, unit: String, overrides: &BTreeMap<String, bool>, display_manager: &str) {
    if is_protected(&unit, user.is_some(), overrides, display_manager) {
        plan.left.insert(unit);
        return;
    }
    match user {
        Some(user) => {
            plan.user.entry(user).or_default().insert(unit);
        }
        None => {
            plan.system.insert(unit);
        }
    }
}

/// Reexecs and restarts what `plan` lists; returns what failed, without failing.
pub fn run(plan: &Plan) -> Vec<String> {
    let mut failures = Vec::new();
    let mut attempt = |mut cmd: Command, what: String| match cmd.output() {
        Ok(output) if output.status.success() => {}
        Ok(output) => failures.push(format!("{what}: {}", String::from_utf8_lossy(&output.stderr).trim())),
        Err(e) => failures.push(format!("{what}: {e}")),
    };
    if plan.reexec {
        let mut cmd = systemctl(None);
        cmd.arg("daemon-reexec");
        attempt(cmd, "systemctl daemon-reexec".to_string());
    }
    for user in &plan.user_reexec {
        let mut cmd = systemctl(Some(user));
        cmd.arg("daemon-reexec");
        attempt(cmd, format!("systemctl --user daemon-reexec for {user}"));
    }
    if !plan.system.is_empty() {
        eprintln!(
            ":: restarting {}",
            plan.system.iter().cloned().collect::<Vec<_>>().join(" ")
        );
        let mut cmd = systemctl(None);
        cmd.arg("try-restart").args(&plan.system);
        attempt(cmd, "systemctl try-restart".to_string());
    }
    for (user, units) in &plan.user {
        eprintln!(
            ":: restarting {} for {user}",
            units.iter().cloned().collect::<Vec<_>>().join(" ")
        );
        let mut cmd = systemctl(Some(user));
        cmd.arg("try-restart").args(units);
        attempt(cmd, format!("systemctl --user try-restart for {user}"));
    }
    if !plan.left.is_empty() {
        eprintln!(
            ":: still using replaced files until you log out or reboot: {}",
            plan.left.iter().cloned().collect::<Vec<_>>().join(", ")
        );
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgroups_name_the_unit_or_session() {
        assert_eq!(owner("/init.scope"), Owner::Init);
        assert_eq!(
            owner("/system.slice/sshd.service"),
            Owner::Unit(None, "sshd.service".into())
        );
        assert_eq!(
            owner("/user.slice/user-1000.slice/user@1000.service/session.slice/pipewire.service"),
            Owner::Unit(Some(1000), "pipewire.service".into())
        );
        assert_eq!(
            owner("/user.slice/user-1000.slice/user@1000.service/init.scope"),
            Owner::UserManager(1000)
        );
        assert_eq!(owner("/user.slice/user-1000.slice/session-2.scope"), Owner::Session);
        assert_eq!(
            owner("/user.slice/user-1000.slice/user@1000.service/app.slice/app-firefox@1.scope"),
            Owner::Session
        );
    }

    #[test]
    fn the_desktop_and_logins_are_never_restarted() {
        let none = BTreeMap::new();
        assert!(is_protected("sddm.service", false, &none, "sddm.service"));
        assert!(is_protected("getty@tty1.service", false, &none, ""));
        assert!(!is_protected("cups.service", false, &none, "sddm.service"));
        assert!(is_protected("plasma-kwin_wayland.service", true, &none, ""));
        for network in [
            "systemd-networkd.service",
            "systemd-resolved.service",
            "iwd.service",
            "wpa_supplicant.service",
        ] {
            assert!(is_protected(network, false, &none, ""), "{network} can cut off ssh");
        }
        for media in [
            "pipewire.service",
            "pipewire-pulse.service",
            "wireplumber.service",
            "xdg-desktop-portal.service",
            "xdg-desktop-portal-kde.service",
        ] {
            assert!(
                is_protected(media, true, &none, ""),
                "restarting {media} breaks audio or screen sharing"
            );
        }
        assert!(!is_protected("syncthing.service", true, &none, ""));
        let overrides = BTreeMap::from([("sshd.service".to_string(), true), ("cups.service".to_string(), false)]);
        assert!(!is_protected("sshd.service", false, &overrides, ""));
        assert!(is_protected("cups.service", false, &overrides, ""));
    }
}

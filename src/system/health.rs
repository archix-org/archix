//! The checks a new deployment's counted boot must pass before boot-complete.target, run by archix-health-check.service; a failure marks its entry bad so the next boot starts the previous deployment.

use std::collections::BTreeSet;
use std::fs;
use std::net::{Ipv4Addr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::User;
use crate::paths::Paths;

/// `boot.health_check`: what a new deployment's boot must reach, beyond boot-complete.target, to count as good.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HealthCheck {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub units: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub listening: Vec<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reachable: Vec<String>,
    #[serde(default)]
    pub login: bool,
    #[serde(default)]
    pub timeout: u32,
    #[serde(default)]
    pub reboot_on_failure: bool,
}

impl HealthCheck {
    pub fn is_set(&self) -> bool {
        !self.units.is_empty() || !self.listening.is_empty() || !self.reachable.is_empty() || self.login
    }
}

/// One thing a boot must reach, named as the notes name it.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Shells,
    Unit(String),
    Listening(u16),
    Reachable(String),
    Login(String),
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::Shells => write!(f, "\"declared users' login shells\""),
            Target::Unit(unit) => write!(f, "\"{unit} active\""),
            Target::Listening(port) => write!(f, "\"TCP port {port} listening\""),
            Target::Reachable(host) => write!(f, "\"{host} reachable\""),
            Target::Login(user) => write!(f, "\"{user} can log in\""),
        }
    }
}

/// The targets of a deployment's check: the declared users' shells always, then what `boot.health_check` lists.
pub fn targets(check: Option<&HealthCheck>, users: &std::collections::BTreeMap<String, User>) -> Vec<Target> {
    let mut targets = vec![Target::Shells];
    let Some(check) = check else {
        return targets;
    };
    targets.extend(check.units.iter().cloned().map(Target::Unit));
    targets.extend(check.listening.iter().copied().map(Target::Listening));
    targets.extend(check.reachable.iter().cloned().map(Target::Reachable));
    if check.login {
        targets.extend(
            users
                .iter()
                .filter(|(_, user)| !no_login(&user.shell))
                .map(|(name, _)| Target::Login(name.clone())),
        );
    }
    targets
}

/// Shells that mean an account isn't for logging in.
fn no_login(shell: &str) -> bool {
    matches!(
        Path::new(shell).file_name().and_then(|name| name.to_str()),
        Some("nologin" | "false")
    )
}

/// Declared users whose login shell is missing under `root` or not listed in its /etc/shells, which PAM and chsh refuse.
pub fn shell_problems(root: &Path, users: &std::collections::BTreeMap<String, User>) -> Vec<String> {
    let listed: BTreeSet<String> = fs::read_to_string(root.join("etc/shells"))
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect();
    let mut problems = Vec::new();
    for (name, user) in users {
        if no_login(&user.shell) {
            continue;
        }
        let path = root.join(user.shell.trim_start_matches('/'));
        if !path.is_file() {
            problems.push(format!("{name}'s shell {} doesn't exist", user.shell));
        } else if !listed.contains(&user.shell) {
            problems.push(format!("{name}'s shell {} isn't in /etc/shells", user.shell));
        }
    }
    problems
}

/// Local TCP ports in LISTEN state, from /proc/net/tcp or tcp6.
pub fn listening_ports(table: &str) -> BTreeSet<u16> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let local = fields.nth(1)?;
            let state = fields.nth(1)?;
            (state == "0A")
                .then(|| u16::from_str_radix(local.rsplit_once(':')?.1, 16).ok())
                .flatten()
        })
        .collect()
}

/// The default IPv4 gateway in /proc/net/route.
pub fn default_gateway(routes: &str) -> Option<Ipv4Addr> {
    routes.lines().skip(1).find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.get(1) != Some(&"00000000") || fields.get(7) != Some(&"00000000") {
            return None;
        }
        let raw = u32::from_str_radix(fields.get(2)?, 16).ok()?;
        (raw != 0).then(|| Ipv4Addr::from(raw.to_le_bytes()))
    })
}

/// Whether a TCP connection to `host:port` succeeds within a few seconds.
pub fn connects(address: &str) -> Result<()> {
    let addresses: Vec<_> = address
        .to_socket_addrs()
        .with_context(|| format!("resolving {address}"))?
        .collect();
    let mut last = None;
    for socket in addresses {
        match TcpStream::connect_timeout(&socket, Duration::from_secs(3)) {
            Ok(_) => return Ok(()),
            Err(e) => last = Some(e),
        }
    }
    match last {
        Some(e) => Err(e).with_context(|| format!("connecting to {address}")),
        None => anyhow::bail!("{address} has no address"),
    }
}

fn quiet(command: &mut Command) -> Result<()> {
    let output = command
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {command:?}"))?;
    if output.status.success() {
        return Ok(());
    }
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    anyhow::bail!("{}", text.trim())
}

/// Whether a reachable target is host:port ([v6]:port), not a bare host to ping.
fn has_port(target: &str) -> bool {
    target.rsplit_once(':').is_some_and(|(host, port)| {
        port.parse::<u16>().is_ok() && (!host.contains(':') || host.starts_with('[') && host.ends_with(']'))
    })
}

/// Checks one target on the running system under `root`.
pub fn check(root: &Path, target: &Target, users: &std::collections::BTreeMap<String, User>) -> Result<()> {
    match target {
        Target::Shells => match shell_problems(root, users).as_slice() {
            [] => Ok(()),
            problems => anyhow::bail!("{}", problems.join("; ")),
        },
        Target::Unit(unit) => quiet(Command::new("systemctl").args(["is-active", "--quiet", unit]))
            .map_err(|_| anyhow::anyhow!("{unit} isn't active")),
        Target::Listening(port) => {
            let mut ports = BTreeSet::new();
            for table in ["proc/net/tcp", "proc/net/tcp6"] {
                ports.extend(listening_ports(
                    &fs::read_to_string(root.join(table)).unwrap_or_default(),
                ));
            }
            match ports.contains(port) {
                true => Ok(()),
                false => anyhow::bail!("nothing listens on TCP port {port}"),
            }
        }
        Target::Reachable(host) if host == "gateway" => {
            let routes = fs::read_to_string(root.join("proc/net/route")).unwrap_or_default();
            let gateway = default_gateway(&routes).context("there's no default gateway")?;
            quiet(Command::new("ping").args(["-c", "1", "-W", "2", &gateway.to_string()]))
        }
        Target::Reachable(host) if has_port(host) => connects(host),
        Target::Reachable(host) => quiet(Command::new("ping").args(["-c", "1", "-W", "2", host])),
        Target::Login(user) => {
            let shell = users.get(user).map_or("/bin/sh", |u| u.shell.as_str());
            quiet(Command::new("su").args(["-s", shell, user, "-c", "true"]))
        }
    }
}

/// Retries the targets every two seconds until all pass or `timeout` runs out; the first that still fails, and why.
pub fn run(
    root: &Path,
    targets: &[Target],
    users: &std::collections::BTreeMap<String, User>,
    timeout: u32,
) -> Option<(Target, String)> {
    let deadline = Instant::now() + Duration::from_secs(timeout.into());
    let mut left: Vec<&Target> = targets.iter().collect();
    loop {
        let mut failed = None;
        left.retain(|target| match check(root, target, users) {
            Ok(()) => false,
            Err(e) => {
                failed.get_or_insert_with(|| ((*target).clone(), format!("{e:#}")));
                true
            }
        });
        let failed = failed?;
        let now = Instant::now();
        if now >= deadline {
            return Some(failed);
        }
        std::thread::sleep((deadline - now).min(Duration::from_secs(2)));
    }
}

/// A health check that failed, kept until the fallback boot says why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Failure {
    pub generation: u32,
    pub target: String,
    pub why: String,
    pub at: u64,
}

pub fn failure_path(paths: &Paths) -> PathBuf {
    paths.state.join("boot").join("health-failed.json")
}

pub fn read_failure(paths: &Paths, generation: u32) -> Option<Failure> {
    let failure: Failure = serde_json::from_str(&fs::read_to_string(failure_path(paths)).ok()?).ok()?;
    (failure.generation == generation).then_some(failure)
}

/// A unix time as UTC, to the second.
pub fn utc(secs: u64) -> String {
    let date = crate::date::Date::from_unix(secs);
    let day = secs % 86_400;
    format!("{date} {:02}:{:02}:{:02} UTC", day / 3600, day / 60 % 60, day % 60)
}

/// What a fallback caused by a failed check says at login and in `archix status`.
pub fn fallback_note(from: u32, to: u32, failure: &Failure) -> String {
    format!(
        "archix: deployment {from} fell back to {to} because the health check {} failed at {} ({}); switch again once it passes",
        failure.target,
        utc(failure.at),
        failure.why
    )
}

/// archix-health-check.service: checks the booted deployment while systemd-boot is still counting its entry, and on failure marks the entry bad, notes why, and reboots if asked to.
pub fn run_boot(paths: &Paths) -> Result<bool> {
    let Some(booted) = super::boot::booted_deployment(&paths.root) else {
        println!("archix: this boot isn't from an archix entry; nothing to check");
        return Ok(true);
    };
    let esp = fs::read_to_string(paths.state.join("boot").join("esp")).context("reading the ESP's path")?;
    let esp = Path::new(esp.trim());
    if !super::boot::counting(esp, booted) {
        println!("archix: deployment {booted}'s entry already counts as good; nothing to check");
        return Ok(true);
    }
    let generation = crate::generations::Store::new(paths).get(booted)?;
    let check = generation
        .settings
        .boot
        .as_ref()
        .and_then(|boot| boot.health_check.as_ref());
    let users = &generation.system.users;
    let targets = targets(check, users);
    let timeout = check.filter(|check| check.is_set()).map_or(0, |check| check.timeout);
    let Some((target, why)) = run(&paths.root, &targets, users, timeout) else {
        println!("archix: deployment {booted} passed its health check");
        return Ok(true);
    };
    let failure = Failure {
        generation: booted,
        target: target.to_string(),
        why,
        at: crate::generations::now(),
    };
    let path = failure_path(paths);
    fs::create_dir_all(path.parent().context("the record has a directory")?)?;
    crate::durable::write(&path, serde_json::to_string_pretty(&failure)?.as_bytes())?;
    let marked = super::boot::mark_bad(esp, booted)?;
    let reboot = check.is_some_and(|check| check.reboot_on_failure);
    let note = format!(
        "archix: deployment {booted} failed its health check {} ({}); {}",
        failure.target,
        failure.why,
        match (marked, reboot) {
            (true, true) => "its entry is marked bad, and the machine reboots into the previous deployment now",
            (true, false) => "its entry is marked bad, so the next boot starts the previous deployment",
            (false, _) => "but its entry couldn't be marked bad, so the next boot starts it again",
        }
    );
    println!("{note}");
    let mut notes = fs::read_to_string(paths.notes()).unwrap_or_default();
    notes.push_str(&note);
    notes.push('\n');
    let _ = fs::write(paths.notes(), notes);
    if marked && reboot {
        let _ = Command::new("systemctl").args(["--no-block", "reboot"]).status();
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn user(shell: &str) -> User {
        User {
            uid: None,
            group: None,
            groups: Vec::new(),
            shell: shell.into(),
            home: None,
            description: String::new(),
            system: false,
        }
    }

    #[test]
    fn a_declared_shell_must_exist_and_be_listed_in_etc_shells() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("usr/bin")).unwrap();
        fs::create_dir_all(root.join("etc")).unwrap();
        fs::write(root.join("usr/bin/bash"), "").unwrap();
        fs::write(root.join("usr/bin/zsh"), "").unwrap();
        fs::write(root.join("etc/shells"), "# shells\n/usr/bin/bash\n").unwrap();
        let users: BTreeMap<String, User> = [
            ("ann".to_string(), user("/usr/bin/bash")),
            ("bob".to_string(), user("/usr/bin/zsh")),
            ("cat".to_string(), user("/usr/bin/fish")),
            ("svc".to_string(), user("/usr/bin/nologin")),
        ]
        .into();
        assert_eq!(
            shell_problems(root, &users),
            [
                "bob's shell /usr/bin/zsh isn't in /etc/shells",
                "cat's shell /usr/bin/fish doesn't exist"
            ]
        );
        let check = HealthCheck {
            login: true,
            ..Default::default()
        };
        let named: Vec<String> = targets(Some(&check), &users).iter().map(Target::to_string).collect();
        assert_eq!(
            named,
            [
                "\"declared users' login shells\"",
                "\"ann can log in\"",
                "\"bob can log in\"",
                "\"cat can log in\""
            ]
        );
        assert_eq!(
            targets(None, &users),
            [Target::Shells],
            "the shells are checked by default"
        );
    }

    #[test]
    fn listening_ports_and_the_default_gateway_come_from_proc() {
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   \
                   0: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1 1\n   \
                   1: 0100007F:1F90 0100007F:D431 01 00000000:00000000 00:00000000 00000000     0        0 2 1\n";
        assert_eq!(listening_ports(tcp), BTreeSet::from([22]));
        let routes = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
                      enp0s2\t00000000\t0202000A\t0003\t0\t0\t1024\t00000000\t0\t0\t0\n\
                      enp0s2\t0002000A\t00000000\t0001\t0\t0\t1024\t00FFFFFF\t0\t0\t0\n";
        assert_eq!(default_gateway(routes), Some(Ipv4Addr::new(10, 0, 2, 2)));
        assert_eq!(default_gateway("Iface\tDestination\n"), None);
        assert!(has_port("example.org:443") && has_port("10.0.2.2:8079") && has_port("[fd00::1]:22"));
        assert!(!has_port("example.org") && !has_port("fd00::1") && !has_port("gateway"));
    }

    #[test]
    fn a_target_is_retried_until_the_timeout_and_the_first_failure_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let open = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
        let closed = {
            let gone = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("127.0.0.1:{}", gone.local_addr().unwrap().port())
        };
        let users = BTreeMap::new();
        assert_eq!(run(dir.path(), &[Target::Reachable(open.clone())], &users, 0), None);
        let started = Instant::now();
        let (target, why) = run(
            dir.path(),
            &[Target::Reachable(open), Target::Reachable(closed.clone())],
            &users,
            3,
        )
        .expect("a closed port can't pass");
        assert_eq!(target, Target::Reachable(closed));
        assert!(why.contains("connecting"), "{why}");
        assert!(
            started.elapsed() >= Duration::from_secs(3),
            "it retried until the timeout"
        );
    }

    #[test]
    fn a_fallback_note_names_the_target_and_the_time() {
        let failure = Failure {
            generation: 7,
            target: Target::Listening(22).to_string(),
            why: "nothing listens on TCP port 22".into(),
            at: 1_792_576_985,
        };
        assert_eq!(
            fallback_note(7, 6, &failure),
            "archix: deployment 7 fell back to 6 because the health check \"TCP port 22 listening\" failed at 2026-10-21 10:03:05 UTC (nothing listens on TCP port 22); switch again once it passes"
        );
    }
}

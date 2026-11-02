//! Evaluating a captured config tree in a child process: a transient uid nobody else runs as, no_new_privs, and limits on CPU, memory and time; it hands back only the evaluated config as a versioned plan.
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::value::Value;

/// The plan's format: every newer archix reads every older one.
pub const PLAN_FORMAT: u32 = 1;
/// The range systemd hands DynamicUser= units from, where nothing has a passwd entry of its own.
const TRANSIENT: std::ops::RangeInclusive<u32> = 61184..=65519;

#[derive(Debug, Clone)]
pub struct Limits {
    pub cpu_seconds: u64,
    pub memory_bytes: u64,
    pub wall: Duration,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            cpu_seconds: 60,
            memory_bytes: 2 << 30,
            wall: Duration::from_secs(120),
        }
    }
}

/// The evaluated config as a plan: what root renders and applies, and what a generation stores.
pub fn plan_json(config: &Value) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "format": PLAN_FORMAT, "config": config })).expect("a Value serializes")
}

/// Reads a plan any archix wrote: format 1 and every older one there is.
pub fn read_plan(bytes: &[u8]) -> Result<Value> {
    let json: serde_json::Value = serde_json::from_slice(bytes).context("reading the plan")?;
    let format = json.get("format").and_then(serde_json::Value::as_u64).unwrap_or(1);
    if format > PLAN_FORMAT as u64 {
        bail!("the plan is format {format}, newer than this archix reads ({PLAN_FORMAT}); update archix");
    }
    Ok(from_json(json.get("config").context("the plan has no config")?))
}

pub fn from_json(json: &serde_json::Value) -> Value {
    match json {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => Value::Int(i),
            None => Value::Float(n.as_f64().unwrap_or(0.0)),
        },
        serde_json::Value::String(s) => Value::Str(s.clone()),
        serde_json::Value::Array(items) => Value::List(items.iter().map(from_json).collect()),
        serde_json::Value::Object(map) => Value::Map(map.iter().map(|(k, v)| (k.clone(), from_json(v))).collect()),
    }
}

/// An uid and gid in systemd's transient range that no account and no process has.
pub fn transient_id() -> Result<u32> {
    let span = TRANSIENT.end() - TRANSIENT.start() + 1;
    let start = std::process::id() % span;
    let running = running_uids();
    for offset in 0..span {
        let id = TRANSIENT.start() + (start + offset) % span;
        let free = unsafe { libc::getpwuid(id).is_null() && libc::getgrgid(id).is_null() };
        if free && !running.contains(&id) {
            return Ok(id);
        }
    }
    bail!("no free uid in {TRANSIENT:?} to evaluate the config as")
}

fn running_uids() -> std::collections::BTreeSet<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.metadata().ok().map(|meta| meta.uid()))
        .collect()
}

/// Evaluates `dir` for `hostname` in a child under a transient uid, with `overlay`'s files instead of the tree's, and returns the evaluated config.
pub fn evaluate(dir: &Path, hostname: &str, overlay: &BTreeMap<PathBuf, String>, limits: &Limits) -> Result<Value> {
    let exe = crate::testhooks::own_exe().context("finding archix's own binary")?;
    let id = transient_id()?;
    let mut command = Command::new(exe);
    command
        .env_clear()
        .env("PATH", "/usr/bin")
        .env("LC_ALL", "C")
        .arg("evaluate-isolated")
        .arg("--dir")
        .arg(dir)
        .arg("--host")
        .arg(hostname)
        .current_dir("/")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (cpu, memory) = (limits.cpu_seconds, limits.memory_bytes);
    unsafe {
        command.pre_exec(move || {
            for (resource, value) in [
                (libc::RLIMIT_CPU, cpu),
                (libc::RLIMIT_AS, memory),
                (libc::RLIMIT_CORE, 0),
            ] {
                let limit = libc::rlimit {
                    rlim_cur: value,
                    rlim_max: value,
                };
                if libc::setrlimit(resource, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    if unsafe { libc::geteuid() } == 0 {
        crate::captured::drop_to(&mut command, id, id, vec![id]);
    } else {
        crate::captured::drop_to(
            &mut command,
            unsafe { libc::geteuid() },
            unsafe { libc::getegid() },
            Vec::new(),
        );
    }
    let mut child = command.spawn().context("starting the config's evaluation")?;
    let input = serde_json::to_vec(&serde_json::json!({ "overlay": overlay }))?;
    let mut stdin = child.stdin.take().context("the evaluation's stdin")?;
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let mut stdout = child.stdout.take().context("the evaluation's stdout")?;
    let mut stderr = child.stderr.take().context("the evaluation's stderr")?;
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        bytes
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > limits.wall {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "evaluating the config took longer than {} s, so it was stopped; nothing was changed",
                limits.wall.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let _ = writer.join();
    let stdout = out.join().unwrap_or_default();
    let stderr = String::from_utf8_lossy(&err.join().unwrap_or_default())
        .trim()
        .to_string();
    if !status.success() {
        use std::os::unix::process::ExitStatusExt;
        let why = match status.signal() {
            Some(libc::SIGXCPU) | Some(libc::SIGKILL) => {
                format!("it used more than {} s of CPU, so it was stopped", limits.cpu_seconds)
            }
            Some(signal) => format!(
                "it was killed by signal {signal}, perhaps for using more than {} MiB",
                limits.memory_bytes >> 20
            ),
            None if stderr.is_empty() => format!("it exited with {status}"),
            None => stderr,
        };
        bail!("{why}");
    }
    read_plan(&stdout)
}

/// `archix evaluate-isolated`, already dropped to the transient uid: evaluates DIR with the overlay on stdin and prints the plan.
pub fn child(dir: &Path, hostname: &str) -> Result<bool> {
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input)?;
    let input: serde_json::Value = serde_json::from_slice(&input).unwrap_or_default();
    let overlay = input
        .get("overlay")
        .and_then(serde_json::Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(path, text)| Some((PathBuf::from(path), text.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    match super::evaluate(&super::Input {
        config_dir: dir.to_path_buf(),
        hostname: hostname.to_string(),
        overlay,
    }) {
        Ok(evaluation) => {
            std::io::stdout().write_all(&plan_json(&evaluation.config))?;
            Ok(true)
        }
        Err(errors) => {
            eprintln!("{errors}");
            Ok(false)
        }
    }
}

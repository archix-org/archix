//! archix's pacman hooks, shipped beside the packages' own in /usr/share/libalpm/hooks: strict mode's guard, the snapshot before a plain transaction with its space estimate, and the step after it that rebuilds the boot image and resets boot counting.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Where libalpm reads package-shipped hooks, below the root; a hook of the same name in a HookDir (/etc/pacman.d/hooks) replaces one here.
pub const SYSTEM_DIR: &str = "/usr/share/libalpm/hooks";
/// Notes pacman's PID when a transaction removes archix, so strict mode lets that transaction through.
pub const REMOVAL: &str = "00-archix-removal.hook";
pub const STRICT: &str = "00-archix-strict.hook";
/// After the strict hook and before every other pre-transaction hook, mkinitcpio's removal hook (60-) included, so the snapshot holds the system untouched.
pub const SNAPSHOT: &str = "01-archix-snapshot.hook";
/// After DKMS (70-) and mkinitcpio (90-), so the image is built from what they left.
pub const POST: &str = "95-archix-post.hook";
pub const NAMES: [&str; 4] = [REMOVAL, STRICT, SNAPSHOT, POST];
/// The strict hook's script: plain sh, so pacman never depends on archix running.
pub const STRICT_SCRIPT: &str = "/usr/lib/archix/strict-hook.sh";
/// In archix's state: present while strict mode is on, holding the config directory it was turned on from.
pub const STRICT_FLAG: &str = "strict";
/// pacman's PID while a transaction removing archix runs.
pub const REMOVING: &str = "/run/archix/removing";
/// The snapshot a keep made the root from the next boot, until that boot; /run is gone by then.
pub const KEEP_PENDING: &str = "/run/archix/keep-pending";
/// What the pre-transaction step learnt for the step after, keyed by pacman's PID.
pub const TRANSACTION: &str = "/run/archix/pacman-transaction.json";
/// Recent actions the hooks took when space ran short, which `archix status` shows.
pub const EVENTS: &str = "pacman-events.json";

const PACKAGE_TRIGGER: &str =
    "[Trigger]\nOperation = Install\nOperation = Upgrade\nOperation = Remove\nType = Package\nTarget = *\n\n";

/// Every hook file and the strict script at their absolute paths, for the archix at `archix` with its state in `state`.
pub fn files(archix: &Path, state: &Path) -> Vec<(PathBuf, String)> {
    let archix = archix.display();
    let dir = Path::new(SYSTEM_DIR);
    let ours: Vec<String> = NAMES
        .iter()
        .map(|name| dir.join(name).display().to_string())
        .chain([STRICT_SCRIPT.to_string()])
        .collect();
    vec![
        (
            dir.join(REMOVAL),
            format!(
                "[Trigger]\nOperation = Remove\nType = Package\nTarget = archix\nTarget = archix-git\nTarget = archix-bin\n\n\
                 [Action]\nDescription = archix: noting that this transaction removes archix...\nWhen = PreTransaction\n\
                 Exec = /bin/sh -c 'mkdir -p {run} && echo $PPID > {REMOVING}; [ ! -x {STATIC} ] || cp {STATIC} {COPY}; exit 0'\n",
                run = Path::new(REMOVING).parent().unwrap_or(Path::new("/run")).display(),
                STATIC = crate::integration::STATIC_BINARY,
                COPY = crate::system::removal::COPY,
            ),
        ),
        (
            dir.join(STRICT),
            format!(
                "{PACKAGE_TRIGGER}[Action]\nDescription = archix: checking strict mode...\nWhen = PreTransaction\n\
                 Exec = /bin/sh -c '[ -f {STRICT_SCRIPT} ] || exit 0; exec /bin/sh {STRICT_SCRIPT}'\nAbortOnFail\n"
            ),
        ),
        (
            dir.join(SNAPSHOT),
            format!(
                "{PACKAGE_TRIGGER}[Action]\nDescription = archix: snapshotting the root before this transaction...\nWhen = PreTransaction\n\
                 Exec = /bin/sh -c '[ -x {archix} ] || exit 0; exec {archix} pacman-hook pre'\nAbortOnFail\nNeedsTargets\n"
            ),
        ),
        (
            dir.join(POST),
            format!(
                "{PACKAGE_TRIGGER}[Action]\nDescription = archix: recording the transaction and updating the boot menu...\nWhen = PostTransaction\n\
                 Exec = /bin/sh -c '[ -x {archix} ] && exec {archix} pacman-hook post; [ -x {copy} ] && {copy} pacman-hook remove < /dev/null; rm -f {} {copy}'\nNeedsTargets\n",
                ours.join(" "),
                copy = crate::system::removal::COPY
            ),
        ),
        (PathBuf::from(STRICT_SCRIPT), strict_script(&archix.to_string(), state)),
    ]
}

/// The strict hook's script: refuses while a keep waits for its reboot, and while strict mode is on unless archix runs the transaction or it removes archix; lets everything through once archix, its strict flag or the config that turned it on is gone.
fn strict_script(archix: &str, state: &Path) -> String {
    format!(
        r#"#!/bin/sh
[ -x {archix} ] || exit 0
pid=$(cat {REMOVING} 2>/dev/null)
[ -n "$pid" ] && [ "$pid" = "$PPID" ] && [ -d "/proc/$pid" ] && exit 0
if [ -s {KEEP_PENDING} ]; then
  n=$(cat {KEEP_PENDING})
  echo "archix: rollback to snapshot $n is pending; reboot, or \`archix rollback --cancel\`" >&2
  exit 1
fi
flag={flag}
[ -f "$flag" ] || exit 0
[ -n "$ARCHIX_TRANSACTION" ] && exit 0
config=$(cat "$flag")
[ -z "$config" ] || [ -f "$config/init.lua" ] || exit 0
echo "archix: strict mode is on, so packages change only through archix (\`archix install\`, \`archix remove\`, \`archix upgrade\`); set strict = false and switch to turn it off, or remove archix with pacman -R archix" >&2
exit 1
"#,
        flag = state.join(STRICT_FLAG).display()
    )
}

/// What's wrong with archix's hooks below `root`: masked by a /dev/null link, shadowed by a file of the same name in a HookDir, missing or changed; `wanted` is what archix writes, by path.
pub fn trouble(root: &Path, wanted: &[(PathBuf, String)]) -> Vec<String> {
    let at = |path: &Path| root.join(path.strip_prefix("/").unwrap_or(path));
    let mut found = Vec::new();
    let hookdirs = hook_dirs(&at(Path::new("/etc/pacman.conf")));
    for name in NAMES {
        let system = Path::new(SYSTEM_DIR).join(name);
        let what = match name {
            STRICT | REMOVAL => "strict mode and a pending rollback stop nothing",
            SNAPSHOT => "plain pacman runs take no snapshot",
            _ => "plain pacman runs don't update the boot menu or reset boot counting",
        };
        let shadow = hookdirs
            .iter()
            .map(|dir| dir.join(name))
            .find(|path| fs::symlink_metadata(at(path)).is_ok());
        if let Some(path) = shadow {
            let how = match masked(&at(&path)) {
                true => "masked by",
                false => "shadowed by",
            };
            found.push(format!(
                "archix's pacman hook {name} is {how} {}, so {what}; remove that file",
                path.display()
            ));
            continue;
        }
        if masked(&at(&system)) {
            found.push(format!(
                "archix's pacman hook {} is a link to /dev/null, so {what}; the next switch writes it again",
                system.display()
            ));
            continue;
        }
        let expected = wanted.iter().find(|(path, _)| *path == system).map(|(_, text)| text);
        match fs::read_to_string(at(&system)) {
            Err(_) => found.push(format!(
                "archix's pacman hook {} is missing, so {what}; the next switch writes it again",
                system.display()
            )),
            Ok(text) if expected.is_some_and(|expected| *expected != text) => found.push(format!(
                "archix's pacman hook {} isn't what archix wrote, so it may not run as it should; the next switch writes it again",
                system.display()
            )),
            Ok(_) => {}
        }
    }
    found
}

fn masked(path: &Path) -> bool {
    fs::read_link(path).is_ok_and(|target| target == Path::new("/dev/null"))
}

/// The HookDir directories pacman.conf at `conf` names, /etc/pacman.d/hooks when it names none.
fn hook_dirs(conf: &Path) -> Vec<PathBuf> {
    let text = fs::read_to_string(conf).unwrap_or_default();
    let mut dirs: Vec<PathBuf> = text
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key.trim() == "HookDir").then(|| PathBuf::from(value.trim()))
        })
        .collect();
    if dirs.is_empty() {
        dirs.push(PathBuf::from("/etc/pacman.d/hooks"));
    }
    dirs
}

/// What a pacman process's command line says about its transaction.
#[derive(Debug, Default, PartialEq)]
pub struct Invocation {
    /// A -R transaction, which installs nothing.
    pub removes: bool,
    pub noconfirm: bool,
    /// Package files given to -U.
    pub files: Vec<PathBuf>,
}

impl Invocation {
    /// From the NUL-separated command line in /proc/PID/cmdline; a front end that isn't pacman (pamac) gives an empty one.
    pub fn parse(cmdline: &[u8]) -> Invocation {
        let args: Vec<String> = cmdline
            .split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect();
        let is_pacman = args
            .first()
            .is_some_and(|arg0| Path::new(arg0).file_name().is_some_and(|name| name == "pacman"));
        if !is_pacman {
            return Invocation::default();
        }
        let op = |short: char, long: &str| {
            args.iter().skip(1).any(|arg| {
                arg == long || (arg.starts_with('-') && !arg.starts_with("--") && arg[1..].starts_with(short))
            })
        };
        Invocation {
            removes: op('R', "--remove"),
            noconfirm: args.iter().any(|arg| arg == "--noconfirm"),
            files: args
                .iter()
                .skip(1)
                .filter(|arg| !arg.starts_with('-') && arg.contains(".pkg.tar"))
                .map(PathBuf::from)
                .collect(),
        }
    }
}

/// The bytes the transaction's packages take installed, not less what they replace, which the snapshot keeps: each target's size in the sync databases (the largest, if several repos have it), else in a package file the command line names, else its installed size; nothing for a removal.
pub fn estimate(db: &Path, targets: &[String], invocation: &Invocation) -> u64 {
    if invocation.removes {
        return 0;
    }
    let mut synced: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    for file in fs::read_dir(db.join("sync")).into_iter().flatten().flatten() {
        let path = file.path();
        if path.extension().is_none_or(|ext| ext != "db") {
            continue;
        }
        for (name, pkg) in super::read_sync_db(&path).unwrap_or_default() {
            let size = synced.entry(name).or_default();
            *size = (*size).max(pkg.installed_size);
        }
    }
    let from_files: Vec<(String, u64)> = invocation.files.iter().filter_map(|file| file_size(file)).collect();
    targets
        .iter()
        .map(|name| {
            if let Some((_, size)) = from_files.iter().find(|(file_name, _)| file_name == name) {
                return *size;
            }
            if let Some(size) = synced.get(name) {
                return *size;
            }
            installed_size(&db.join("local"), name).unwrap_or(0)
        })
        .sum()
}

/// A package file's name and installed size, from its .PKGINFO.
fn file_size(path: &Path) -> Option<(String, u64)> {
    use std::io::Read;
    let file = fs::File::open(path).ok()?;
    let mut archive = tar::Archive::new(zstd::Decoder::new(file).ok()?);
    for entry in archive.entries().ok()? {
        let mut entry = entry.ok()?;
        if entry.path().ok()?.as_os_str() != ".PKGINFO" {
            continue;
        }
        let mut text = String::new();
        entry.read_to_string(&mut text).ok()?;
        let value = |key: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(key)?.strip_prefix(" = "))
        };
        return Some((value("pkgname")?.to_string(), value("size")?.parse().ok()?));
    }
    None
}

fn installed_size(local: &Path, name: &str) -> Option<u64> {
    let dir = local_entry(local, name)?;
    let text = fs::read_to_string(dir.join("desc")).ok()?;
    super::parse_desc(&text).get("SIZE")?.first()?.parse().ok()
}

/// The local database directory of installed package `name`.
fn local_entry(local: &Path, name: &str) -> Option<PathBuf> {
    fs::read_dir(local)
        .ok()?
        .flatten()
        .find(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|dir| super::name_from_entry(dir) == name)
        })
        .map(|entry| entry.path())
}

/// Room kept beyond the estimate: libalpm's own check adds min(5% of the filesystem, 20 MiB), and archix's estimate misses what scriptlets and other hooks write.
pub fn margin(estimate: u64) -> u64 {
    (64 << 20).max(estimate / 10)
}

/// How the root stands once the transaction's files and its margin are written.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verdict {
    /// Above rollback.min_free and the keep reserve.
    Fine,
    /// Below them; still trying is offered only when it stays above the keep reserve.
    Short { still_try: bool },
}

/// Whether `needed` bytes fit on a root with `space` free, above `min_free` percent and the reserve for one keep.
pub fn verdict(space: crate::snapshot::Space, needed: u64, min_free: u32) -> Verdict {
    let left = after(space, needed);
    match left.short(min_free) {
        false => Verdict::Fine,
        true => Verdict::Short {
            still_try: left.bytes >= crate::snapshot::KEEP_RESERVE,
        },
    }
}

/// `space` less `needed` bytes.
pub fn after(space: crate::snapshot::Space, needed: u64) -> crate::snapshot::Space {
    let total = match space.percent > 0.0 {
        true => space.bytes as f64 * 100.0 / space.percent,
        false => 0.0,
    };
    let bytes = space.bytes.saturating_sub(needed);
    crate::snapshot::Space {
        percent: match total > 0.0 {
            true => bytes as f64 * 100.0 / total,
            false => 0.0,
        },
        bytes,
    }
}

/// What to do short of space with no terminal to ask: rollback.low_space. Prune never drops the last safe recovery point, and aborts instead when there is none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LowSpace {
    Abort,
    #[default]
    Prune,
    /// Let the transaction run without a snapshot.
    Skip,
}

impl LowSpace {
    pub fn parse(text: &str) -> Option<LowSpace> {
        match text {
            "abort" => Some(LowSpace::Abort),
            "prune" => Some(LowSpace::Prune),
            "skip" => Some(LowSpace::Skip),
            _ => None,
        }
    }
}

/// libalpm's own Path triggers for rebuilding the initramfs, from mkinitcpio's install hook below `root`, or the ones it shipped with when it isn't there.
pub fn initramfs_triggers(root: &Path) -> Vec<String> {
    let hook = root
        .join(SYSTEM_DIR.trim_start_matches('/'))
        .join("90-mkinitcpio-install.hook");
    let mut triggers = Vec::new();
    let mut path_section = false;
    for line in fs::read_to_string(hook).unwrap_or_default().lines() {
        let line = line.trim();
        if line.starts_with('[') {
            path_section = false;
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "Type" => path_section = value.trim() == "Path",
            "Target" if path_section => triggers.push(value.trim().to_string()),
            _ => {}
        }
    }
    if triggers.is_empty() {
        triggers = [
            "usr/lib/modules/*/vmlinuz",
            "usr/lib/initcpio/*",
            "usr/lib/firmware/*",
            "usr/lib/modules/*/extramodules/",
            "usr/src/*/dkms.conf",
            "usr/lib/systemd/systemd",
            "usr/lib/systemd/systemd-udevd",
            "usr/bin/cryptsetup",
            "usr/lib/modprobe.d/",
        ]
        .map(str::to_string)
        .to_vec();
    }
    triggers.extend(["boot/*-ucode.img".to_string(), "usr/bin/mkinitcpio".to_string()]);
    triggers
}

/// Whether any installed target in `local` (pacman's local database) owns a file one of `triggers` matches, as libalpm matches them; a target that isn't installed counts when `missing` is set.
pub fn touches_initramfs(local: &Path, targets: &[String], triggers: &[String], missing: bool) -> bool {
    targets.iter().any(|name| {
        let Some(dir) = local_entry(local, name) else {
            return missing;
        };
        let text = fs::read_to_string(dir.join("files")).unwrap_or_default();
        text.lines()
            .skip_while(|line| *line != "%FILES%")
            .skip(1)
            .take_while(|line| !line.is_empty())
            .any(|file| triggers.iter().any(|pattern| fnmatch(pattern, file)))
    })
}

fn fnmatch(pattern: &str, path: &str) -> bool {
    let (Ok(pattern), Ok(path)) = (std::ffi::CString::new(pattern), std::ffi::CString::new(path)) else {
        return false;
    };
    unsafe { libc::fnmatch(pattern.as_ptr(), path.as_ptr(), 0) == 0 }
}

/// What the pre-transaction step left for the step after.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Pending {
    pub pacman: u32,
    /// Before the transaction, a target owned a file the initramfs is built from.
    pub initramfs: bool,
}

/// Reads what the pre-transaction step of pacman `pacman`'s transaction left, and forgets it.
pub fn take_pending(root: &Path, pacman: u32) -> Option<Pending> {
    let path = root.join(TRANSACTION.trim_start_matches('/'));
    let pending: Pending = serde_json::from_str(&fs::read_to_string(&path).ok()?).ok()?;
    let _ = fs::remove_file(&path);
    (pending.pacman == pacman).then_some(pending)
}

pub fn save_pending(root: &Path, pending: &Pending) -> Result<()> {
    let path = root.join(TRANSACTION.trim_start_matches('/'));
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    fs::write(&path, serde_json::to_string(pending)?).with_context(|| format!("writing {}", path.display()))
}

/// In archix's per-root state from the step before a plain transaction, once the slot has its entry, until the step after it has flushed the root: a boot that finds it was cut off mid-transaction, and doesn't count as good.
pub const UNFINISHED: &str = "pacman-unfinished.json";

/// A plain transaction that started and whose step after it hasn't finished.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Unfinished {
    pub pacman: u32,
    pub started: u64,
    /// The fallback slot when it started, which predates it.
    pub slot: Option<u32>,
    /// The generation a switch was applying, when archix's own switch set the mark rather than a plain transaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch: Option<u32>,
}

impl Unfinished {
    /// Why a boot that finds this doesn't count as good, and what to do.
    pub fn why(&self) -> String {
        let slot = match self.slot {
            Some(slot) => format!(
                "; the fallback slot, snapshot {slot}, is the system from before it: `archix rollback {slot}` keeps it"
            ),
            None => String::new(),
        };
        match self.switch {
            Some(number) => format!(
                "the switch to generation {number} started at {} was cut off before it finished, so the packages and files it was changing may be torn{slot}, or boot this entry again and run `archix switch` again",
                crate::system::health::utc(self.started)
            ),
            None => format!(
                "the pacman transaction started at {} was cut off before the step after it finished, so the files it was changing may be torn{slot}, or boot this entry again and finish the update with pacman",
                crate::system::health::utc(self.started)
            ),
        }
    }

    /// The health check's name for what failed.
    pub fn target(&self) -> &'static str {
        match self.switch {
            Some(_) => "switch",
            None => "pacman-transaction",
        }
    }
}

/// Written only after the menu that falls back from it is on the ESP (menu-synced), by the step before a transaction and by a switch.
pub fn mark_unfinished(state: &Path, unfinished: &Unfinished) -> Result<()> {
    crate::durable::write(&state.join(UNFINISHED), serde_json::to_string(unfinished)?.as_bytes())?;
    crate::testhooks::mark("unfinished-marked");
    Ok(())
}

pub fn unfinished(state: &Path) -> Option<Unfinished> {
    serde_json::from_str(&fs::read_to_string(state.join(UNFINISHED)).ok()?).ok()
}

/// Drops the mark once the transaction and the step after it are on disk; the caller has flushed the root.
pub fn finished(state: &Path) -> Result<()> {
    let path = state.join(UNFINISHED);
    match fs::remove_file(&path) {
        Ok(()) => {
            crate::durable::sync_dir(&path)?;
            crate::testhooks::mark("unfinished-cleared");
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// One thing a hook did when space ran short.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub at: u64,
    pub text: String,
}

/// Notes `text` in the journal and in archix's state for `archix status`, keeping the newest ten.
pub fn log(state: &Path, text: &str) {
    let _ = std::process::Command::new("logger")
        .args(["-t", "archix", "--", text])
        .stderr(std::process::Stdio::null())
        .status();
    let mut events = events(state);
    events.push(Event {
        at: crate::generations::now(),
        text: text.to_string(),
    });
    let skip = events.len().saturating_sub(10);
    let events: Vec<Event> = events.into_iter().skip(skip).collect();
    if let Ok(json) = serde_json::to_string_pretty(&events) {
        let _ = crate::durable::write(&state.join(EVENTS), json.as_bytes());
    }
}

pub fn events(state: &Path) -> Vec<Event> {
    fs::read_to_string(state.join(EVENTS))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// The config directory as the strict script sees it from inside `root`, which pacman chroots its hooks into.
pub fn config_in_root(root: &Path, config_dir: &Path) -> PathBuf {
    match config_dir.strip_prefix(root) {
        Ok(inside) if root != Path::new("/") => Path::new("/").join(inside),
        _ => config_dir.to_path_buf(),
    }
}

/// Names the targets of a transaction that are still installed, for the boot-image check after it.
pub fn installed_targets(local: &Path, targets: &[String]) -> BTreeSet<String> {
    targets
        .iter()
        .filter(|name| local_entry(local, name).is_some())
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{KEEP_RESERVE, Space};

    #[test]
    fn the_hooks_sort_strict_first_then_the_snapshot_before_every_other_pre_transaction_hook() {
        let mut names = vec![
            "10-glibc-locale-gen.hook",
            "00-archix-try.hook",
            "60-mkinitcpio-remove.hook",
            "05-snap-pac-pre.hook",
            SNAPSHOT,
            STRICT,
            REMOVAL,
        ];
        names.sort();
        let at = |name: &str| names.iter().position(|n| *n == name).unwrap();
        assert!(at(REMOVAL) < at(STRICT) && at(STRICT) < at(SNAPSHOT), "{names:?}");
        assert!(at(SNAPSHOT) < at("05-snap-pac-pre.hook") && at(SNAPSHOT) < at("60-mkinitcpio-remove.hook"));
        let files = files(Path::new("/usr/bin/archix"), Path::new("/var/lib/archix"));
        let text = |name: &str| &files.iter().find(|(path, _)| path.ends_with(name)).unwrap().1;
        assert!(text(STRICT).contains("AbortOnFail") && text(SNAPSHOT).contains("AbortOnFail"));
        assert!(
            !text(POST).contains("AbortOnFail"),
            "nothing after a transaction can stop it"
        );
        assert!(text(SNAPSHOT).contains("|| exit 0; exec /usr/bin/archix pacman-hook pre"));
        assert!(text(POST).contains(&format!("rm -f /usr/share/libalpm/hooks/{REMOVAL}")));
        let copy = crate::system::removal::COPY;
        assert!(
            text(REMOVAL).contains(&format!("cp /usr/lib/archix/archix-static {copy}"))
                && !text(REMOVAL).contains("AbortOnFail"),
            "the removal keeps a copy of archix for the step after, and never stops pacman: {}",
            text(REMOVAL)
        );
        assert!(
            text(POST).contains(&format!("[ -x {copy} ] && {copy} pacman-hook remove < /dev/null")),
            "once archix is gone the step after runs the copy's cleanup: {}",
            text(POST)
        );
    }

    #[test]
    fn a_command_line_gives_removals_noconfirm_and_package_files() {
        let parse = |args: &[&str]| Invocation::parse(args.join("\0").as_bytes());
        assert_eq!(
            parse(&["pacman", "-Rns", "foo"]),
            Invocation {
                removes: true,
                ..Invocation::default()
            }
        );
        let upgrade = parse(&["/usr/bin/pacman", "-U", "--noconfirm", "/tmp/a-1-1-any.pkg.tar.zst"]);
        assert!(!upgrade.removes && upgrade.noconfirm);
        assert_eq!(upgrade.files, [PathBuf::from("/tmp/a-1-1-any.pkg.tar.zst")]);
        assert!(!parse(&["pacman", "-Syu"]).removes && !parse(&["pacman", "--sync", "--refresh"]).removes);
        assert_eq!(
            parse(&["pamac-daemon"]),
            Invocation::default(),
            "not pacman: nothing is known"
        );
    }

    #[test]
    fn short_means_below_the_floor_after_the_transaction_and_still_trying_keeps_the_keep_reserve() {
        let gib = 1u64 << 30;
        let space = Space {
            percent: 50.0,
            bytes: 10 * gib,
        };
        assert_eq!(verdict(space, gib, 10), Verdict::Fine);
        assert_eq!(
            verdict(space, 8 * gib, 30),
            Verdict::Short { still_try: true },
            "2 GiB of 20 left is under 30% but over the reserve"
        );
        assert_eq!(
            verdict(space, 10 * gib - KEEP_RESERVE / 2, 10),
            Verdict::Short { still_try: false }
        );
        let left = after(space, 5 * gib);
        assert!((left.percent - 25.0).abs() < 0.01 && left.bytes == 5 * gib, "{left:?}");
        assert_eq!(margin(0), 64 << 20);
        assert_eq!(margin(10 * gib), gib);
    }

    #[test]
    fn hooks_masked_shadowed_missing_or_changed_are_named() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let files = files(Path::new("/usr/bin/archix"), Path::new("/var/lib/archix"));
        for (path, text) in &files {
            let path = root.join(path.strip_prefix("/").unwrap());
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        assert!(trouble(root, &files).is_empty(), "{:?}", trouble(root, &files));
        let etc = root.join("etc/pacman.d/hooks");
        fs::create_dir_all(&etc).unwrap();
        std::os::unix::fs::symlink("/dev/null", etc.join(SNAPSHOT)).unwrap();
        fs::write(etc.join(STRICT), "[Trigger]\n").unwrap();
        let system = root.join(SYSTEM_DIR.trim_start_matches('/'));
        fs::remove_file(system.join(POST)).unwrap();
        std::os::unix::fs::symlink("/dev/null", system.join(POST)).unwrap();
        fs::write(system.join(REMOVAL), "edited").unwrap();
        let found = trouble(root, &files).join("\n");
        assert!(
            found.contains(&format!("{SNAPSHOT} is masked by /etc/pacman.d/hooks/{SNAPSHOT}")),
            "{found}"
        );
        assert!(
            found.contains(&format!("{STRICT} is shadowed by /etc/pacman.d/hooks/{STRICT}")),
            "{found}"
        );
        assert!(found.contains(&format!("{POST} is a link to /dev/null")), "{found}");
        assert!(found.contains(&format!("{REMOVAL} isn't what archix wrote")), "{found}");
        fs::write(root.join("etc/pacman.conf"), "[options]\nHookDir = /etc/alpm-hooks\n").unwrap();
        let elsewhere = trouble(root, &files).join("\n");
        assert!(
            !elsewhere.contains("/etc/pacman.d/hooks/"),
            "with HookDir set, only it shadows: {elsewhere}"
        );
        fs::remove_file(system.join(SNAPSHOT)).unwrap();
        assert!(
            trouble(root, &files)
                .join("\n")
                .contains(&format!("{SNAPSHOT} is missing"))
        );
    }

    #[test]
    fn a_target_touches_the_initramfs_when_it_owns_a_file_a_trigger_matches() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path();
        for (entry, files) in [
            ("linux-6.17.1.arch1-1", "usr/lib/modules/6.17.1-arch1-1/vmlinuz"),
            ("tree-2.2.1-1", "usr/bin/tree"),
        ] {
            fs::create_dir_all(local.join(entry)).unwrap();
            fs::write(local.join(entry).join("files"), format!("%FILES%\nusr/\n{files}\n\n")).unwrap();
        }
        let triggers = initramfs_triggers(Path::new("/nonexistent"));
        assert!(touches_initramfs(local, &["linux".into()], &triggers, false));
        assert!(!touches_initramfs(local, &["tree".into()], &triggers, false));
        assert!(touches_initramfs(local, &["gone".into()], &triggers, true));
        assert!(!touches_initramfs(local, &["gone".into()], &triggers, false));
    }
}

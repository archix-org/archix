//! `capabilities`: file capabilities on files in a deployment's /usr and /opt, set in the staged tree before the commit, as `setcap` would.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::CStr;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::deploy::xattr;

const XATTR: &CStr = c"security.capability";
const REVISION_1: u32 = 0x0100_0000;
const REVISION_2: u32 = 0x0200_0000;
const REVISION_3: u32 = 0x0300_0000;
const REVISION_MASK: u32 = 0xFF00_0000;
const EFFECTIVE: u32 = 0x0000_0001;

/// Linux's capabilities by number, as libcap names them.
const NAMES: &[&str] = &[
    "cap_chown",
    "cap_dac_override",
    "cap_dac_read_search",
    "cap_fowner",
    "cap_fsetid",
    "cap_kill",
    "cap_setgid",
    "cap_setuid",
    "cap_setpcap",
    "cap_linux_immutable",
    "cap_net_bind_service",
    "cap_net_broadcast",
    "cap_net_admin",
    "cap_net_raw",
    "cap_ipc_lock",
    "cap_ipc_owner",
    "cap_sys_module",
    "cap_sys_rawio",
    "cap_sys_chroot",
    "cap_sys_ptrace",
    "cap_sys_pacct",
    "cap_sys_admin",
    "cap_sys_boot",
    "cap_sys_nice",
    "cap_sys_resource",
    "cap_sys_time",
    "cap_sys_tty_config",
    "cap_mknod",
    "cap_lease",
    "cap_audit_write",
    "cap_audit_control",
    "cap_setfcap",
    "cap_mac_override",
    "cap_mac_admin",
    "cap_syslog",
    "cap_wake_alarm",
    "cap_block_suspend",
    "cap_audit_read",
    "cap_perfmon",
    "cap_bpf",
    "cap_checkpoint_restore",
];

/// A file's capability sets; a file has one effective bit, which raises all of permitted and inheritable at exec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Caps {
    pub permitted: u64,
    pub inheritable: u64,
    pub effective: bool,
}

fn all() -> u64 {
    (1u64 << NAMES.len()) - 1
}

fn number(name: &str) -> Result<u64> {
    let lower = name.to_ascii_lowercase();
    if let Some(index) = NAMES.iter().position(|known| *known == lower) {
        return Ok(1 << index);
    }
    match lower.parse::<usize>() {
        Ok(n) if n < NAMES.len() => Ok(1 << n),
        _ => bail!("{name:?} isn't a capability this kernel knows, such as cap_sys_nice"),
    }
}

/// Parses libcap's text form (`cap_from_text(3)`, as `setcap` takes it): space-separated clauses of a comma-separated list of capabilities (or `all`, or none before `=`) followed by operators `=`, `+` or `-`, each with flags from `e`, `i` and `p`.
pub fn parse(text: &str) -> Result<Caps> {
    let (mut p, mut i, mut e) = (0u64, 0u64, 0u64);
    let clauses: Vec<&str> = text.split_whitespace().collect();
    if clauses.is_empty() {
        bail!("{text:?} names no capability; write it as setcap takes it, such as cap_sys_nice+ep");
    }
    for clause in clauses {
        let Some(at) = clause.find(['=', '+', '-']) else {
            bail!("{clause:?} has no operator: add =, + or - and the flags, as in cap_sys_nice+ep");
        };
        let (list, mut ops) = clause.split_at(at);
        let caps = match list {
            "" => {
                if !ops.starts_with('=') {
                    bail!("{clause:?}: a clause without capabilities must start with =");
                }
                all()
            }
            "all" => all(),
            list => list
                .split(',')
                .map(number)
                .collect::<Result<Vec<u64>>>()?
                .into_iter()
                .fold(0, |sum, bit| sum | bit),
        };
        while let Some(op) = ops.chars().next() {
            let rest = &ops[1..];
            let end = rest.find(['=', '+', '-']).unwrap_or(rest.len());
            let flags = &rest[..end];
            if let Some(bad) = flags.chars().find(|c| !"eip".contains(*c)) {
                bail!("{clause:?}: {bad:?} isn't a flag; the flags are e, i and p");
            }
            if op == '=' {
                p &= !caps;
                i &= !caps;
                e &= !caps;
            } else if flags.is_empty() {
                bail!("{clause:?}: {op} needs flags from e, i and p");
            }
            for flag in flags.chars() {
                let set = match flag {
                    'p' => &mut p,
                    'i' => &mut i,
                    _ => &mut e,
                };
                if op == '-' {
                    *set &= !caps;
                } else {
                    *set |= caps;
                }
            }
            ops = &rest[end..];
        }
    }
    if p | i == 0 {
        bail!("{text:?} leaves the file without capabilities; drop it from capabilities instead");
    }
    if e != 0 && e != p | i {
        bail!(
            "{text:?}: a file has a single effective flag, so give e to every capability it has or to none, as in cap_sys_nice+ep"
        );
    }
    Ok(Caps {
        permitted: p,
        inheritable: i,
        effective: e != 0,
    })
}

/// The `security.capability` value for `caps`, revision 2 as the kernel and `setcap` write it.
pub fn encode(caps: &Caps) -> Vec<u8> {
    let magic = REVISION_2 | if caps.effective { EFFECTIVE } else { 0 };
    [
        magic,
        caps.permitted as u32,
        caps.inheritable as u32,
        (caps.permitted >> 32) as u32,
        (caps.inheritable >> 32) as u32,
    ]
    .iter()
    .flat_map(|word| word.to_le_bytes())
    .collect()
}

/// The sets in a `security.capability` value of any revision; None for one that isn't.
pub fn decode(value: &[u8]) -> Option<Caps> {
    let word = |n: usize| -> Option<u32> { Some(u32::from_le_bytes(value.get(n * 4..n * 4 + 4)?.try_into().ok()?)) };
    let magic = word(0)?;
    let (permitted, inheritable) = match (magic & REVISION_MASK, value.len()) {
        (REVISION_1, 12) => (word(1)? as u64, word(2)? as u64),
        (REVISION_2, 20) | (REVISION_3, 24) => (
            word(1)? as u64 | (word(3)? as u64) << 32,
            word(2)? as u64 | (word(4)? as u64) << 32,
        ),
        _ => return None,
    };
    Some(Caps {
        permitted,
        inheritable,
        effective: magic & EFFECTIVE != 0,
    })
}

/// `caps` as `getcap` prints it, such as cap_sys_nice=ep.
pub fn to_text(caps: &Caps) -> String {
    let names = |set: u64| -> Vec<&str> {
        NAMES
            .iter()
            .enumerate()
            .filter(|(n, _)| set & (1 << n) != 0)
            .map(|(_, name)| *name)
            .collect()
    };
    let e = if caps.effective { "e" } else { "" };
    let both = caps.permitted & caps.inheritable;
    let mut clauses = Vec::new();
    for (set, flags) in [
        (both, format!("{e}ip")),
        (caps.permitted & !both, format!("{e}p")),
        (caps.inheritable & !both, format!("{e}i")),
    ] {
        if set != 0 {
            clauses.push(format!("{}={flags}", names(set).join(",")));
        }
    }
    clauses.join(" ")
}

/// The capabilities on the file at `path`, None when it has none.
pub fn read(path: &Path) -> Result<Option<Caps>> {
    let value = xattr::get(path, XATTR).with_context(|| format!("reading {}'s capabilities", path.display()))?;
    Ok(value.as_deref().and_then(decode))
}

/// Checks `capabilities` from the config: each key a file under /usr or /opt (not the shared /usr/local), each value libcap's text form.
pub fn validate(capabilities: &BTreeMap<String, String>) -> Result<()> {
    for (path, text) in capabilities {
        let inside = ["/usr/", "/opt/"].iter().any(|tree| path.starts_with(tree)) && !path.starts_with("/usr/local/");
        if !inside || path.ends_with('/') || path.split('/').any(|part| part == ".." || part == ".") {
            bail!(
                "capabilities: {path:?} must be a file in a deployment's /usr or /opt, which archix builds; /usr/local is shared and setcap works there by hand"
            );
        }
        parse(text).with_context(|| format!("capabilities.{path}"))?;
    }
    Ok(())
}

/// Gives `path` an inode of its own, a copy with its owner, mode, other extended attributes and times, so a change to it can't reach another deployment hardlinked to the same one.
fn own_inode(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if meta.nlink() <= 1 {
        return Ok(());
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".archix-cap");
    let tmp = path.with_file_name(name);
    let _ = fs::remove_file(&tmp);
    let copied = (|| -> Result<()> {
        fs::copy(path, &tmp)?;
        std::os::unix::fs::lchown(&tmp, Some(meta.uid()), Some(meta.gid()))?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(meta.mode() & 0o7777))?;
        for attr in xattr::list(path)? {
            if attr.as_c_str() == XATTR {
                continue;
            }
            if let Some(value) = xattr::get(path, &attr)? {
                xattr::set(&tmp, &attr, &value)?;
            }
        }
        crate::deploy::flatten::set_times(&tmp, &meta)?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if copied.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    copied.with_context(|| format!("copying {} to give it its own inode", path.display()))
}

/// The capabilities a record's text gives, None for the empty text of none.
pub fn recorded(text: &str) -> Option<Caps> {
    parse(text).ok()
}

fn text_of(caps: Option<Caps>) -> String {
    caps.map(|caps| to_text(&caps)).unwrap_or_default()
}

/// Gives `file` the capabilities `caps`, or none, on an inode of its own unless it has them already.
fn put(file: &Path, caps: Option<Caps>) -> Result<()> {
    if read(file)? == caps {
        return Ok(());
    }
    own_inode(file)?;
    match caps {
        Some(caps) => xattr::set(file, XATTR, &encode(&caps)),
        None => xattr::remove(file, XATTR),
    }
    .with_context(|| format!("setting {}'s capabilities", file.display()))
}

/// The base deployment's side of `apply`: the capabilities its config declared, and those its record says packages gave its files.
pub struct Base<'a> {
    pub declared: &'a BTreeMap<String, String>,
    pub packages: &'a BTreeMap<String, String>,
}

/// Sets `to`'s capabilities in `tree`, a deployment not yet committed, and gives a file the base declared and `to` no longer does back the capabilities its package left it, when it still has the base's; each file changed gets its own inode first. `owned` are the files packages ship (without the leading /) and `declared` those the config writes; a capability on anything else is refused before any changes. Returns the new deployment's record of what packages left each file it declares or gave back.
pub fn apply(
    tree: &Path,
    base: &Base,
    to: &BTreeMap<String, String>,
    owned: &HashSet<String>,
    declared: &BTreeSet<String>,
) -> Result<BTreeMap<String, String>> {
    let mut refused = Vec::new();
    let mut wanted = Vec::new();
    for (path, text) in to {
        let caps = parse(text).with_context(|| format!("capabilities.{path}"))?;
        let file = tree.join(path.trim_start_matches('/'));
        match fs::symlink_metadata(&file) {
            Err(_) => refused.push(format!("{path} isn't in the new deployment")),
            Ok(meta) if meta.file_type().is_symlink() => refused.push(format!(
                "{path} is a symlink (to {}); give the file it points at",
                fs::read_link(&file).unwrap_or_default().display()
            )),
            Ok(meta) if !meta.is_file() => refused.push(format!("{path} isn't a regular file")),
            Ok(_) if !owned.contains(path.trim_start_matches('/')) && !declared.contains(path) => refused.push(format!(
                "{path} isn't a package's or a file the config declares, so nothing would put it back the same in another deployment"
            )),
            Ok(_) => wanted.push((path, file, caps)),
        }
    }
    if !refused.is_empty() {
        bail!("capabilities: {}; nothing was changed", refused.join("; "));
    }
    // A file still carrying the base's declared capabilities is the base's file, whose package state the base recorded; any other is as its package left it now.
    let left_by_package =
        |path: &str, now: Option<Caps>| match base.declared.get(path).and_then(|text| parse(text).ok()) {
            Some(had) if now == Some(had) => base.packages.get(path).and_then(|text| recorded(text)),
            _ => now,
        };
    let mut packages = BTreeMap::new();
    for (path, file, caps) in wanted {
        let now = read(&file)?;
        packages.insert(path.clone(), text_of(left_by_package(path, now)));
        put(&file, Some(caps))?;
    }
    for path in base.declared.keys().filter(|path| !to.contains_key(*path)) {
        let file = tree.join(path.trim_start_matches('/'));
        if !fs::symlink_metadata(&file).is_ok_and(|meta| meta.is_file()) {
            continue;
        }
        let left = left_by_package(path, read(&file)?);
        packages.insert(path.clone(), text_of(left));
        put(&file, left)?;
    }
    Ok(packages)
}

/// What each file in a deployment's record should have: its declared capabilities, or those its package left it when no longer declared.
fn expected(
    declared: &BTreeMap<String, String>,
    packages: &BTreeMap<String, String>,
) -> BTreeMap<String, Option<Caps>> {
    let mut want: BTreeMap<String, Option<Caps>> = packages
        .iter()
        .map(|(path, text)| (path.clone(), recorded(text)))
        .collect();
    for (path, text) in declared {
        if let Ok(caps) = parse(text) {
            want.insert(path.clone(), Some(caps));
        }
    }
    want
}

/// The capabilities a deployment's files lack or have otherwise, one line each: the declared ones, and for a file no longer declared those its package left it, from `packages`, the deployment's record of them.
pub fn check(
    deployment: &Path,
    declared: &BTreeMap<String, String>,
    packages: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut problems = Vec::new();
    for (path, want) in expected(declared, packages) {
        let theirs = if declared.contains_key(&path) {
            ""
        } else {
            " as its package left it"
        };
        match (read(&deployment.join(path.trim_start_matches('/'))), want) {
            (Ok(have), want) if have == want => {}
            (Ok(Some(have)), Some(want)) => {
                problems.push(format!("{path} has {}, not {}{theirs}", to_text(&have), to_text(&want)))
            }
            (Ok(None), Some(want)) => {
                problems.push(format!("{path} lacks its capabilities {}{theirs}", to_text(&want)))
            }
            (Ok(Some(have)), None) => problems.push(format!(
                "{path} has {}, which its package didn't give it",
                to_text(&have)
            )),
            (Ok(None), None) => {}
            (Err(e), _) => problems.push(format!("{path}: {e:#}")),
        }
    }
    problems
}

/// Puts back what `check` finds amiss in a deployment's tree, each changed file on an inode of its own; a file that's gone stays gone.
pub fn repair(
    deployment: &Path,
    declared: &BTreeMap<String, String>,
    packages: &BTreeMap<String, String>,
) -> Result<()> {
    for (path, want) in expected(declared, packages) {
        let file = deployment.join(path.trim_start_matches('/'));
        if fs::symlink_metadata(&file).is_ok_and(|meta| meta.is_file()) {
            put(&file, want)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bit(name: &str) -> u64 {
        number(name).unwrap()
    }

    #[test]
    fn libcap_text_parses_into_the_sets_setcap_would_write() {
        let nice = parse("cap_sys_nice+ep").unwrap();
        assert_eq!(
            nice,
            Caps {
                permitted: bit("cap_sys_nice"),
                inheritable: 0,
                effective: true
            }
        );
        assert_eq!(
            parse("CAP_SYS_NICE=pe").unwrap(),
            nice,
            "names are case-insensitive, flags in any order"
        );
        assert_eq!(parse("23+ep").unwrap(), nice, "a capability by number");
        let net = parse("cap_net_raw,cap_net_admin=eip").unwrap();
        assert_eq!(net.permitted, bit("cap_net_raw") | bit("cap_net_admin"));
        assert_eq!(net.inheritable, net.permitted);
        assert!(net.effective);
        let two = parse("cap_net_raw+p cap_bpf+p").unwrap();
        assert_eq!(two.permitted, bit("cap_net_raw") | bit("cap_bpf"));
        assert!(!two.effective);
        assert_eq!(
            parse("=ep cap_sys_admin-ep").unwrap().permitted,
            all() & !bit("cap_sys_admin")
        );
        assert_eq!(parse("all+p").unwrap().permitted, all());
        assert_eq!(parse("cap_chown+eip cap_chown-i").unwrap().inheritable, 0);
        assert_eq!(parse("cap_checkpoint_restore+p").unwrap().permitted, 1 << 40);
    }

    #[test]
    fn text_setcap_would_refuse_is_refused_with_why() {
        for (bad, why) in [
            ("", "names no capability"),
            ("cap_sys_nice", "no operator"),
            ("cap_sys_nicer+ep", "isn't a capability"),
            ("cap_sys_nice+ex", "isn't a flag"),
            ("cap_sys_nice+", "needs flags"),
            ("+ep", "must start with ="),
            ("cap_sys_nice=", "without capabilities"),
            ("cap_sys_nice+p cap_bpf+ep", "single effective flag"),
            ("41+p", "isn't a capability"),
        ] {
            let err = format!("{:#}", parse(bad).unwrap_err());
            assert!(err.contains(why), "{bad:?}: {err}");
        }
    }

    #[test]
    fn the_xattr_round_trips_and_reads_every_revision() {
        let caps = parse("cap_sys_admin+ep cap_checkpoint_restore+ep").unwrap();
        let value = encode(&caps);
        assert_eq!(value.len(), 20);
        assert_eq!(&value[..4], &(REVISION_2 | EFFECTIVE).to_le_bytes());
        assert_eq!(decode(&value), Some(caps));
        let mut v3 = value.clone();
        v3[..4].copy_from_slice(&(REVISION_3 | EFFECTIVE).to_le_bytes());
        v3.extend(1000u32.to_le_bytes());
        assert_eq!(decode(&v3), Some(caps), "revision 3 carries a namespace's root uid");
        let v1: Vec<u8> = [REVISION_1, 1 << 23, 0].iter().flat_map(|w| w.to_le_bytes()).collect();
        assert_eq!(decode(&v1).unwrap().permitted, bit("cap_sys_nice"));
        assert_eq!(decode(&value[..19]), None);
        assert_eq!(to_text(&caps), "cap_sys_admin,cap_checkpoint_restore=ep");
        assert_eq!(
            to_text(&parse("cap_net_raw=eip cap_bpf+eip cap_bpf-i").unwrap()),
            "cap_net_raw=eip cap_bpf=ep"
        );
        assert_eq!(
            to_text(&parse("cap_net_raw+ip cap_bpf+p").unwrap()),
            "cap_net_raw=ip cap_bpf=p"
        );
    }

    #[test]
    fn only_files_in_usr_and_opt_outside_usr_local_take_capabilities() {
        let one = |path: &str, text: &str| validate(&BTreeMap::from([(path.to_string(), text.to_string())]));
        one("/usr/bin/gamescope", "cap_sys_nice+ep").unwrap();
        one("/opt/app/bin/helper", "cap_net_raw+p").unwrap();
        for bad in [
            "/etc/foo",
            "/usr/local/bin/x",
            "/usr/bin/",
            "/usr/../etc/shadow",
            "usr/bin/x",
            "/home/x",
        ] {
            let err = format!("{:#}", one(bad, "cap_sys_nice+ep").unwrap_err());
            assert!(
                err.contains("must be a file in a deployment's /usr or /opt"),
                "{bad}: {err}"
            );
        }
        let err = format!("{:#}", one("/usr/bin/x", "cap_bogus+ep").unwrap_err());
        assert!(err.starts_with("capabilities./usr/bin/x: "), "{err}");
    }
}

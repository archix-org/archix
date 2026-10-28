//! `holds`: packages kept at a version, an AUR commit or above a floor while the rest of the lock moves with the snapshot.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::archive::Archive;
use crate::aur::index::{constraint, meets};
use crate::aur::srcinfo::dep_name;
use crate::lock::{ARCHIVE_REPO, AUR_REPO, Held, HostLock, LOCAL_REPO, Locked};
use crate::pacman::{SyncPkg, vercmp};
use crate::repos::is_official;
use crate::value::Value;

/// One package's hold, as `holds` declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hold {
    /// At this version; without a pkgrel, the newest pkgrel of that pkgver.
    Exact(String),
    /// Never below this version, otherwise moving with the snapshot.
    AtLeast(String),
    /// An AUR package's recipe at this commit.
    Commit(String),
}

pub type Holds = BTreeMap<String, Hold>;

impl Hold {
    /// The hold as archix.lock records it, which `parse_spec` reads back.
    pub fn spec(&self) -> String {
        match self {
            Hold::Exact(version) => version.clone(),
            Hold::AtLeast(version) => format!(">={version}"),
            Hold::Commit(commit) => format!("commit:{commit}"),
        }
    }

    pub fn parse_spec(spec: &str) -> Result<Hold> {
        if let Some(commit) = spec.strip_prefix("commit:") {
            return commit_hold(commit);
        }
        version_hold(spec)
    }

    /// Whether `version` is what an exact hold asks for; any version meets the other kinds here.
    pub fn matches(&self, version: &str) -> bool {
        match self {
            Hold::Exact(want) if want.contains('-') => vercmp(version, want) == Ordering::Equal,
            Hold::Exact(want) => vercmp(pkgver(version), want) == Ordering::Equal,
            _ => true,
        }
    }

    /// Whether it keeps the package off the snapshot's version, as a floor doesn't.
    pub fn holds_back(&self) -> bool {
        !matches!(self, Hold::AtLeast(_))
    }
}

/// `[epoch:]pkgver` without the pkgrel.
fn pkgver(version: &str) -> &str {
    version.rsplit_once('-').map_or(version, |(pkgver, _)| pkgver)
}

fn commit_hold(commit: &str) -> Result<Hold> {
    let commit = commit.trim();
    if !(7..=40).contains(&commit.len()) || !commit.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("{commit:?} isn't a git commit: give 7 to 40 hex digits");
    }
    Ok(Hold::Commit(commit.to_ascii_lowercase()))
}

fn version_hold(text: &str) -> Result<Hold> {
    let text = text.trim();
    let (floor, version) = match text.strip_prefix(">=") {
        Some(rest) => (true, rest.trim()),
        None => (false, text),
    };
    if version.starts_with(['<', '>', '=']) {
        bail!(
            "{text:?}: a hold is an exact version (\"1.1.2-1\", or \"1.1.2\" for its newest pkgrel), a floor (\">= 1.1.2\") or an AUR commit ({{ commit = \"...\" }})"
        );
    }
    valid_version(version).with_context(|| format!("{text:?} isn't a pacman version"))?;
    Ok(if floor {
        Hold::AtLeast(version.to_string())
    } else {
        Hold::Exact(version.to_string())
    })
}

/// `[epoch:]pkgver[-pkgrel]` as pacman versions are: an epoch of digits, a pkgver without `-` or `:`, a pkgrel of digits with an optional `.n`.
fn valid_version(version: &str) -> Result<()> {
    let (epoch, rest) = match version.split_once(':') {
        Some((epoch, rest)) => (Some(epoch), rest),
        None => (None, version),
    };
    if epoch.is_some_and(|epoch| epoch.is_empty() || !epoch.chars().all(|c| c.is_ascii_digit())) {
        bail!("its epoch isn't a number");
    }
    let (ver, rel) = match rest.split_once('-') {
        Some((ver, rel)) => (ver, Some(rel)),
        None => (rest, None),
    };
    if ver.is_empty() || !ver.chars().all(|c| c.is_ascii_alphanumeric() || "._+~".contains(c)) {
        bail!("its pkgver may only hold letters, digits and . _ + ~");
    }
    if let Some(rel) = rel {
        let mut parts = rel.split('.');
        let numeric = |part: Option<&str>| part.is_some_and(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
        let whole = numeric(parts.next());
        let sub = parts.next().is_none_or(|p| numeric(Some(p)));
        if !whole || !sub || parts.next().is_some() {
            bail!("its pkgrel must be a number such as 1 or 2.1");
        }
    }
    Ok(())
}

/// The `holds` option: package name to a version string or a `{ commit = ... }` table.
pub fn from_config(value: Option<&Value>) -> Result<Holds> {
    let Some(Value::Map(map)) = value else {
        return Ok(Holds::new());
    };
    let mut holds = Holds::new();
    for (name, spec) in map {
        let hold = match spec {
            Value::Str(text) => version_hold(text),
            Value::Map(table) => match (table.get("commit"), table.len()) {
                (Some(Value::Str(commit)), 1) => commit_hold(commit),
                _ => Err(anyhow::anyhow!("a table hold takes exactly one key, commit")),
            },
            _ => Err(anyhow::anyhow!(
                "give a version string or {{ commit = \"...\" }}, not {spec}"
            )),
        };
        holds.insert(name.clone(), hold.with_context(|| format!("holds.{name}"))?);
    }
    Ok(holds)
}

/// The holds a lock was resolved with, back from what it records.
pub fn from_lock(lock: &HostLock) -> Result<Holds> {
    lock.held
        .iter()
        .map(|(name, held)| Ok((name.clone(), Hold::parse_spec(&held.hold)?)))
        .collect()
}

/// The specs `HostLock::matches` compares.
pub fn specs(holds: &Holds) -> BTreeMap<String, String> {
    holds.iter().map(|(name, hold)| (name.clone(), hold.spec())).collect()
}

/// What a package says about itself for dependency checks: its version, what it provides and depends on.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Meta {
    pub version: String,
    pub provides: Vec<String>,
    pub depends: Vec<String>,
}

impl From<&SyncPkg> for Meta {
    fn from(pkg: &SyncPkg) -> Meta {
        Meta {
            version: pkg.version.clone(),
            provides: pkg.provides.clone(),
            depends: pkg.depends.clone(),
        }
    }
}

/// Reads the version, provides and depends from a package file's .PKGINFO.
pub fn pkginfo(path: &Path) -> Result<Meta> {
    let at = || format!("reading {}", path.display());
    let file = File::open(path).with_context(at)?;
    let mut archive = tar::Archive::new(zstd::Decoder::new(file).with_context(at)?);
    for entry in archive.entries().with_context(at)? {
        let mut entry = entry.with_context(at)?;
        if entry.path().with_context(at)?.as_os_str() != ".PKGINFO" {
            continue;
        }
        let mut text = String::new();
        entry.read_to_string(&mut text).with_context(at)?;
        let values = |key: &str| -> Vec<String> {
            text.lines()
                .filter_map(|line| line.strip_prefix(key)?.strip_prefix(" = "))
                .map(str::to_string)
                .collect()
        };
        return Ok(Meta {
            version: values("pkgver").into_iter().next().unwrap_or_default(),
            provides: values("provides"),
            depends: values("depend"),
        });
    }
    bail!("{} has no .PKGINFO", path.display())
}

/// Whether `meta`, the package `name`, meets the dependency `dep`, by name or by what it provides.
fn satisfies(name: &str, meta: &Meta, dep: &str) -> bool {
    let wanted = dep_name(dep);
    let want = constraint(dep);
    (name == wanted && meets(Some(&meta.version), want))
        || meta
            .provides
            .iter()
            .filter(|provided| dep_name(provided) == wanted)
            .any(|provided| meets(provided.split_once('=').map(|(_, v)| v), want))
}

/// Whether `meta`, the package `name`, could ever stand for `dep`: its name or something it provides, at any version.
fn answers(name: &str, meta: &Meta, dep: &str) -> bool {
    let wanted = dep_name(dep);
    name == wanted || meta.provides.iter().any(|provided| dep_name(provided) == wanted)
}

/// Checks the lock's packages, `metas` by name, still meet each other's dependencies with the held ones in place; `snapshot` has what each held package was at the snapshot. A broken dependency that a held package used to answer, or that a held package has, is an error naming both sides.
pub fn check_dependencies(
    metas: &BTreeMap<String, Meta>,
    held: &BTreeMap<String, Meta>,
    snapshot: &BTreeMap<String, Meta>,
) -> Result<()> {
    let mut problems = Vec::new();
    for (name, meta) in metas {
        for dep in &meta.depends {
            if metas.iter().any(|(other, m)| satisfies(other, m, dep)) {
                continue;
            }
            if held.contains_key(name) {
                let near = metas
                    .iter()
                    .find(|(other, m)| answers(other, m, dep))
                    .map(|(other, m)| format!("the lock has {other} {}", m.version))
                    .unwrap_or_else(|| "nothing in the lock provides it; declare a package that does".to_string());
                problems.push(format!("held {name} {} needs `{dep}`, but {near}", meta.version));
                continue;
            }
            let Some((culprit, was)) = snapshot
                .iter()
                .find(|(h, before)| satisfies(h, before, dep) || answers(h, before, dep))
            else {
                continue;
            };
            let now = &metas[culprit].version;
            problems.push(format!(
                "{name} {} needs `{dep}`, but holds keep {culprit} at {now} (the snapshot has {})",
                meta.version, was.version
            ));
        }
    }
    if problems.is_empty() {
        return Ok(());
    }
    bail!(
        "the holds break dependencies, as Arch doesn't support partial upgrades:\n  {}\nmove the hold, hold the dependent package too, or lock an older snapshot; nothing was changed",
        problems.join("\n  ")
    )
}

/// Package files the archive keeps for `name`, from its /packages/ listing, for x86_64 or any.
fn archive_files(archive: &Archive, cache: &Path, name: &str) -> Result<Vec<(String, String)>> {
    let dir = cache.join("held");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let listing = dir.join(format!("{name}.listing.{}", std::process::id()));
    let url = archive.package_url(name, "");
    let found = archive.fetch(&url, &listing)?;
    let text = std::fs::read_to_string(&listing).unwrap_or_default();
    let _ = std::fs::remove_file(&listing);
    if !found {
        bail!("the Arch Linux Archive has no {name} at all ({url})");
    }
    Ok(listed_versions(&text, name, crate::testhooks::archive_today()))
}

/// The day an autoindex row says its file was made, from its DD-Mon-YYYY column.
fn row_date(row: &str) -> Option<crate::date::Date> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    row.split_whitespace().find_map(|word| {
        let mut parts = word.split('-');
        let (Some(day), Some(month), Some(year), None) = (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return None;
        };
        let month = MONTHS.iter().position(|m| *m == month)? + 1;
        format!("{year}-{month:02}-{day}").parse().ok()
    })
}

/// (file, version) for each `<name>-<version>-<arch>.pkg.tar.zst` an archive directory listing links to, leaving out files its rows date after `until`.
pub fn listed_versions(html: &str, name: &str, until: Option<crate::date::Date>) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = html
        .split("href=\"")
        .skip(1)
        .filter(
            |row| match (until, row_date(row.split("<a ").next().unwrap_or_default())) {
                (Some(until), Some(made)) => made <= until,
                _ => true,
            },
        )
        .filter_map(|rest| rest.split('"').next())
        .map(|href| href.replace("%3A", ":").replace("%2B", "+"))
        .filter_map(|file| {
            let stem = file.strip_suffix(".pkg.tar.zst")?;
            let rest = stem.strip_prefix(name)?.strip_prefix('-')?;
            let (version, arch) = rest.rsplit_once('-')?;
            let (ver, rel) = version.rsplit_once('-')?;
            if ver.contains('-') {
                return None;
            }
            let real = matches!(arch, "x86_64" | "any")
                && !rel.is_empty()
                && rel.chars().all(|c| c.is_ascii_digit() || c == '.');
            real.then(|| (file.clone(), version.to_string()))
        })
        .collect();
    found.sort_by(|a, b| vercmp(&a.1, &b.1));
    found.dedup_by(|a, b| a.1 == b.1);
    found
}

/// The resolution's view of everything needed to put holds in place.
pub struct Resolution<'a> {
    pub archive: &'a Archive,
    pub cache: &'a Path,
    /// The snapshot's repos, by name.
    pub sync: &'a BTreeMap<String, BTreeMap<String, SyncPkg>>,
    /// Each AUR or recipe package's metadata from its .SRCINFO.
    pub built: &'a BTreeMap<String, Meta>,
    /// The version the AUR's newest recipe builds, for each held AUR package.
    pub aur_newest: &'a BTreeMap<String, String>,
}

/// Puts each hold into a freshly resolved `lock`: exact official holds swap in the archive's package, AUR holds were resolved at their commit already and are checked, floors are checked; then the dependencies are checked across the result. Records the holds in `lock.held`.
pub fn apply(lock: &mut HostLock, holds: &Holds, cx: &Resolution) -> Result<()> {
    lock.held.clear();
    if holds.is_empty() {
        return Ok(());
    }
    let mut snapshot_metas = BTreeMap::new();
    let mut held_metas = BTreeMap::new();
    for (name, hold) in holds {
        let Some(locked) = lock.packages.get(name).cloned() else {
            bail!(
                "holds.{name}: {name} isn't among this config's packages or their dependencies; drop the hold or declare it"
            );
        };
        let snapshot_version = locked.version.clone();
        let mut newest = snapshot_version.clone();
        match (hold, locked.repo.as_str()) {
            (_, LOCAL_REPO) => bail!(
                "holds.{name}: {name} is built from the config's own recipe, which the config already fixes; change the recipe instead"
            ),
            (Hold::Commit(commit), AUR_REPO) => {
                if !locked
                    .commit
                    .as_deref()
                    .is_some_and(|locked| locked.starts_with(commit.as_str()))
                {
                    bail!(
                        "holds.{name}: resolved {name} at commit {}, not the held {commit}",
                        locked.commit.as_deref().unwrap_or("?")
                    );
                }
                newest = cx.aur_newest.get(name).cloned().unwrap_or(newest);
            }
            (Hold::Commit(_), repo) => bail!(
                "holds.{name}: {name} comes from {repo}, not the AUR, so there's no recipe commit to hold; hold a version instead"
            ),
            (Hold::Exact(want), AUR_REPO) => {
                if !hold.matches(&locked.version) {
                    bail!("holds.{name}: resolved {name} {}, not the held {want}", locked.version);
                }
                newest = cx.aur_newest.get(name).cloned().unwrap_or(newest);
            }
            (Hold::Exact(want), repo) => {
                if !hold.matches(&locked.version) {
                    if !is_official(repo) && repo != ARCHIVE_REPO {
                        bail!(
                            "holds.{name}: {name} comes from the custom repo {repo}, which has {} and keeps no older versions; only official packages can be held at another version",
                            locked.version
                        );
                    }
                    let (entry, meta) = from_archive(cx, name, want, &locked)?;
                    snapshot_metas.insert(name.clone(), metas_of(cx, name, &locked));
                    held_metas.insert(name.clone(), meta);
                    lock.packages.insert(name.clone(), entry);
                }
            }
            (Hold::AtLeast(floor), _) => {
                if !meets(Some(&locked.version), Some((">=", floor))) {
                    bail!(
                        "holds.{name}: the {} snapshot has {name} {}, below the held >= {floor}; `archix update` locks a newer snapshot; nothing was changed",
                        lock.date,
                        locked.version
                    );
                }
            }
        }
        lock.pinned.retain(|pinned| pinned != name);
        lock.held.insert(
            name.clone(),
            Held {
                hold: hold.spec(),
                newest,
            },
        );
    }
    let snapshot: BTreeMap<String, Meta> = snapshot_metas
        .into_iter()
        .filter_map(|(name, meta)| Some((name, meta?)))
        .collect();
    if snapshot.is_empty() {
        return Ok(());
    }
    let mut metas = BTreeMap::new();
    for (name, locked) in &lock.packages {
        let meta = match held_metas.get(name) {
            Some(meta) => Some(meta.clone()),
            None => metas_of(cx, name, locked),
        };
        if let Some(meta) = meta {
            metas.insert(name.clone(), meta);
        }
    }
    let held: BTreeMap<String, Meta> = held_metas
        .into_iter()
        .filter(|(name, _)| snapshot.contains_key(name))
        .collect();
    check_dependencies(&metas, &held, &snapshot)
}

/// A locked package's metadata: from its repo's database, or its recipe's for built ones.
fn metas_of(cx: &Resolution, name: &str, locked: &Locked) -> Option<Meta> {
    if locked.is_built() {
        return cx.built.get(name).cloned();
    }
    cx.sync
        .get(&locked.repo)
        .and_then(|packages| packages.get(name))
        .filter(|pkg| pkg.version == locked.version)
        .map(Meta::from)
}

/// The archive's file for `name` at the held `want`, downloaded to read its metadata, and the lock entry for it.
fn from_archive(cx: &Resolution, name: &str, want: &str, locked: &Locked) -> Result<(Locked, Meta)> {
    let hold = Hold::Exact(want.to_string());
    let dir = cx.cache.join("held");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let get = |file: &str| -> Result<Option<std::path::PathBuf>> {
        let path = dir.join(file);
        let have = path.is_file() || cx.archive.fetch(&cx.archive.package_url(name, file), &path)?;
        Ok(have.then_some(path))
    };
    // With a pkgrel the file's name is known but for its arch, which spares reading the archive's listing.
    let mut found = None;
    if want.contains('-') {
        for arch in ["x86_64", "any"] {
            let file = format!("{name}-{want}-{arch}.pkg.tar.zst");
            if let Some(path) = get(&file)? {
                found = Some((file, want.to_string(), path));
                break;
            }
        }
    }
    let (file, version, path) = match found {
        Some(found) => found,
        None => {
            let files = archive_files(cx.archive, cx.cache, name)?;
            let Some((file, version)) = files.iter().rev().find(|(_, version)| hold.matches(version)) else {
                let near: Vec<&str> = files.iter().rev().take(6).map(|(_, v)| v.as_str()).collect();
                bail!(
                    "holds.{name}: the Arch Linux Archive has no {name} {want}{}; newest there: {}",
                    if want.contains('-') { "" } else { "-*" },
                    near.join(", ")
                );
            };
            let Some(path) = get(file)? else {
                bail!("holds.{name}: the Arch Linux Archive lists {file} but doesn't serve it");
            };
            (file.clone(), version.clone(), path)
        }
    };
    let meta = pkginfo(&path)?;
    if meta.version != version {
        let _ = std::fs::remove_file(&path);
        bail!("{} says it's {name} {}, not {version}", path.display(), meta.version);
    }
    let entry = Locked {
        version,
        repo: ARCHIVE_REPO.to_string(),
        file,
        sha256: crate::fetch::sha256(&path)?,
        explicit: locked.explicit,
        ..Default::default()
    };
    Ok((entry, meta))
}

/// Holds on packages whose security fixes matter most, which `archix status` warns about.
pub fn sensitive(name: &str) -> bool {
    name.starts_with("linux")
        || name.starts_with("systemd")
        || name.starts_with("archix")
        || [
            "glibc",
            "openssl",
            "openssh",
            "sudo",
            "pacman",
            "gnutls",
            "ca-certificates",
        ]
        .contains(&name)
}

/// One line per hold for `archix status` and `diff`, and a warning for holds keeping sensitive packages back.
pub fn status_lines(lock: &HostLock) -> Vec<String> {
    let mut lines = Vec::new();
    let mut risky = Vec::new();
    for (name, held) in &lock.held {
        let Some(locked) = lock.packages.get(name) else {
            continue;
        };
        let Ok(hold) = Hold::parse_spec(&held.hold) else {
            continue;
        };
        let source = if locked.repo == AUR_REPO {
            "the AUR's newest recipe"
        } else {
            "the snapshot"
        };
        let line = match (&hold, vercmp(&locked.version, &held.newest)) {
            (Hold::AtLeast(floor), _) => format!("{name} at {} (held at >= {floor})", locked.version),
            (_, Ordering::Equal) => format!("{name} held at {}, as {source} has it", locked.version),
            (_, Ordering::Less) => format!("{name} held at {}, behind {source}'s {}", locked.version, held.newest),
            (_, Ordering::Greater) => format!("{name} held at {}, ahead of {source}'s {}", locked.version, held.newest),
        };
        lines.push(format!("hold: {line}"));
        if hold.holds_back() && sensitive(name) && locked.version != held.newest {
            risky.push(format!("{name} {}", locked.version));
        }
    }
    if !risky.is_empty() {
        lines.push(format!(
            "warning: holds keep security-sensitive packages behind: {}; they miss the fixes in newer versions, and a held library the rest of the system moved past can break at runtime",
            risky.join(", ")
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(version: &str, provides: &[&str], depends: &[&str]) -> Meta {
        Meta {
            version: version.into(),
            provides: provides.iter().map(|s| s.to_string()).collect(),
            depends: depends.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn holds_parse_versions_floors_and_commits_and_refuse_the_rest() {
        let config = Value::Map(BTreeMap::from([
            ("scx-scheds".to_string(), Value::Str("1.1.2-1".into())),
            ("linux".to_string(), Value::Str("6.17.1.arch1".into())),
            ("v4l2loopback-dkms".to_string(), Value::Str(">= 0.15.4".into())),
            ("gtk2".to_string(), Value::Str("1:2.24.33-5.1".into())),
            (
                "foo-git".to_string(),
                Value::Map(BTreeMap::from([("commit".to_string(), Value::Str("ABC1234".into()))])),
            ),
        ]));
        let holds = from_config(Some(&config)).unwrap();
        assert_eq!(holds["scx-scheds"], Hold::Exact("1.1.2-1".into()));
        assert_eq!(holds["linux"], Hold::Exact("6.17.1.arch1".into()));
        assert_eq!(holds["v4l2loopback-dkms"], Hold::AtLeast("0.15.4".into()));
        assert_eq!(holds["gtk2"], Hold::Exact("1:2.24.33-5.1".into()));
        assert_eq!(holds["foo-git"], Hold::Commit("abc1234".into()));
        for hold in holds.values() {
            assert_eq!(&Hold::parse_spec(&hold.spec()).unwrap(), hold, "{hold:?} round-trips");
        }
        let refused = |value: Value| {
            let config = Value::Map(BTreeMap::from([("p".to_string(), value)]));
            format!("{:#}", from_config(Some(&config)).unwrap_err())
        };
        for (bad, why) in [
            ("< 2", "a hold is an exact version"),
            ("= 2", "a hold is an exact version"),
            (">= <3", "a hold is an exact version"),
            ("1.0-1-1", "pkgrel"),
            ("1.0-x", "pkgrel"),
            ("x:1.0", "epoch"),
            ("1.0 beta", "pkgver"),
            ("", "pkgver"),
        ] {
            let err = refused(Value::Str(bad.into()));
            assert!(err.starts_with("holds.p: ") && err.contains(why), "{bad}: {err}");
        }
        let err = refused(Value::Map(BTreeMap::from([(
            "commit".to_string(),
            Value::Str("xyz".into()),
        )])));
        assert!(err.contains("isn't a git commit"), "{err}");
        let err = refused(Value::Map(BTreeMap::from([(
            "version".to_string(),
            Value::Str("1".into()),
        )])));
        assert!(err.contains("exactly one key, commit"), "{err}");
        assert!(refused(Value::Int(3)).contains("give a version string"));
    }

    #[test]
    fn an_exact_hold_without_a_pkgrel_matches_any_pkgrel_of_that_pkgver() {
        let hold = Hold::Exact("1.1.2".into());
        assert!(hold.matches("1.1.2-1") && hold.matches("1.1.2-3.1"));
        assert!(!hold.matches("1.1.20-1") && !hold.matches("1:1.1.2-1"));
        let exact = Hold::Exact("1.1.2-1".into());
        assert!(exact.matches("1.1.2-1") && !exact.matches("1.1.2-2"));
    }

    #[test]
    fn the_archive_listing_gives_each_version_once_oldest_first() {
        let html = r#"<a href="scx-scheds-1.1.2-1-x86_64.pkg.tar.zst">x</a>
<a href="scx-scheds-1.1.2-1-x86_64.pkg.tar.zst.sig">x</a>
<a href="scx-scheds-1.0.9-2-x86_64.pkg.tar.zst">x</a>
<a href="scx-scheds-git-1.0-1-x86_64.pkg.tar.zst">x</a>
<a href="scx-scheds-1.1.10-1-x86_64.pkg.tar.xz">x</a>
<a href="scx-scheds-1%3A2.0-1-any.pkg.tar.zst">x</a>"#;
        let found = listed_versions(html, "scx-scheds", None);
        let versions: Vec<&str> = found.iter().map(|(_, v)| v.as_str()).collect();
        assert_eq!(versions, ["1.0.9-2", "1.1.2-1", "1:2.0-1"]);
        assert_eq!(found[2].0, "scx-scheds-1:2.0-1-any.pkg.tar.zst");
    }

    #[test]
    fn files_the_listing_dates_after_the_day_given_are_left_out() {
        let html = r#"<a href="../">../</a>
<a href="lemon-3.53.3-1-x86_64.pkg.tar.zst">lemon-3.53.3-1-x86_64.pkg.tar.zst</a>                  27-Jun-2026 08:19     49K
<a href="lemon-3.54.0-1-x86_64.pkg.tar.zst">lemon-3.54.0-1-x86_64.pkg.tar.zst</a>                  20-Oct-2026 21:08     49K
<a href="lemon-3.54.1-1-x86_64.pkg.tar.zst">lemon-3.54.1-1-x86_64.pkg.tar.zst</a>                  21-Oct-2026 00:01     49K
"#;
        let versions = |until: Option<&str>| -> Vec<String> {
            listed_versions(html, "lemon", until.map(|day| day.parse().unwrap()))
                .into_iter()
                .map(|(_, version)| version)
                .collect()
        };
        assert_eq!(versions(Some("2026-10-20")), ["3.53.3-1", "3.54.0-1"]);
        assert_eq!(versions(None), ["3.53.3-1", "3.54.0-1", "3.54.1-1"]);
        assert_eq!(versions(Some("2026-01-01")), Vec::<String>::new());
    }

    #[test]
    fn a_held_library_whose_dependents_moved_to_its_next_soname_is_refused_naming_both() {
        let snapshot = BTreeMap::from([("libfoo".to_string(), meta("2.0-1", &["libfoo.so=2-64"], &["glibc"]))]);
        let held = BTreeMap::from([("libfoo".to_string(), meta("1.9-1", &["libfoo.so=1-64"], &["glibc"]))]);
        let mut metas = BTreeMap::from([
            ("glibc".to_string(), meta("2.42-1", &[], &[])),
            ("app".to_string(), meta("3.0-1", &[], &["libfoo.so=2-64", "glibc"])),
            ("tool".to_string(), meta("1.0-1", &[], &["libfoo>=1.5"])),
        ]);
        metas.insert("libfoo".into(), held["libfoo"].clone());
        let err = format!("{:#}", check_dependencies(&metas, &held, &snapshot).unwrap_err());
        assert!(
            err.contains("app 3.0-1 needs `libfoo.so=2-64`, but holds keep libfoo at 1.9-1 (the snapshot has 2.0-1)"),
            "{err}"
        );
        assert!(
            !err.contains("tool"),
            "a dependency the held version meets is fine: {err}"
        );
        assert!(err.contains("nothing was changed"), "{err}");

        metas.insert("app".into(), meta("3.0-1", &[], &["libfoo.so=1-64"]));
        assert!(check_dependencies(&metas, &held, &snapshot).is_ok());

        metas.insert(
            "libfoo".into(),
            meta("1.9-1", &["libfoo.so=1-64"], &["libbar>=2", "libgone"]),
        );
        let held = BTreeMap::from([("libfoo".to_string(), metas["libfoo"].clone())]);
        metas.insert("libbar".into(), meta("1.5-1", &[], &[]));
        let err = format!("{:#}", check_dependencies(&metas, &held, &snapshot).unwrap_err());
        assert!(
            err.contains("held libfoo 1.9-1 needs `libbar>=2`, but the lock has libbar 1.5-1"),
            "{err}"
        );
        assert!(
            err.contains("needs `libgone`, but nothing in the lock provides it"),
            "{err}"
        );
    }

    #[test]
    fn status_names_each_hold_and_warns_about_sensitive_ones() {
        let mut lock = HostLock {
            date: "2026-10-14".parse().unwrap(),
            repos: Vec::new(),
            declared: Vec::new(),
            prefer: Vec::new(),
            pinned: Vec::new(),
            held: BTreeMap::new(),
            packages: BTreeMap::new(),
            flatpak: BTreeMap::new(),
            recipes: BTreeMap::new(),
        };
        let add = |lock: &mut HostLock, name: &str, version: &str, hold: &str, newest: &str| {
            lock.packages.insert(
                name.into(),
                Locked {
                    version: version.into(),
                    repo: ARCHIVE_REPO.into(),
                    ..Default::default()
                },
            );
            lock.held.insert(
                name.into(),
                Held {
                    hold: hold.into(),
                    newest: newest.into(),
                },
            );
        };
        add(&mut lock, "scx-scheds", "1.1.2-1", "1.1.2-1", "1.1.3-2");
        add(&mut lock, "glibc", "2.41-1", "2.41", "2.42-1");
        add(&mut lock, "v4l2loopback-dkms", "0.15.5-1", ">=0.15.4", "0.15.5-1");
        add(&mut lock, "openssl", "3.6.0-1", "3.6.0-1", "3.6.0-1");
        let lines = status_lines(&lock);
        assert_eq!(
            lines,
            [
                "hold: glibc held at 2.41-1, behind the snapshot's 2.42-1",
                "hold: openssl held at 3.6.0-1, as the snapshot has it",
                "hold: scx-scheds held at 1.1.2-1, behind the snapshot's 1.1.3-2",
                "hold: v4l2loopback-dkms at 0.15.5-1 (held at >= 0.15.4)",
                "warning: holds keep security-sensitive packages behind: glibc 2.41-1; they miss the fixes in newer versions, and a held library the rest of the system moved past can break at runtime",
            ]
        );
    }
}

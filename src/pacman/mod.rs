mod vercmp;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};

pub use vercmp::vercmp;

/// A package entry in a repository's sync database.
#[derive(Debug, Clone, PartialEq)]
pub struct SyncPkg {
    pub name: String,
    pub version: String,
    pub filename: String,
    pub sha256: String,
    pub description: String,
    /// `%PROVIDES%`: virtual names this package satisfies, possibly with `=version`.
    pub provides: Vec<String>,
    /// `%DEPENDS%`, possibly with version constraints.
    pub depends: Vec<String>,
    /// `%CONFLICTS%`, possibly with version constraints.
    pub conflicts: Vec<String>,
    /// `%BASE%`: the pkgbase whose recipe builds it; the name itself when the entry has none.
    pub base: String,
    /// `%CSIZE%`: the package file's size, 0 when the entry has none.
    pub size: u64,
}

/// A package installed on the system, from pacman's local database.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalPkg {
    pub name: String,
    pub version: String,
    pub arch: String,
    pub explicit: bool,
    /// Unix time pacman installed this version, from `%INSTALLDATE%`.
    #[serde(default)]
    pub installed_at: Option<u64>,
}

/// Parses a pacman `desc` file: `%KEY%` headers, each followed by value lines up to a blank line.
pub fn parse_desc(text: &str) -> HashMap<String, Vec<String>> {
    let mut fields = HashMap::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let Some(key) = line.strip_prefix('%').and_then(|l| l.strip_suffix('%')) else {
            continue;
        };
        let values = lines
            .by_ref()
            .take_while(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        fields.insert(key.to_string(), values);
    }
    fields
}

fn first(fields: &HashMap<String, Vec<String>>, key: &str) -> Option<String> {
    fields.get(key).and_then(|values| values.first()).cloned()
}

/// Reads every package from a sync database file such as `core.db`.
pub fn read_sync_db(path: &Path) -> Result<BTreeMap<String, SyncPkg>> {
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|mut f| f.read_to_end(&mut bytes))
        .with_context(|| format!("reading {}", path.display()))?;
    let decoder: Box<dyn Read> = if bytes.starts_with(&[0x1f, 0x8b]) {
        Box::new(GzDecoder::new(bytes.as_slice()))
    } else if bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Box::new(zstd::Decoder::new(bytes.as_slice()).with_context(|| format!("reading {}", path.display()))?)
    } else {
        bail!(
            "{} is neither a gzip- nor a zstd-compressed sync database",
            path.display()
        );
    };
    let mut packages = BTreeMap::new();
    let mut archive = tar::Archive::new(decoder);
    for entry in archive
        .entries()
        .with_context(|| format!("reading {}", path.display()))?
    {
        let mut entry = entry?;
        if !entry.path()?.ends_with("desc") {
            continue;
        }
        let mut text = String::new();
        entry.read_to_string(&mut text)?;
        let fields = parse_desc(&text);
        let (Some(name), Some(version), Some(filename), Some(sha256)) = (
            first(&fields, "NAME"),
            first(&fields, "VERSION"),
            first(&fields, "FILENAME"),
            first(&fields, "SHA256SUM"),
        ) else {
            bail!(
                "{}: package entry {:?} is missing fields",
                path.display(),
                entry.path()?
            );
        };
        packages.insert(
            name.clone(),
            SyncPkg {
                version,
                filename,
                sha256,
                description: first(&fields, "DESC").unwrap_or_default(),
                provides: fields.get("PROVIDES").cloned().unwrap_or_default(),
                depends: fields.get("DEPENDS").cloned().unwrap_or_default(),
                conflicts: fields.get("CONFLICTS").cloned().unwrap_or_default(),
                base: first(&fields, "BASE").unwrap_or_else(|| name.clone()),
                size: 0,
                name,
            },
        );
    }
    Ok(packages)
}

/// Moves local database entries a crash tore (a `desc` without NAME and VERSION) into `aside`, returning their package names.
pub fn set_aside_torn(dir: &Path, aside: &Path) -> Result<Vec<String>> {
    let mut torn = Vec::new();
    for entry in fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .flatten()
    {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let fields = parse_desc(&fs::read_to_string(path.join("desc")).unwrap_or_default());
        if first(&fields, "NAME").is_some() && first(&fields, "VERSION").is_some() {
            continue;
        }
        let dir_name = entry.file_name().to_string_lossy().into_owned();
        fs::create_dir_all(aside)?;
        let mut target = aside.join(&dir_name);
        for n in 1.. {
            if !target.exists() {
                break;
            }
            target = aside.join(format!("{dir_name}.{n}"));
        }
        fs::rename(&path, &target).with_context(|| format!("moving {} aside", path.display()))?;
        torn.push(name_from_entry(&dir_name).to_string());
    }
    Ok(torn)
}

/// The package name in a local database directory name, `name-pkgver-pkgrel`.
pub fn name_from_entry(dir_name: &str) -> &str {
    dir_name.rsplitn(3, '-').nth(2).unwrap_or(dir_name)
}

/// Reads the installed packages from a local database directory, normally `/var/lib/pacman/local`.
/// Installed packages that ship a pacman keyring, such as archlinux-keyring and cachyos-keyring.
pub fn keyring_packages(dir: &Path) -> BTreeSet<String> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            name_from_entry(&entry.file_name().to_string_lossy()).ends_with("-keyring")
                && fs::read_to_string(entry.path().join("files")).is_ok_and(|files| {
                    files
                        .lines()
                        .any(|line| line.starts_with(KEYRINGS) && line.ends_with(".gpg"))
                })
        })
        .map(|entry| name_from_entry(&entry.file_name().to_string_lossy()).to_string())
        .collect()
}

/// Where keyring packages put the keys `pacman-key --populate` imports.
pub const KEYRINGS: &str = "usr/share/pacman/keyrings/";

pub fn read_local_db(dir: &Path) -> Result<BTreeMap<String, LocalPkg>> {
    let mut packages = BTreeMap::new();
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let desc = entry?.path().join("desc");
        if !desc.is_file() {
            continue;
        }
        let text = fs::read_to_string(&desc).with_context(|| format!("reading {}", desc.display()))?;
        let fields = parse_desc(&text);
        let (Some(name), Some(version)) = (first(&fields, "NAME"), first(&fields, "VERSION")) else {
            bail!(
                "{} is missing NAME or VERSION, probably torn by a crash during a switch; `archix recover` repairs it",
                desc.display()
            );
        };
        let explicit = first(&fields, "REASON").is_none_or(|reason| reason == "0");
        let arch = first(&fields, "ARCH").unwrap_or_else(|| "x86_64".to_string());
        let installed_at = first(&fields, "INSTALLDATE").and_then(|secs| secs.parse().ok());
        packages.insert(
            name.clone(),
            LocalPkg {
                name,
                version,
                arch,
                explicit,
                installed_at,
            },
        );
    }
    Ok(packages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;

    const DESC: &str = "%FILENAME%\nacl-2.3.2-1-x86_64.pkg.tar.zst\n\n%NAME%\nacl\n\n%VERSION%\n2.3.2-1\n\n\
                        %DEPENDS%\nglibc\nlibattr\n\n%SHA256SUM%\nabc123\n\n%CSIZE%\n137000\n\n";

    #[test]
    fn parses_multi_value_fields() {
        let fields = parse_desc(DESC);
        assert_eq!(fields["DEPENDS"], vec!["glibc", "libattr"]);
        assert_eq!(fields["NAME"], vec!["acl"]);
    }

    #[test]
    fn names_come_from_entry_directories_like_pacman_reads_them() {
        assert_eq!(name_from_entry("glibc-2.44+r50+g1848099f063e-1"), "glibc");
        assert_eq!(name_from_entry("linux-api-headers-7.2-1"), "linux-api-headers");
        assert_eq!(name_from_entry("zlib-1:1.3.2-3"), "zlib");
    }

    #[test]
    fn reads_a_sync_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("core.db");
        let mut builder = tar::Builder::new(GzEncoder::new(File::create(&path).unwrap(), Compression::default()));
        let mut header = tar::Header::new_gnu();
        header.set_size(DESC.len() as u64);
        header.set_cksum();
        builder
            .append_data(&mut header, "acl-2.3.2-1/desc", DESC.as_bytes())
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap();

        let packages = read_sync_db(&path).unwrap();
        assert_eq!(
            packages["acl"],
            SyncPkg {
                name: "acl".into(),
                version: "2.3.2-1".into(),
                filename: "acl-2.3.2-1-x86_64.pkg.tar.zst".into(),
                sha256: "abc123".into(),
                description: String::new(),
                provides: Vec::new(),
                depends: vec!["glibc".into(), "libattr".into()],
                conflicts: Vec::new(),
                base: "acl".into(),
                size: 137000,
            }
        );
    }

    #[test]
    fn reads_install_reason_from_local_database() {
        let dir = tempfile::tempdir().unwrap();
        for (entry, body) in [
            ("git-2.51.0-1", "%NAME%\ngit\n\n%VERSION%\n2.51.0-1\n\n"),
            (
                "zlib-1:1.3.1-2",
                "%NAME%\nzlib\n\n%VERSION%\n1:1.3.1-2\n\n%REASON%\n1\n\n",
            ),
        ] {
            fs::create_dir(dir.path().join(entry)).unwrap();
            fs::write(dir.path().join(entry).join("desc"), body).unwrap();
        }
        fs::write(dir.path().join("ALPM_DB_VERSION"), "9\n").unwrap();
        let packages = read_local_db(dir.path()).unwrap();
        assert!(packages["git"].explicit);
        assert!(!packages["zlib"].explicit);
        assert_eq!(packages["zlib"].version, "1:1.3.1-2");
    }
}

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use md5::{Digest, Md5};

use crate::fetch::sha256;
use crate::generations::{Generation, now};
use crate::paths::Paths;

/// Enough of a system for pacman, systemd and the kernel to work again; rescue restores these from the cache.
pub const CORE: &[&str] = &[
    "filesystem",
    "glibc",
    "gcc-libs",
    "libgcc",
    "libstdc++",
    "bash",
    "readline",
    "ncurses",
    "coreutils",
    "util-linux",
    "util-linux-libs",
    "systemd",
    "systemd-libs",
    "systemd-sysvcompat",
    "dbus",
    "dbus-broker",
    "dbus-broker-units",
    "pacman",
    "libarchive",
    "curl",
    "openssl",
    "zlib",
    "zstd",
    "xz",
    "bzip2",
    "lz4",
    "acl",
    "attr",
    "libcap",
    "libxcrypt",
    "pam",
    "shadow",
    "kmod",
    "gpgme",
    "libgpg-error",
    "libassuan",
    "archix",
    "linux",
    "linux-lts",
    "linux-zen",
    "linux-hardened",
];

#[derive(Debug, Default)]
pub struct Rescued {
    pub restored: Vec<String>,
    pub missing: Vec<String>,
}

/// Unpacks a generation's core packages (plus `extra`) into `root` from its caches and rewrites their pacman db entries.
pub fn rescue(root: &Path, generation: &Generation, extra: &[String]) -> Result<Rescued> {
    let paths = Paths::under(root);
    let mut rescued = Rescued::default();
    let names: Vec<&String> = generation
        .packages
        .iter()
        .filter(|(name, pkg)| !pkg.build_only && (CORE.contains(&name.as_str()) || extra.contains(name)))
        .map(|(name, _)| name)
        .collect();
    for name in names {
        let pkg = &generation.packages[name];
        let candidates = [paths.packages().join(&pkg.file), paths.pacman_cache.join(&pkg.file)];
        let Some(file) = candidates.into_iter().find(|path| path.is_file()) else {
            rescued
                .missing
                .push(format!("{name} {} ({} isn't cached)", pkg.version, pkg.file));
            continue;
        };
        if !pkg.sha256.is_empty() && sha256(&file)? != pkg.sha256 {
            rescued.missing.push(format!(
                "{name} {} (the cached file doesn't match the lock)",
                pkg.version
            ));
            continue;
        }
        install(root, &paths, &file, pkg.explicit).with_context(|| format!("restoring {name}"))?;
        rescued.restored.push(format!("{name} {}", pkg.version));
    }
    Ok(rescued)
}

fn decoder(path: &Path) -> Result<Box<dyn Read>> {
    let mut magic = [0u8; 4];
    File::open(path)?.read_exact(&mut magic)?;
    let file = File::open(path)?;
    Ok(if magic == [0x28, 0xb5, 0x2f, 0xfd] {
        Box::new(zstd::Decoder::new(file)?)
    } else if magic[..2] == [0x1f, 0x8b] {
        Box::new(flate2::read::GzDecoder::new(file))
    } else {
        bail!("{} is neither zstd- nor gzip-compressed", path.display())
    })
}

fn md5_of(path: &Path) -> Result<String> {
    let mut hasher = Md5::new();
    let mut file = File::open(path)?;
    let mut buf = vec![0; 1 << 16];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Installs one package file the way pacman would, minus hooks and install scripts.
fn install(root: &Path, paths: &Paths, file: &Path, explicit: bool) -> Result<()> {
    let mut info: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut files: Vec<String> = Vec::new();
    let mut archive = tar::Archive::new(decoder(file)?);
    archive.set_preserve_permissions(true);
    archive.set_preserve_ownerships(true);
    archive.set_overwrite(true);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().into_owned();
        if path == ".PKGINFO" {
            let mut text = String::new();
            entry.read_to_string(&mut text)?;
            for line in text.lines() {
                if let Some((key, value)) = line.split_once(" = ") {
                    info.entry(key.to_string()).or_default().push(value.to_string());
                }
            }
            continue;
        }
        if path.starts_with('.') {
            continue;
        }
        let is_dir = entry.header().entry_type().is_dir();
        let backup = info.get("backup").is_some_and(|backups| backups.contains(&path));
        let target = root.join(&path);
        if backup && target.exists() {
            let pacnew = PathBuf::from(format!("{}.pacnew", target.display()));
            entry.unpack(&pacnew)?;
        } else {
            entry.unpack_in(root)?;
        }
        files.push(if is_dir && !path.ends_with('/') {
            format!("{path}/")
        } else {
            path
        });
    }
    let first = |key: &str| info.get(key).and_then(|v| v.first()).cloned().unwrap_or_default();
    let (name, version) = (first("pkgname"), first("pkgver"));
    if name.is_empty() || version.is_empty() {
        bail!("{} has no .PKGINFO", file.display());
    }

    let local = paths.local_db();
    fs::create_dir_all(&local)?;
    if !local.join("ALPM_DB_VERSION").exists() {
        fs::write(local.join("ALPM_DB_VERSION"), "9\n")?;
    }
    for entry in fs::read_dir(&local)?.flatten() {
        let desc = fs::read_to_string(entry.path().join("desc")).unwrap_or_default();
        if desc.lines().skip_while(|l| *l != "%NAME%").nth(1) == Some(name.as_str()) {
            fs::remove_dir_all(entry.path())?;
        }
    }
    let mut desc = String::new();
    let mut field = |key: &str, values: Vec<String>| {
        if !values.is_empty() && values.iter().any(|v| !v.is_empty()) {
            desc.push_str(&format!("%{key}%\n{}\n\n", values.join("\n")));
        }
    };
    field("NAME", vec![name.clone()]);
    field("VERSION", vec![version.clone()]);
    field("BASE", vec![first("pkgbase")]);
    field("DESC", vec![first("pkgdesc")]);
    field("URL", vec![first("url")]);
    field("ARCH", vec![first("arch")]);
    field("BUILDDATE", vec![first("builddate")]);
    field("INSTALLDATE", vec![now().to_string()]);
    field("PACKAGER", vec![first("packager")]);
    field("SIZE", vec![first("size")]);
    if !explicit {
        field("REASON", vec!["1".to_string()]);
    }
    for (key, pkginfo) in [
        ("LICENSE", "license"),
        ("GROUPS", "group"),
        ("DEPENDS", "depend"),
        ("OPTDEPENDS", "optdepend"),
        ("PROVIDES", "provides"),
        ("CONFLICTS", "conflict"),
        ("REPLACES", "replaces"),
    ] {
        field(key, info.get(pkginfo).cloned().unwrap_or_default());
    }
    field("VALIDATION", vec!["none".to_string()]);

    let mut list = format!("%FILES%\n{}\n\n", files.join("\n"));
    let backups: Vec<String> = info
        .get("backup")
        .into_iter()
        .flatten()
        .map(|path| {
            let hash = md5_of(&root.join(path)).unwrap_or_default();
            format!("{path}\t{hash}")
        })
        .collect();
    if !backups.is_empty() {
        list.push_str(&format!("%BACKUP%\n{}\n\n", backups.join("\n")));
    }
    let dir = local.join(format!("{name}-{version}"));
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("desc"), desc)?;
    fs::write(dir.join("files"), list)?;
    Ok(())
}

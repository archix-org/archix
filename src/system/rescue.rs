use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::fetch::sha256;
use crate::generations::{Generation, now};
use crate::paths::Paths;

/// The roots of what rescue restores; activation records their full dependency closure in the generation.
const ROOTS: &[&str] = &[
    "filesystem",
    "systemd",
    "systemd-sysvcompat",
    "dbus-broker",
    "pacman",
    "bash",
    "coreutils",
    "util-linux",
    "kmod",
    "shadow",
    "pam",
    "archix",
    "archix-git",
];

/// The installed dependency closure of `ROOTS` plus every kernel package (the owners of usr/lib/modules/*/vmlinuz), from pacman's local database.
pub fn closure(local_db: &Path) -> Vec<String> {
    closure_of(local_db, ROOTS, true, BTreeMap::new())
}

/// `incoming` maps package names to the (depends, provides) of the versions about to be unpacked, which override the installed ones.
fn closure_of(
    local_db: &Path,
    roots: &[&str],
    with_kernels: bool,
    incoming: BTreeMap<String, (Vec<String>, Vec<String>)>,
) -> Vec<String> {
    let mut depends: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut provider: BTreeMap<String, String> = BTreeMap::new();
    let mut kernels = Vec::new();
    for entry in fs::read_dir(local_db).into_iter().flatten().flatten() {
        let desc = crate::pacman::parse_desc(&fs::read_to_string(entry.path().join("desc")).unwrap_or_default());
        let Some(name) = desc.get("NAME").and_then(|n| n.first()).cloned() else {
            continue;
        };
        for provided in desc.get("PROVIDES").into_iter().flatten() {
            provider
                .entry(crate::aur::srcinfo::dep_name(provided).to_string())
                .or_insert_with(|| name.clone());
        }
        let deps = desc.get("DEPENDS").into_iter().flatten();
        depends.insert(
            name.clone(),
            deps.map(|dep| crate::aur::srcinfo::dep_name(dep).to_string()).collect(),
        );
        let files = fs::read_to_string(entry.path().join("files")).unwrap_or_default();
        if with_kernels
            && files
                .lines()
                .any(|line| line.starts_with("usr/lib/modules/") && line.ends_with("/vmlinuz"))
        {
            kernels.push(name);
        }
    }
    for (name, (deps, provides)) in incoming {
        for provided in provides {
            provider.insert(crate::aur::srcinfo::dep_name(&provided).to_string(), name.clone());
        }
        depends.insert(
            name,
            deps.iter()
                .map(|dep| crate::aur::srcinfo::dep_name(dep).to_string())
                .collect(),
        );
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut queue: Vec<String> = roots.iter().map(|root| root.to_string()).chain(kernels).collect();
    while let Some(want) = queue.pop() {
        let name = if depends.contains_key(&want) {
            want
        } else if let Some(name) = provider.get(&want) {
            name.clone()
        } else {
            continue;
        };
        if seen.insert(name.clone()) {
            queue.extend(depends[&name].iter().cloned());
        }
    }
    seen.into_iter().collect()
}

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
        .filter(|(name, pkg)| {
            let core = if generation.rescue.is_empty() {
                CORE.contains(&name.as_str())
            } else {
                generation.rescue.contains(name)
            };
            !pkg.build_only && (core || extra.contains(name))
        })
        .map(|(name, _)| name)
        .collect();
    let mut dropped = Vec::new();
    let mut kernels = BTreeSet::new();
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
        dropped.extend(install(root, &paths, &file, pkg.explicit).with_context(|| format!("restoring {name}"))?);
        rescued.restored.push(format!("{name} {}", pkg.version));
        let entry = paths.local_db().join(format!("{name}-{}", pkg.version)).join("files");
        kernels.extend(
            fs::read_to_string(entry)
                .unwrap_or_default()
                .lines()
                .filter_map(|path| {
                    let (version, inside) = path.strip_prefix("usr/lib/modules/")?.split_once('/')?;
                    (!inside.is_empty()).then(|| version.to_string())
                }),
        );
    }
    remove_dropped(root, &paths, &dropped)?;
    index_modules(root, &kernels);
    Ok(rescued)
}

/// Writes modules.dep and the other indexes of each kernel version, as pacman's depmod hook would; without them modprobe and udev load nothing.
fn index_modules(root: &Path, versions: &BTreeSet<String>) {
    for version in versions {
        let indexed = match Command::new("depmod").arg("-b").arg(root).arg(version).output() {
            Ok(output) => output.status.success(),
            // The initramfs may have no depmod; the restored root's own runs inside it.
            Err(_) => {
                let root = root.to_path_buf();
                let mut depmod = Command::new("/usr/bin/depmod");
                depmod.arg(version);
                unsafe {
                    depmod.pre_exec(move || {
                        std::os::unix::fs::chroot(&root)?;
                        std::env::set_current_dir("/")
                    });
                }
                depmod.output().is_ok_and(|output| output.status.success())
            }
        };
        if !indexed {
            eprintln!(
                "warning: depmod failed for {version}, so its modules may not load until `depmod {version}` runs"
            );
        }
    }
}

/// Unpacks the files among `files` that belong to pacman's own dependency closure, without pacman, for when a cut-off transaction left it unable to start; returns them as "name version".
pub fn restore_pacman(root: &Path, files: &[PathBuf], explicit: &BTreeMap<String, bool>) -> Result<Vec<String>> {
    let paths = Paths::under(root);
    let mut packages = BTreeMap::new();
    for file in files {
        let package = read_package(root, file, false).with_context(|| format!("reading {}", file.display()))?;
        let field = |key: &str| package.info.get(key).cloned().unwrap_or_default();
        if let Some(name) = field("pkgname").first() {
            packages.insert(
                name.clone(),
                (file, field("pkgver").concat(), field("depend"), field("provides")),
            );
        }
    }
    let incoming = packages
        .iter()
        .map(|(name, (_, _, deps, provides))| (name.clone(), (deps.clone(), provides.clone())))
        .collect();
    let needed = closure_of(&paths.local_db(), &["pacman"], false, incoming);
    let mut restored = Vec::new();
    let mut dropped = Vec::new();
    for (name, (file, version, _, _)) in packages.iter().filter(|(name, _)| needed.contains(*name)) {
        let explicit = explicit.get(name).copied().unwrap_or(false);
        dropped.extend(install(root, &paths, file, explicit).with_context(|| format!("restoring {name}"))?);
        restored.push(format!("{name} {version}"));
    }
    remove_dropped(root, &paths, &dropped)?;
    Ok(restored)
}

/// Only once every entry is written: a file can move between packages, and the package that ships it now must keep it.
fn remove_dropped(root: &Path, paths: &Paths, dropped: &[String]) -> Result<()> {
    let owned = crate::conflicts::owned_files(&paths.local_db());
    for path in dropped.iter().filter(|path| !owned.contains(*path)) {
        let target = root.join(path);
        if fs::symlink_metadata(&target).is_ok_and(|meta| !meta.is_dir()) {
            fs::remove_file(&target).with_context(|| format!("removing {}", target.display()))?;
        }
    }
    unsafe { libc::sync() };
    Ok(())
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

/// What a package file holds, as pacman's database entry for it needs.
struct Package {
    info: BTreeMap<String, Vec<String>>,
    files: Vec<String>,
    /// md5 of each backup file as the package ships it, which pacman later compares against to tell user edits apart.
    backup_md5: BTreeMap<String, String>,
    /// .MTREE, .INSTALL and .CHANGELOG, stored in the entry as mtree, install and changelog.
    meta: Vec<(&'static str, Vec<u8>)>,
}

/// Reads a package file, unpacking it under `root` when `extract`; backup files that already exist get a .pacnew.
fn read_package(root: &Path, file: &Path, extract: bool) -> Result<Package> {
    let mut package = Package {
        info: BTreeMap::new(),
        files: Vec::new(),
        backup_md5: BTreeMap::new(),
        meta: Vec::new(),
    };
    let mut archive = tar::Archive::new(decoder(file)?);
    archive.set_preserve_permissions(true);
    archive.set_preserve_ownerships(unsafe { libc::geteuid() } == 0);
    archive.set_overwrite(true);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().into_owned();
        let meta_name = match path.as_str() {
            ".MTREE" => Some("mtree"),
            ".INSTALL" => Some("install"),
            ".CHANGELOG" => Some("changelog"),
            _ => None,
        };
        if path == ".PKGINFO" {
            let mut text = String::new();
            entry.read_to_string(&mut text)?;
            for line in text.lines() {
                if let Some((key, value)) = line.split_once(" = ") {
                    package.info.entry(key.to_string()).or_default().push(value.to_string());
                }
            }
            continue;
        }
        if let Some(name) = meta_name {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            package.meta.push((name, bytes));
            continue;
        }
        if path.starts_with('.') {
            continue;
        }
        let is_dir = entry.header().entry_type().is_dir();
        let backup = package
            .info
            .get("backup")
            .is_some_and(|backups| backups.contains(&path));
        if backup {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            package
                .backup_md5
                .insert(path.clone(), crate::capture::md5_bytes(&bytes));
            if extract {
                let target = root.join(&path);
                let dest = if target.exists() {
                    PathBuf::from(format!("{}.pacnew", target.display()))
                } else {
                    target
                };
                if let Some(dir) = dest.parent() {
                    fs::create_dir_all(dir)?;
                }
                crate::durable::write(&dest, &bytes)?;
                fs::set_permissions(
                    &dest,
                    <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(entry.header().mode()? & 0o7777),
                )?;
                if unsafe { libc::geteuid() } == 0 {
                    let (uid, gid) = (entry.header().uid()? as u32, entry.header().gid()? as u32);
                    std::os::unix::fs::chown(&dest, Some(uid), Some(gid))?;
                }
            }
        } else if extract {
            let target = root.join(&path);
            if !is_dir && fs::symlink_metadata(&target).is_ok_and(|meta| !meta.is_dir()) {
                fs::remove_file(&target).with_context(|| format!("replacing {}", target.display()))?;
            }
            entry.unpack_in(root)?;
        }
        package.files.push(if is_dir && !path.ends_with('/') {
            format!("{path}/")
        } else {
            path
        });
    }
    Ok(package)
}

/// Installs one package file the way pacman would, minus hooks and install scripts.
fn install(root: &Path, paths: &Paths, file: &Path, explicit: bool) -> Result<Vec<String>> {
    let package = read_package(root, file, true)?;
    write_entry(paths, file, &package, explicit)
}

/// Writes the pacman database entry for a package file without unpacking it, such as over an entry a crash tore; returns the package name.
pub fn register(root: &Path, file: &Path, explicit: bool) -> Result<String> {
    let package = read_package(root, file, false)?;
    write_entry(&Paths::under(root), file, &package, explicit)?;
    Ok(package
        .info
        .get("pkgname")
        .and_then(|v| v.first())
        .cloned()
        .unwrap_or_default())
}

/// What the replaced version of a package installed and the restored one doesn't ship; config files stay.
fn dropped_files(entry: &Path, keep: &[String]) -> Vec<String> {
    let listed = crate::pacman::parse_desc(&fs::read_to_string(entry.join("files")).unwrap_or_default());
    let backups: Vec<&str> = listed
        .get("BACKUP")
        .into_iter()
        .flatten()
        .filter_map(|line| line.split('\t').next())
        .collect();
    listed
        .get("FILES")
        .into_iter()
        .flatten()
        .filter(|path| !path.ends_with('/') && !keep.contains(path) && !backups.contains(&path.as_str()))
        .cloned()
        .collect()
}

/// Replaces the package's database entry, returning the files the replaced version had and this one doesn't.
fn write_entry(paths: &Paths, file: &Path, package: &Package, explicit: bool) -> Result<Vec<String>> {
    let (info, files) = (&package.info, &package.files);
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
    let mut dropped = Vec::new();
    for entry in fs::read_dir(&local)?.flatten() {
        let desc = fs::read_to_string(entry.path().join("desc")).unwrap_or_default();
        // A torn desc names nothing, but pacman still lists the entry by its directory name.
        let dir_name = entry.file_name().to_string_lossy().into_owned();
        if desc.lines().skip_while(|l| *l != "%NAME%").nth(1) == Some(name.as_str())
            || crate::pacman::name_from_entry(&dir_name) == name
        {
            dropped.extend(dropped_files(&entry.path(), files));
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
        .map(|path| format!("{path}\t{}", package.backup_md5.get(path).map_or("", String::as_str)))
        .collect();
    if !backups.is_empty() {
        list.push_str(&format!("%BACKUP%\n{}\n\n", backups.join("\n")));
    }
    let dir = local.join(format!("{name}-{version}"));
    fs::create_dir_all(&dir)?;
    crate::durable::write(&dir.join("desc"), desc.as_bytes())?;
    crate::durable::write(&dir.join("files"), list.as_bytes())?;
    for (name, bytes) in &package.meta {
        crate::durable::write(&dir.join(name), bytes)?;
    }
    Ok(dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny package: .PKGINFO, a binary, a hard link to it and a backup-listed config file.
    fn package(dir: &Path) -> PathBuf {
        let path = dir.join("demo-1.0-1-any.pkg.tar.zst");
        let mut builder = tar::Builder::new(
            zstd::Encoder::new(File::create(&path).unwrap(), 3)
                .unwrap()
                .auto_finish(),
        );
        let mut add = |name: &str, data: &[u8]| {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(0);
            header.set_cksum();
            builder.append_data(&mut header, name, data).unwrap();
        };
        add(
            ".PKGINFO",
            b"pkgname = demo\npkgver = 1.0-1\npkgdesc = a demo\narch = any\nbackup = etc/demo.conf\n",
        );
        add(".MTREE", b"mtree bytes");
        add("usr/bin/demo", b"#!/bin/sh\n");
        add("etc/demo.conf", b"packaged\n");
        let mut link = tar::Header::new_gnu();
        link.set_entry_type(tar::EntryType::Link);
        link.set_size(0);
        link.set_uid(0);
        link.set_gid(0);
        link.set_mtime(0);
        link.set_mode(0o755);
        builder
            .append_link(&mut link, "usr/bin/demo-alias", "usr/bin/demo")
            .unwrap();
        builder.into_inner().unwrap();
        path
    }

    #[test]
    fn rescuing_a_kernel_writes_its_module_index() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(root.join("usr/lib")).unwrap();
        std::os::unix::fs::symlink("usr/lib", root.join("lib")).unwrap();
        let paths = Paths::under(&root);
        fs::create_dir_all(paths.packages()).unwrap();
        let file = paths.packages().join("linux-test-6.0.0-1-x86_64.pkg.tar.zst");
        let mut builder = tar::Builder::new(
            zstd::Encoder::new(File::create(&file).unwrap(), 3)
                .unwrap()
                .auto_finish(),
        );
        for (name, data) in [
            (
                ".PKGINFO",
                &b"pkgname = linux-test\npkgver = 6.0.0-1\narch = x86_64\n"[..],
            ),
            (".MTREE", b"mtree bytes"),
            ("usr/lib/modules/6.0.0-test/vmlinuz", b"kernel"),
            ("usr/lib/modules/6.0.0-test/kernel/foo.ko", b"module"),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, data).unwrap();
        }
        builder.into_inner().unwrap();
        let generation: Generation = serde_json::from_value(serde_json::json!({
            "number": 1, "created": 0, "host": "box", "date": "2026-10-10",
            "packages": { "linux-test": { "version": "6.0.0-1", "repo": "core", "file": "linux-test-6.0.0-1-x86_64.pkg.tar.zst" } },
            "settings": { "remove_undeclared": true, "ignored": [], "keep": 10, "keep_days": 30 },
            "config_commit": null, "config_dirty": false, "rescue": ["linux-test"],
        }))
        .unwrap();
        let rescued = rescue(&root, &generation, &[]).unwrap();
        assert_eq!(rescued.restored, ["linux-test 6.0.0-1"]);
        assert!(
            root.join("usr/lib/modules/6.0.0-test/modules.dep").is_file(),
            "pacman's depmod hook doesn't run without pacman, so rescue has to"
        );
    }

    #[test]
    fn rescue_covers_what_systemd_and_the_kernels_need() {
        let db = tempfile::tempdir().unwrap();
        let entry = |name: &str, desc: &str, files: &str| {
            let dir = db.path().join(format!("{name}-1-1"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("desc"), format!("%NAME%\n{name}\n\n%VERSION%\n1-1\n\n{desc}")).unwrap();
            fs::write(dir.join("files"), format!("%FILES%\n{files}\n\n")).unwrap();
        };
        entry("systemd", "%DEPENDS%\nlibseccomp.so=2-64\n\n", "usr/bin/systemctl");
        entry(
            "libseccomp",
            "%PROVIDES%\nlibseccomp.so=2-64\n\n",
            "usr/lib/libseccomp.so.2",
        );
        entry(
            "linux-cachyos",
            "%DEPENDS%\nkmod\n\n",
            "usr/lib/modules/7.2.9-1-cachyos/vmlinuz",
        );
        entry("kmod", "", "usr/bin/kmod");
        entry("firefox", "", "usr/bin/firefox");
        assert_eq!(closure(db.path()), ["kmod", "libseccomp", "linux-cachyos", "systemd"]);
    }

    #[test]
    fn restoring_pacman_unpacks_only_what_pacman_needs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let paths = Paths::under(&root);
        let pacman = paths.local_db().join("pacman-7.1.0-1");
        fs::create_dir_all(&pacman).unwrap();
        fs::write(pacman.join("desc"), "%NAME%\npacman\n\n%VERSION%\n7.1.0-1\n\n").unwrap();
        let file = package(dir.path());
        assert!(
            restore_pacman(&root, std::slice::from_ref(&file), &BTreeMap::new())
                .unwrap()
                .is_empty()
        );
        assert!(!root.join("usr/bin/demo").exists());
        fs::write(
            pacman.join("desc"),
            "%NAME%\npacman\n\n%VERSION%\n7.1.0-1\n\n%DEPENDS%\ndemo>=1\n\n",
        )
        .unwrap();
        assert_eq!(
            restore_pacman(&root, &[file], &BTreeMap::new()).unwrap(),
            ["demo 1.0-1"]
        );
        assert!(root.join("usr/bin/demo").exists());
        assert!(
            fs::read_to_string(paths.local_db().join("demo-1.0-1/desc"))
                .unwrap()
                .contains("%REASON%\n1\n")
        );
    }

    #[test]
    fn installs_over_existing_files_and_keeps_edited_configs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(&root).unwrap();
        let file = package(dir.path());
        let paths = Paths::under(&root);
        install(&root, &paths, &file, true).unwrap();
        fs::write(root.join("etc/demo.conf"), "edited\n").unwrap();
        install(&root, &paths, &file, true).expect("a second install replaces files and hard links");
        assert_eq!(fs::read_to_string(root.join("etc/demo.conf")).unwrap(), "edited\n");
        assert_eq!(
            fs::read_to_string(root.join("etc/demo.conf.pacnew")).unwrap(),
            "packaged\n"
        );
        assert!(root.join("usr/bin/demo-alias").exists());
        let desc = fs::read_to_string(paths.local_db().join("demo-1.0-1/desc")).unwrap();
        assert!(
            desc.contains("%NAME%\ndemo\n") && desc.contains("%VERSION%\n1.0-1\n"),
            "{desc}"
        );
        let files = fs::read_to_string(paths.local_db().join("demo-1.0-1/files")).unwrap();
        assert!(
            files.contains("usr/bin/demo-alias")
                && files.contains(&format!(
                    "%BACKUP%\netc/demo.conf\t{}",
                    crate::capture::md5_bytes(b"packaged\n")
                )),
            "the backup hash is the packaged file's, so pacman still sees the user's edit as an edit: {files}"
        );
        assert_eq!(
            fs::read(paths.local_db().join("demo-1.0-1/mtree")).unwrap(),
            b"mtree bytes"
        );
        assert_eq!(
            fs::read_dir(paths.local_db()).unwrap().count(),
            2,
            "one entry plus ALPM_DB_VERSION"
        );
    }
}

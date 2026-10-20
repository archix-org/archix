//! Turns an installed package that no repo or the AUR has, one built by hand or gone from the AUR, into a recipe: its files as pacman installed them, with its metadata and install script.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;

use crate::conflicts::unescape;
use crate::pacman::parse_desc;

/// One file, directory or symlink a package installed, as its mtree records it.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub path: String,
    pub kind: Kind,
    pub mode: u32,
    pub uid: u64,
    pub gid: u64,
    pub mtime: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    File { size: u64, sha256: Option<String> },
    Dir,
    Link(String),
}

/// An installed package as pacman's local database describes it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Installed {
    pub name: String,
    pub version: String,
    pub desc: String,
    pub url: String,
    pub arch: String,
    pub licenses: Vec<String>,
    pub groups: Vec<String>,
    pub depends: Vec<String>,
    pub optdepends: Vec<String>,
    pub provides: Vec<String>,
    pub conflicts: Vec<String>,
    pub replaces: Vec<String>,
    pub backup: Vec<String>,
    pub install: Option<String>,
    pub entries: Vec<Entry>,
    /// Config files you edited, as the package file in a cache shipped them.
    pub originals: BTreeMap<String, Vec<u8>>,
    /// Config files you edited that no cached package file has as shipped, which the recipe leaves out.
    pub left_out: Vec<String>,
}

/// Where a recipe's file archive went, and its size.
pub struct Written {
    pub archive: PathBuf,
    pub size: u64,
}

/// The installed `name` from pacman's local database under `root`, if every file it owns is still as pacman installed it (the config files it lets you edit aside), else why not.
pub fn read(root: &Path, name: &str, caches: &[PathBuf]) -> std::result::Result<Installed, String> {
    let local = root.join("var/lib/pacman/local");
    let entry = fs::read_dir(&local)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .find(|dir| {
            let desc = parse_desc(&fs::read_to_string(dir.join("desc")).unwrap_or_default());
            desc.get("NAME")
                .and_then(|names| names.first())
                .is_some_and(|found| found == name)
        })
        .ok_or_else(|| "it isn't installed".to_string())?;
    let desc =
        parse_desc(&fs::read_to_string(entry.join("desc")).map_err(|e| format!("reading its pacman entry: {e}"))?);
    let files = parse_desc(&fs::read_to_string(entry.join("files")).unwrap_or_default());
    let mut mtree = String::new();
    fs::File::open(entry.join("mtree"))
        .map_err(|e| format!("reading its mtree: {e}"))
        .and_then(|file| {
            GzDecoder::new(file)
                .read_to_string(&mut mtree)
                .map_err(|e| format!("reading its mtree: {e}"))
        })?;
    let list = |key: &str| desc.get(key).cloned().unwrap_or_default();
    let first = |key: &str| list(key).into_iter().next().unwrap_or_default();
    let backup: Vec<String> = files
        .get("BACKUP")
        .into_iter()
        .flatten()
        .filter_map(|line| line.split('\t').next())
        .map(str::to_string)
        .collect();
    let entries = parse_mtree(&mtree);
    let version = first("VERSION");
    let (mut originals, mut left_out) = (BTreeMap::new(), Vec::new());
    for entry in &entries {
        let on_disk = root.join(&entry.path);
        if let Kind::File { sha256, .. } = &entry.kind
            && backup.contains(&entry.path)
        {
            let unedited = sha256
                .as_ref()
                .is_some_and(|want| crate::fetch::sha256(&on_disk).is_ok_and(|have| have == *want));
            if !unedited {
                match original(caches, name, &version, &entry.path, sha256.as_deref()) {
                    Some(bytes) => {
                        originals.insert(entry.path.clone(), bytes);
                    }
                    None => left_out.push(entry.path.clone()),
                }
            }
            continue;
        }
        let meta = fs::symlink_metadata(&on_disk).map_err(|_| format!("{} is missing", entry.path))?;
        if let Kind::File { size, sha256 } = &entry.kind {
            let same = match sha256 {
                Some(sha256) => crate::fetch::sha256(&on_disk).is_ok_and(|have| have == *sha256),
                None => meta.len() == *size,
            };
            if !same {
                return Err(format!("{} changed since pacman installed it", entry.path));
            }
        }
    }
    Ok(Installed {
        name: name.to_string(),
        version,
        desc: first("DESC"),
        url: first("URL"),
        arch: first("ARCH"),
        licenses: list("LICENSE"),
        groups: list("GROUPS"),
        depends: list("DEPENDS"),
        optdepends: list("OPTDEPENDS"),
        provides: list("PROVIDES"),
        conflicts: list("CONFLICTS"),
        replaces: list("REPLACES"),
        backup,
        install: fs::read_to_string(entry.join("install")).ok(),
        entries,
        originals,
        left_out,
    })
}

/// A config file as the package shipped it, from that package version's file in one of `caches` (or a directory in one named after the package, as yay and paru keep them), if it matches the sha256 pacman recorded.
fn original(caches: &[PathBuf], name: &str, version: &str, path: &str, sha256: Option<&str>) -> Option<Vec<u8>> {
    let want = sha256?;
    let prefix = format!("{name}-{version}-");
    let candidates = caches
        .iter()
        .flat_map(|cache| [cache.clone(), cache.join(name)])
        .flat_map(|dir| fs::read_dir(dir).into_iter().flatten().flatten())
        .map(|entry| entry.path())
        .filter(|file| {
            let file_name = file.file_name().unwrap_or_default().to_string_lossy();
            file_name
                .strip_prefix(&prefix)
                .and_then(|rest| rest.split_once(".pkg.tar"))
                .is_some_and(|(arch, _)| !arch.contains('-'))
        });
    for file in candidates {
        let Ok(decoder) = decoder(&file) else {
            continue;
        };
        let mut archive = tar::Archive::new(decoder);
        let Ok(entries) = archive.entries() else {
            continue;
        };
        for entry in entries.flatten() {
            let mut entry = entry;
            if entry.path().ok().as_deref() != Some(Path::new(path)) {
                continue;
            }
            let mut bytes = Vec::new();
            if entry.read_to_end(&mut bytes).is_ok() && crate::fetch::sha256_bytes(&bytes) == want {
                return Some(bytes);
            }
            break;
        }
    }
    None
}

/// The entries of a package's mtree, without the package's own metadata files such as `.PKGINFO`.
pub(crate) fn parse_mtree(text: &str) -> Vec<Entry> {
    let mut defaults: HashMap<String, String> = HashMap::new();
    let mut entries = Vec::new();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let Some(first) = words.next() else { continue };
        let keywords = words
            .filter_map(|word| word.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()));
        match first {
            "/set" => defaults.extend(keywords),
            "/unset" => {
                for word in line.split_whitespace().skip(1) {
                    defaults.remove(word);
                }
            }
            path if path.starts_with("./") => {
                let mut fields = defaults.clone();
                fields.extend(keywords);
                let path = unescape(&path[2..]);
                if path.starts_with('.') {
                    continue;
                }
                let number = |key: &str| fields.get(key).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
                let kind = match fields.get("type").map(String::as_str) {
                    Some("dir") => Kind::Dir,
                    Some("link") => Kind::Link(unescape(fields.get("link").map_or("", String::as_str))),
                    _ => Kind::File {
                        size: number("size"),
                        sha256: fields.get("sha256digest").cloned(),
                    },
                };
                entries.push(Entry {
                    path,
                    kind,
                    mode: fields
                        .get("mode")
                        .and_then(|m| u32::from_str_radix(m, 8).ok())
                        .unwrap_or(0o644),
                    uid: number("uid"),
                    gid: number("gid"),
                    mtime: fields
                        .get("time")
                        .and_then(|t| t.split('.').next())
                        .and_then(|t| t.parse().ok())
                        .unwrap_or(0),
                });
            }
            _ => {}
        }
    }
    entries
}

/// Writes `dir` (pkgbuilds/<name>) as a recipe that builds `pkg` again from its files under `root`, returning the size of the file archive.
pub fn write_recipe(pkg: &Installed, root: &Path, dir: &Path, sources: &Path) -> Result<Written> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    fs::create_dir_all(sources).with_context(|| format!("creating {}", sources.display()))?;
    let archive = sources.join(format!(".{}.part", pkg.name));
    let encoder = zstd::Encoder::new(
        fs::File::create(&archive).with_context(|| format!("creating {}", archive.display()))?,
        19,
    )?;
    let mut tar = tar::Builder::new(encoder);
    for entry in pkg.entries.iter().filter(|entry| !pkg.left_out.contains(&entry.path)) {
        let mut header = tar::Header::new_ustar();
        header.set_mode(entry.mode);
        header.set_uid(entry.uid);
        header.set_gid(entry.gid);
        header.set_mtime(entry.mtime);
        match &entry.kind {
            Kind::Dir => {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_size(0);
                tar.append_data(&mut header, &entry.path, std::io::empty())?;
            }
            Kind::Link(target) => {
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_size(0);
                tar.append_link(&mut header, &entry.path, target)?;
            }
            Kind::File { .. } if pkg.originals.contains_key(&entry.path) => {
                let bytes = &pkg.originals[&entry.path];
                header.set_entry_type(tar::EntryType::Regular);
                header.set_size(bytes.len() as u64);
                tar.append_data(&mut header, &entry.path, bytes.as_slice())?;
            }
            Kind::File { .. } => {
                let path = root.join(&entry.path);
                let file = fs::File::open(&path).with_context(|| format!("reading {}", path.display()))?;
                header.set_entry_type(tar::EntryType::Regular);
                header.set_size(file.metadata()?.len());
                tar.append_data(&mut header, &entry.path, file)?;
            }
        }
    }
    tar.into_inner()?.finish()?.sync_all()?;
    let sha256 = crate::fetch::sha256(&archive)?;
    let kept = sources.join(&sha256);
    fs::rename(&archive, &kept).with_context(|| format!("renaming {}", archive.display()))?;
    let (epoch, rest) = match pkg.version.split_once(':') {
        Some((epoch, rest)) => (Some(epoch), rest),
        None => (None, pkg.version.as_str()),
    };
    let (pkgver, pkgrel) = rest.rsplit_once('-').unwrap_or((rest, "1"));
    let mut text = format!(
        "# Made by archix init from the installed {} {}, which no repo or the AUR has; its files are kept in /var/cache/archix/sources, not in the config.\n# archix: private\npkgname={}\n",
        pkg.name, pkg.version, pkg.name
    );
    if let Some(epoch) = epoch {
        let _ = writeln!(text, "epoch={epoch}");
    }
    let _ = writeln!(text, "pkgver={pkgver}\npkgrel={pkgrel}\npkgdesc={}", quote(&pkg.desc));
    let _ = writeln!(text, "arch=({})\nurl={}", quote(&pkg.arch), quote(&pkg.url));
    let backup: Vec<String> = pkg
        .backup
        .iter()
        .filter(|path| !pkg.left_out.contains(path))
        .cloned()
        .collect();
    for (key, values) in [
        ("license", &pkg.licenses),
        ("groups", &pkg.groups),
        ("depends", &pkg.depends),
        ("optdepends", &pkg.optdepends),
        ("provides", &pkg.provides),
        ("conflicts", &pkg.conflicts),
        ("replaces", &pkg.replaces),
        ("backup", &backup),
    ] {
        if !values.is_empty() {
            let quoted: Vec<String> = values.iter().map(|value| quote(value)).collect();
            let _ = writeln!(text, "{key}=({})", quoted.join(" "));
        }
    }
    if let Some(install) = &pkg.install {
        let name = format!("{}.install", pkg.name);
        fs::write(dir.join(&name), install)?;
        let _ = writeln!(text, "install={name}");
    }
    // makepkg would otherwise strip, compress or drop files the package shipped as they were.
    text.push_str("options=('!strip' '!debug' '!lto' '!zipman' '!purge' 'docs' 'libtool' 'staticlibs' 'emptydirs')\n");
    let _ = writeln!(
        text,
        "source=('files.tar.zst')\nnoextract=('files.tar.zst')\nsha256sums=('{}')\n",
        sha256
    );
    text.push_str("package() {\n  bsdtar --numeric-owner -xpf \"$srcdir/files.tar.zst\" -C \"$pkgdir\"\n}\n");
    fs::write(dir.join("PKGBUILD"), text)?;
    Ok(Written {
        size: fs::metadata(&kept)?.len(),
        archive: kept,
    })
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// A reader for a package file, which is zstd- or gzip-compressed.
fn decoder(path: &Path) -> Result<Box<dyn Read>> {
    let mut magic = [0u8; 4];
    fs::File::open(path)?.read_exact(&mut magic)?;
    let file = fs::File::open(path)?;
    Ok(if magic == [0x28, 0xb5, 0x2f, 0xfd] {
        Box::new(zstd::Decoder::new(file)?)
    } else if magic[..2] == [0x1f, 0x8b] {
        Box::new(flate2::read::GzDecoder::new(file))
    } else {
        bail!("{} is neither zstd- nor gzip-compressed", path.display())
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Read, Write};

    use super::*;

    const DESC: &str = "%NAME%\nhandmade\n\n%VERSION%\n1:1.2-3\n\n%DESC%\nmade by 'hand'\n\n%URL%\nhttps://example.org\n\n\
        %ARCH%\nx86_64\n\n%LICENSE%\nMIT\n\n%GROUPS%\ntools\n\n%DEPENDS%\nbash\nglibc>=2.40\n\n%OPTDEPENDS%\npython: for the script\n\n\
        %PROVIDES%\nhandmade-bin=1.2\n\n%CONFLICTS%\nhandmade-git\n\n%REPLACES%\nold-handmade\n\n%REASON%\n1\n\n";

    fn sha(text: &str) -> String {
        crate::fetch::sha256_bytes(text.as_bytes())
    }

    /// A root with handmade installed: a script, a config file, a symlink, a file with a space in its name.
    fn installed_root(script_now: &str, conf_now: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let entry = root.path().join("var/lib/pacman/local/handmade-1:1.2-3");
        fs::create_dir_all(&entry).unwrap();
        fs::write(entry.join("desc"), DESC).unwrap();
        fs::write(entry.join("files"), "%FILES%\netc/\netc/handmade.conf\nusr/\nusr/bin/\nusr/bin/handmade\nusr/bin/hm\n\n%BACKUP%\netc/handmade.conf\tabc\n\n").unwrap();
        fs::write(entry.join("install"), "post_install() {\n  true\n}\n").unwrap();
        let mtree = format!(
            "#mtree\n/set type=file uid=0 gid=0 mode=644\n./.PKGINFO time=1.0 size=1 sha256digest={}\n\
             ./etc time=7.0 mode=755 type=dir\n./etc/handmade.conf time=7.0 size=4 sha256digest={}\n\
             ./usr time=7.0 mode=755 type=dir\n./usr/bin time=7.0 mode=755 type=dir\n\
             ./usr/bin/handmade time=7.5 mode=755 size=9 sha256digest={}\n./usr/bin/hm time=7.0 mode=777 type=link link=handmade\n\
             ./usr/share time=7.0 mode=755 type=dir\n./usr/share/my\\040doc time=7.0 uid=1000 gid=1000 size=3 sha256digest={}\n",
            sha("x"),
            sha("old\n"),
            sha("#!/bin/sh"),
            sha("doc")
        );
        let mut gz = flate2::write::GzEncoder::new(
            fs::File::create(entry.join("mtree")).unwrap(),
            flate2::Compression::default(),
        );
        gz.write_all(mtree.as_bytes()).unwrap();
        gz.finish().unwrap();
        fs::create_dir_all(root.path().join("etc")).unwrap();
        fs::create_dir_all(root.path().join("usr/bin")).unwrap();
        fs::create_dir_all(root.path().join("usr/share")).unwrap();
        fs::write(root.path().join("etc/handmade.conf"), conf_now).unwrap();
        fs::write(root.path().join("usr/bin/handmade"), script_now).unwrap();
        std::os::unix::fs::symlink("handmade", root.path().join("usr/bin/hm")).unwrap();
        fs::write(root.path().join("usr/share/my doc"), "doc").unwrap();
        root
    }

    /// A package file in a cache that ships the config file as it was packaged.
    fn cache_with_original(conf: &str) -> tempfile::TempDir {
        let cache = tempfile::tempdir().unwrap();
        let file = fs::File::create(cache.path().join("handmade-1:1.2-3-x86_64.pkg.tar.zst")).unwrap();
        let mut tar = tar::Builder::new(zstd::Encoder::new(file, 3).unwrap().auto_finish());
        let mut header = tar::Header::new_ustar();
        header.set_size(conf.len() as u64);
        header.set_mode(0o644);
        tar.append_data(&mut header, "etc/handmade.conf", conf.as_bytes())
            .unwrap();
        tar.into_inner().unwrap();
        cache
    }

    fn archive_contents(archive: &Path) -> Vec<String> {
        let mut tar = tar::Archive::new(zstd::Decoder::new(fs::File::open(archive).unwrap()).unwrap());
        let mut seen = Vec::new();
        for entry in tar.entries().unwrap() {
            let mut entry = entry.unwrap();
            let header = entry.header().clone();
            let mut content = String::new();
            entry.read_to_string(&mut content).unwrap();
            seen.push(format!(
                "{} {:o} {} {} {}",
                entry.path().unwrap().display(),
                header.mode().unwrap(),
                header.uid().unwrap(),
                header
                    .link_name()
                    .unwrap()
                    .map(|l| l.display().to_string())
                    .unwrap_or_default(),
                content
            ));
        }
        seen
    }

    #[test]
    fn an_installed_package_reads_back_with_its_files() {
        let root = installed_root("#!/bin/sh", "edited\n");
        let pkg = read(root.path(), "handmade", &[]).unwrap();
        assert_eq!(pkg.version, "1:1.2-3");
        assert_eq!(pkg.depends, ["bash", "glibc>=2.40"]);
        assert_eq!(pkg.backup, ["etc/handmade.conf"]);
        assert_eq!(pkg.install.as_deref(), Some("post_install() {\n  true\n}\n"));
        let paths: Vec<&str> = pkg.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "etc",
                "etc/handmade.conf",
                "usr",
                "usr/bin",
                "usr/bin/handmade",
                "usr/bin/hm",
                "usr/share",
                "usr/share/my doc"
            ]
        );
        let doc = &pkg.entries[7];
        assert_eq!((doc.mode, doc.uid, doc.gid, doc.mtime), (0o644, 1000, 1000, 7));
        assert_eq!(pkg.entries[5].kind, Kind::Link("handmade".into()));
        assert_eq!(pkg.entries[4].mode, 0o755);
        assert_eq!(
            pkg.left_out,
            ["etc/handmade.conf"],
            "an edited config file with no packaged original is left out"
        );
        assert!(pkg.originals.is_empty());
    }

    #[test]
    fn an_edited_config_file_comes_from_the_package_file_in_a_cache() {
        let root = installed_root("#!/bin/sh", "edited\n");
        let cache = cache_with_original("old\n");
        let pkg = read(root.path(), "handmade", &[cache.path().to_path_buf()]).unwrap();
        assert_eq!(pkg.originals["etc/handmade.conf"], b"old\n");
        assert!(pkg.left_out.is_empty());
        let tampered = cache_with_original("not what was installed\n");
        let pkg = read(root.path(), "handmade", &[tampered.path().to_path_buf()]).unwrap();
        assert_eq!(
            pkg.left_out,
            ["etc/handmade.conf"],
            "a cached file that doesn't match the mtree isn't used"
        );
    }

    #[test]
    fn a_package_whose_files_changed_since_it_was_installed_is_not_read() {
        let root = installed_root("#!/bin/bash", "old\n");
        assert_eq!(
            read(root.path(), "handmade", &[]).unwrap_err(),
            "usr/bin/handmade changed since pacman installed it"
        );
        fs::remove_file(root.path().join("usr/share/my doc")).unwrap();
        fs::write(root.path().join("usr/bin/handmade"), "#!/bin/sh").unwrap();
        assert_eq!(
            read(root.path(), "handmade", &[]).unwrap_err(),
            "usr/share/my doc is missing"
        );
        assert_eq!(read(root.path(), "other", &[]).unwrap_err(), "it isn't installed");
    }

    #[test]
    fn the_recipe_keeps_its_files_outside_the_config_and_leaves_edits_out() {
        let root = installed_root("#!/bin/sh", "edited\n");
        let pkg = read(root.path(), "handmade", &[]).unwrap();
        let dir = root.path().join("pkgbuilds/handmade");
        let sources = root.path().join("var/cache/archix/sources");
        let written = write_recipe(&pkg, root.path(), &dir, &sources).unwrap();
        let sha = crate::fetch::sha256(&written.archive).unwrap();
        assert_eq!(written.archive, sources.join(&sha));
        assert_eq!(written.size, fs::metadata(&written.archive).unwrap().len());
        let mut in_config: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        in_config.sort();
        assert_eq!(
            in_config,
            ["PKGBUILD", "handmade.install"],
            "no files of the package go into the config"
        );
        let pkgbuild = fs::read_to_string(dir.join("PKGBUILD")).unwrap();
        let mut lines = pkgbuild.lines();
        assert_eq!(
            lines.next(),
            Some(
                "# Made by archix init from the installed handmade 1:1.2-3, which no repo or the AUR has; its files are kept in /var/cache/archix/sources, not in the config."
            )
        );
        assert_eq!(lines.next(), Some("# archix: private"));
        assert!(!pkgbuild.contains("backup="), "{pkgbuild}");
        assert!(pkgbuild.contains(&format!("sha256sums=('{sha}')")), "{pkgbuild}");
        assert!(pkgbuild.contains("install=handmade.install"), "{pkgbuild}");
        assert_eq!(
            archive_contents(&written.archive),
            [
                "etc 755 0  ",
                "usr 755 0  ",
                "usr/bin 755 0  ",
                "usr/bin/handmade 755 0  #!/bin/sh",
                "usr/bin/hm 777 0 handmade ",
                "usr/share 755 0  ",
                "usr/share/my doc 644 1000  doc",
            ],
            "the edited config file isn't in the archive"
        );
        let cache = cache_with_original("old\n");
        let pkg = read(root.path(), "handmade", &[cache.path().to_path_buf()]).unwrap();
        let written = write_recipe(&pkg, root.path(), &dir, &sources).unwrap();
        assert!(archive_contents(&written.archive).contains(&"etc/handmade.conf 644 0  old\n".to_string()));
        assert!(
            fs::read_to_string(dir.join("PKGBUILD"))
                .unwrap()
                .contains("backup=('etc/handmade.conf')")
        );
    }
}

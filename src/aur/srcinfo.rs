use anyhow::{Result, bail};

use crate::archive::ARCH;

/// What one AUR commit builds, read from its `.SRCINFO`.
#[derive(Debug, Clone, PartialEq)]
pub struct SrcInfo {
    pub base: String,
    pub version: String,
    pub makedepends: Vec<String>,
    pub checkdepends: Vec<String>,
    /// Fingerprints of the keys allowed to sign the sources.
    pub validpgpkeys: Vec<String>,
    pub packages: Vec<SrcPkg>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SrcPkg {
    pub name: String,
    pub arch_any: bool,
    pub depends: Vec<String>,
    pub provides: Vec<String>,
}

impl SrcInfo {
    pub fn package(&self, name: &str) -> Option<&SrcPkg> {
        self.packages.iter().find(|pkg| pkg.name == name)
    }

    /// The file makepkg produces for one of this base's packages.
    pub fn file(&self, pkg: &SrcPkg) -> String {
        let arch = if pkg.arch_any { "any" } else { ARCH };
        format!("{}-{}-{arch}.pkg.tar.zst", pkg.name, self.version)
    }
}

#[derive(Default)]
struct Fields {
    arch: Option<Vec<String>>,
    depends: Option<Vec<String>>,
    provides: Option<Vec<String>>,
}

impl Fields {
    fn add(&mut self, key: &str, value: &str) {
        let arch_specific = format!("_{ARCH}");
        let key = key.strip_suffix(arch_specific.as_str()).unwrap_or(key);
        let slot = match key {
            "arch" => &mut self.arch,
            "depends" => &mut self.depends,
            "provides" => &mut self.provides,
            _ => return,
        };
        slot.get_or_insert_with(Vec::new).push(value.to_string());
    }
}

/// Parses `.SRCINFO`: a pkgbase section of defaults, then pkgname sections whose arrays replace the defaults.
pub fn parse(text: &str) -> Result<SrcInfo> {
    let (mut base, mut pkgver, mut pkgrel, mut epoch) = (None, None, None, None);
    let (mut makedepends, mut checkdepends, mut validpgpkeys) = (Vec::new(), Vec::new(), Vec::new());
    let mut defaults = Fields::default();
    let mut packages: Vec<(String, Fields)> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once(" = ") else {
            continue;
        };
        let arch_specific = format!("_{ARCH}");
        match (key, packages.last_mut()) {
            ("pkgbase", _) => base = Some(value.to_string()),
            ("pkgname", _) => packages.push((value.to_string(), Fields::default())),
            ("pkgver", None) => pkgver = Some(value),
            ("pkgrel", None) => pkgrel = Some(value),
            ("epoch", None) => epoch = Some(value),
            ("makedepends", None) => makedepends.push(value.to_string()),
            ("checkdepends", None) => checkdepends.push(value.to_string()),
            ("validpgpkeys", None) => validpgpkeys.push(value.to_string()),
            (k, None) if k == format!("makedepends{arch_specific}") => makedepends.push(value.to_string()),
            (k, None) if k == format!("checkdepends{arch_specific}") => checkdepends.push(value.to_string()),
            (key, None) => defaults.add(key, value),
            (key, Some((_, fields))) => fields.add(key, value),
        }
    }
    let (Some(base), Some(pkgver), Some(pkgrel)) = (base, pkgver, pkgrel) else {
        bail!(".SRCINFO is missing pkgbase, pkgver or pkgrel");
    };
    if packages.is_empty() {
        bail!(".SRCINFO for {base} has no pkgname");
    }
    let version = match epoch {
        Some(epoch) if epoch != "0" => format!("{epoch}:{pkgver}-{pkgrel}"),
        _ => format!("{pkgver}-{pkgrel}"),
    };
    let packages = packages
        .into_iter()
        .map(|(name, fields)| {
            let arch = fields.arch.or_else(|| defaults.arch.clone()).unwrap_or_default();
            SrcPkg {
                name,
                arch_any: arch.iter().any(|a| a == "any"),
                depends: fields.depends.or_else(|| defaults.depends.clone()).unwrap_or_default(),
                provides: fields
                    .provides
                    .or_else(|| defaults.provides.clone())
                    .unwrap_or_default(),
            }
        })
        .collect();
    Ok(SrcInfo {
        base,
        version,
        makedepends,
        checkdepends,
        validpgpkeys,
        packages,
    })
}

/// The package name in a dependency string such as `python>=3.12` or `sh: optional`.
pub fn dep_name(dep: &str) -> &str {
    let end = dep.find(['<', '>', '=', ':']).unwrap_or(dep.len());
    dep[..end].trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPLIT: &str = "pkgbase = foo\n\tpkgdesc = d\n\tpkgver = 1.2\n\tpkgrel = 3\n\tepoch = 2\n\
        \tarch = x86_64\n\tmakedepends = cmake\n\tmakedepends_x86_64 = nasm\n\tcheckdepends = python\n\
        \tdepends = glibc\n\tdepends_x86_64 = lib32-glibc\n\tdepends_aarch64 = nope\n\tvalidpgpkeys = 7C20ADCD35D9797789B6BCC046D62DD9F1DE636E\n\n\
        pkgname = foo\n\tprovides = foo-bin=1.2\n\n\
        pkgname = foo-docs\n\tarch = any\n\tdepends = foo>=1.2\n";

    #[test]
    fn split_packages_inherit_and_override() {
        let info = parse(SPLIT).unwrap();
        assert_eq!(info.base, "foo");
        assert_eq!(info.version, "2:1.2-3");
        assert_eq!(info.makedepends, ["cmake", "nasm"]);
        assert_eq!(info.checkdepends, ["python"]);
        assert_eq!(info.validpgpkeys, ["7C20ADCD35D9797789B6BCC046D62DD9F1DE636E"]);
        let foo = info.package("foo").unwrap();
        assert_eq!(foo.depends, ["glibc", "lib32-glibc"]);
        assert_eq!(foo.provides, ["foo-bin=1.2"]);
        assert_eq!(info.file(foo), "foo-2:1.2-3-x86_64.pkg.tar.zst");
        let docs = info.package("foo-docs").unwrap();
        assert_eq!(docs.depends, ["foo>=1.2"], "a pkgname's depends replace the pkgbase's");
        assert!(docs.arch_any);
        assert_eq!(info.file(docs), "foo-docs-2:1.2-3-any.pkg.tar.zst");
    }

    #[test]
    fn dependency_names() {
        assert_eq!(dep_name("python>=3.12"), "python");
        assert_eq!(dep_name("java-runtime=21"), "java-runtime");
        assert_eq!(dep_name("glibc"), "glibc");
    }
}

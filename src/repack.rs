//! Turns an installed package that no repo or the AUR has, one built by hand or gone from the AUR, into a recipe: its files as pacman installed them, with its metadata and install script.

use std::path::Path;

use anyhow::Result;

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
}

/// The installed `name` from pacman's local database under `root`, if every file it owns is still as pacman installed it (the config files it lets you edit aside), else why not.
pub fn read(_root: &Path, _name: &str) -> std::result::Result<Installed, String> {
    Err("not written yet".to_string())
}

/// Writes `dir` (pkgbuilds/<name>) as a recipe that builds `pkg` again from its files under `root`, returning the size of the file archive.
pub fn write_recipe(_pkg: &Installed, _root: &Path, _dir: &Path) -> Result<u64> {
    Ok(0)
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

    #[test]
    fn an_installed_package_reads_back_with_its_files() {
        let root = installed_root("#!/bin/sh", "edited\n");
        let pkg = read(root.path(), "handmade").unwrap();
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
    }

    #[test]
    fn a_package_whose_files_changed_since_it_was_installed_is_not_read() {
        let root = installed_root("#!/bin/bash", "old\n");
        assert_eq!(
            read(root.path(), "handmade").unwrap_err(),
            "usr/bin/handmade changed since pacman installed it"
        );
        fs::remove_file(root.path().join("usr/share/my doc")).unwrap();
        fs::write(root.path().join("usr/bin/handmade"), "#!/bin/sh").unwrap();
        assert_eq!(
            read(root.path(), "handmade").unwrap_err(),
            "usr/share/my doc is missing"
        );
        assert_eq!(read(root.path(), "other").unwrap_err(), "it isn't installed");
    }

    #[test]
    fn the_recipe_builds_the_same_package_from_its_files() {
        let root = installed_root("#!/bin/sh", "edited\n");
        let pkg = read(root.path(), "handmade").unwrap();
        let dir = root.path().join("pkgbuilds/handmade");
        let size = write_recipe(&pkg, root.path(), &dir).unwrap();
        let archive = dir.join("files.tar.zst");
        assert_eq!(size, fs::metadata(&archive).unwrap().len());
        let expected = format!(
            "# Made by archix init from the installed handmade 1:1.2-3, which no repo or the AUR has: its files as pacman installed them.\n\
             pkgname=handmade\nepoch=1\npkgver=1.2\npkgrel=3\npkgdesc='made by '\\''hand'\\'''\narch=('x86_64')\nurl='https://example.org'\n\
             license=('MIT')\ngroups=('tools')\ndepends=('bash' 'glibc>=2.40')\noptdepends=('python: for the script')\n\
             provides=('handmade-bin=1.2')\nconflicts=('handmade-git')\nreplaces=('old-handmade')\nbackup=('etc/handmade.conf')\n\
             install=handmade.install\noptions=('!strip' '!debug' '!lto' '!zipman' '!purge' 'docs' 'libtool' 'staticlibs' 'emptydirs')\n\
             source=('files.tar.zst')\nnoextract=('files.tar.zst')\nsha256sums=('{}')\n\n\
             package() {{\n  bsdtar --numeric-owner -xpf \"$srcdir/files.tar.zst\" -C \"$pkgdir\"\n}}\n",
            crate::fetch::sha256(&archive).unwrap()
        );
        assert_eq!(fs::read_to_string(dir.join("PKGBUILD")).unwrap(), expected);
        assert_eq!(
            fs::read_to_string(dir.join("handmade.install")).unwrap(),
            "post_install() {\n  true\n}\n"
        );
        let mut tar = tar::Archive::new(zstd::Decoder::new(fs::File::open(&archive).unwrap()).unwrap());
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
        assert_eq!(
            seen,
            [
                "etc 755 0  ",
                "etc/handmade.conf 644 0  edited\n",
                "usr 755 0  ",
                "usr/bin 755 0  ",
                "usr/bin/handmade 755 0  #!/bin/sh",
                "usr/bin/hm 777 0 handmade ",
                "usr/share 755 0  ",
                "usr/share/my doc 644 1000  doc",
            ]
        );
    }
}

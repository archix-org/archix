//! Pins what a recipe downloads: the commit each git source was built at and the sha256 of each download its checksums skip, so a rebuild of the same lock builds the same code.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;

use super::srcinfo::SrcInfo;

/// A source whose content a rebuild could get differently.
#[derive(Debug, Clone, PartialEq)]
pub struct Floating {
    /// The array it's in, `source` or `source_x86_64`.
    pub array: String,
    pub index: usize,
    /// The entry as `.SRCINFO` lists it, which is the key archix.lock records its pin under.
    pub entry: String,
    pub kind: Kind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    /// A git source following a branch, or the remote's default one: the clone makepkg keeps by `name`.
    Git { name: String, branch: Option<String> },
    /// A download whose every checksum is SKIP, saved as `file`.
    Unchecked { file: String },
}

/// The sources of a recipe that a rebuild could get differently.
pub fn floating(_info: &SrcInfo) -> Vec<Floating> {
    Vec::new()
}

/// What a finished build in `dir` (makepkg's SRCDEST) got for each floating source: a commit or a sha256.
pub fn pins(_dir: &Path, _info: &SrcInfo) -> Result<BTreeMap<String, String>> {
    Ok(BTreeMap::new())
}

/// Lines to append to a PKGBUILD so it builds `pins` again: git sources at their commit, unchecked downloads checked against their sha256.
pub fn pinned_tail(_info: &SrcInfo, _pins: &BTreeMap<String, String>) -> String {
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRCINFO: &str = "pkgbase = foo\n\tpkgver = 1\n\tpkgrel = 1\n\tarch = x86_64\n\
        \tsource = foo::git+https://example.org/foo.git\n\
        \tsource = git+https://example.org/bar.git#branch=dev?signed\n\
        \tsource = git+https://example.org/fixed.git#tag=v1\n\
        \tsource = data.txt::https://example.org/data\n\
        \tsource = https://example.org/checked.tar.gz\n\
        \tsource = local.patch\n\
        \tsha256sums = SKIP\n\tsha256sums = SKIP\n\tsha256sums = SKIP\n\tsha256sums = SKIP\n\tsha256sums = abc\n\tsha256sums = SKIP\n\
        \tsource_x86_64 = https://example.org/bin-x86_64\n\tb2sums_x86_64 = SKIP\n\npkgname = foo\n";

    fn info() -> SrcInfo {
        super::super::srcinfo::parse(SRCINFO).unwrap()
    }

    #[test]
    fn only_moving_git_refs_and_unchecked_downloads_float() {
        let found = floating(&info());
        let kinds: Vec<(&str, usize, &Kind)> = found.iter().map(|f| (f.array.as_str(), f.index, &f.kind)).collect();
        assert_eq!(
            kinds,
            [
                (
                    "source",
                    0,
                    &Kind::Git {
                        name: "foo".into(),
                        branch: None
                    }
                ),
                (
                    "source",
                    1,
                    &Kind::Git {
                        name: "bar".into(),
                        branch: Some("dev".into())
                    }
                ),
                (
                    "source",
                    3,
                    &Kind::Unchecked {
                        file: "data.txt".into()
                    }
                ),
                (
                    "source_x86_64",
                    0,
                    &Kind::Unchecked {
                        file: "bin-x86_64".into()
                    }
                ),
            ]
        );
        assert_eq!(found[1].entry, "git+https://example.org/bar.git#branch=dev?signed");
    }

    #[test]
    fn a_finished_build_pins_the_commit_it_cloned_and_the_files_it_downloaded() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "init.defaultBranch=main",
                ])
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(status.status.success(), "{status:?}");
            String::from_utf8(status.stdout).unwrap().trim().to_string()
        };
        git(&["init", "-q", "work"]);
        std::fs::write(dir.path().join("work/f"), "1").unwrap();
        git(&["-C", "work", "add", "f"]);
        git(&["-C", "work", "commit", "-qm", "one"]);
        git(&["-C", "work", "checkout", "-qb", "dev"]);
        std::fs::write(dir.path().join("work/f"), "2").unwrap();
        git(&["-C", "work", "commit", "-qam", "two"]);
        let dev = git(&["-C", "work", "rev-parse", "HEAD"]);
        git(&["-C", "work", "checkout", "-q", "main"]);
        let main = git(&["-C", "work", "rev-parse", "HEAD"]);
        git(&["clone", "-q", "--mirror", "work", "foo"]);
        git(&["clone", "-q", "--mirror", "work", "bar"]);
        std::fs::write(dir.path().join("data.txt"), "abc").unwrap();
        std::fs::write(dir.path().join("bin-x86_64"), "").unwrap();
        let pins = pins(dir.path(), &info()).unwrap();
        assert_eq!(pins["foo::git+https://example.org/foo.git"], main);
        assert_eq!(pins["git+https://example.org/bar.git#branch=dev?signed"], dev);
        assert_eq!(
            pins["data.txt::https://example.org/data"],
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(pins.len(), 4);
    }

    #[test]
    fn a_pinned_recipe_names_commits_and_checks_downloads() {
        let pins = BTreeMap::from([
            ("foo::git+https://example.org/foo.git".to_string(), "c1".to_string()),
            (
                "git+https://example.org/bar.git#branch=dev?signed".to_string(),
                "c2".to_string(),
            ),
            ("data.txt::https://example.org/data".to_string(), "h1".to_string()),
            ("https://example.org/bin-x86_64".to_string(), "h2".to_string()),
        ]);
        assert_eq!(
            pinned_tail(&info(), &pins),
            "\n# archix: the sources archix.lock pinned\n\
             source=('foo::git+https://example.org/foo.git#commit=c1' 'git+https://example.org/bar.git#commit=c2?signed' 'git+https://example.org/fixed.git#tag=v1' 'data.txt::https://example.org/data' 'https://example.org/checked.tar.gz' 'local.patch')\n\
             sha256sums=('SKIP' 'SKIP' 'SKIP' 'h1' 'abc' 'SKIP')\n\
             sha256sums_x86_64=('h2')\n"
        );
        assert_eq!(pinned_tail(&info(), &BTreeMap::new()), "");
    }
}

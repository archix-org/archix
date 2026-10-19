//! Pins what a recipe downloads: the commit each git source was built at and the sha256 of each download its checksums skip, so a rebuild of the same lock builds the same code.

use std::collections::BTreeMap;
use std::path::Path;

use std::process::Command;

use anyhow::{Context, Result, bail};

use super::srcinfo::SrcInfo;

/// Schemes makepkg clones rather than downloads.
const VCS: &[&str] = &["git", "hg", "svn", "bzr", "fossil"];

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
pub fn floating(info: &SrcInfo) -> Vec<Floating> {
    let mut found = Vec::new();
    for (array, entries) in &info.sources {
        let suffix = &array["source".len()..];
        let sums: Vec<&Vec<String>> = info
            .sums
            .iter()
            .filter(|(name, _)| match suffix {
                "" => name.ends_with("sums"),
                suffix => name.ends_with(&format!("sums{suffix}")),
            })
            .map(|(_, sums)| sums)
            .collect();
        for (index, entry) in entries.iter().enumerate() {
            let source = Source::parse(entry);
            let scheme = source.url.split_once("://").map(|(scheme, _)| scheme);
            let kind = if source.is_git() {
                let branch = match source.fragment {
                    None => None,
                    Some(fragment) => match fragment.strip_prefix("branch=") {
                        Some(branch) => Some(branch.to_string()),
                        None => continue,
                    },
                };
                Kind::Git {
                    name: source.name(),
                    branch,
                }
            } else if scheme.is_some_and(|scheme| !scheme.contains('+') && !VCS.contains(&scheme))
                && !sums.is_empty()
                && sums.iter().all(|sums| sums.get(index).is_some_and(|sum| sum == "SKIP"))
            {
                Kind::Unchecked { file: source.name() }
            } else {
                continue;
            };
            found.push(Floating {
                array: array.clone(),
                index,
                entry: entry.clone(),
                kind,
            });
        }
    }
    found
}

/// What a finished build in `dir` (makepkg's SRCDEST) got for each floating source: a commit or a sha256; one it can't tell is left unpinned with a warning.
pub fn pins(dir: &Path, info: &SrcInfo) -> BTreeMap<String, String> {
    let mut pins = BTreeMap::new();
    for source in floating(info) {
        match pin(dir, &source.kind) {
            Ok(pin) => {
                pins.insert(source.entry, pin);
            }
            Err(e) => eprintln!(
                "warning: couldn't tell what {} was when built, so archix.lock doesn't pin it: {e:#}",
                source.entry
            ),
        }
    }
    pins
}

fn pin(dir: &Path, kind: &Kind) -> Result<String> {
    match kind {
        Kind::Git { name, branch } => {
            let reference = match branch {
                Some(branch) => format!("refs/heads/{branch}^{{commit}}"),
                None => "HEAD^{commit}".to_string(),
            };
            let out = Command::new("git")
                .args(["-c", "safe.directory=*", "-C"])
                .arg(dir.join(name))
                .args(["rev-parse", "--verify", &reference])
                .output()
                .context("running git")?;
            if !out.status.success() {
                bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
            }
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        }
        Kind::Unchecked { file } => crate::fetch::sha256(&dir.join(file)),
    }
}

/// Lines to append to a PKGBUILD so it builds `pins` again: git sources at their commit, unchecked downloads checked against their sha256.
pub fn pinned_tail(info: &SrcInfo, pins: &BTreeMap<String, String>) -> String {
    let mut sources = info.sources.clone();
    let mut sums: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for source in floating(info) {
        let Some(pin) = pins.get(&source.entry) else {
            continue;
        };
        match source.kind {
            Kind::Git { .. } => {
                let parsed = Source::parse(&source.entry);
                let prefix = parsed.rename.map(|name| format!("{name}::")).unwrap_or_default();
                let signed = if parsed.signed { "?signed" } else { "" };
                sources
                    .get_mut(&source.array)
                    .expect("floating sources come from an array")[source.index] =
                    format!("{prefix}{}#commit={pin}{signed}", parsed.url);
            }
            Kind::Unchecked { .. } => {
                let suffix = &source.array["source".len()..];
                let array = format!("sha256sums{suffix}");
                let len = info.sources[&source.array].len();
                let sha256sums = sums.entry(array.clone()).or_insert_with(|| {
                    info.sums
                        .get(&array)
                        .cloned()
                        .unwrap_or_else(|| vec!["SKIP".to_string(); len])
                });
                sha256sums[source.index] = pin.clone();
            }
        }
    }
    let changed_sources = sources
        .iter()
        .filter(|(array, entries)| info.sources.get(*array) != Some(entries));
    let lines: Vec<String> = changed_sources
        .chain(sums.iter())
        .map(|(array, entries)| {
            format!(
                "{array}=({})",
                entries.iter().map(|e| quote(e)).collect::<Vec<_>>().join(" ")
            )
        })
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    format!("\n# archix: the sources archix.lock pinned\n{}\n", lines.join("\n"))
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// One `source` entry: `[name::]url[#fragment][?signed]`.
struct Source<'a> {
    rename: Option<&'a str>,
    url: &'a str,
    fragment: Option<&'a str>,
    signed: bool,
}

impl<'a> Source<'a> {
    fn parse(entry: &'a str) -> Source<'a> {
        let (rename, rest) = match entry.split_once("::") {
            Some((name, rest)) => (Some(name), rest),
            None => (None, entry),
        };
        let (rest, signed) = match rest.strip_suffix("?signed") {
            Some(rest) => (rest, true),
            None => (rest, false),
        };
        let (url, fragment) = match rest.split_once('#') {
            Some((url, fragment)) => (url, Some(fragment)),
            None => (rest, None),
        };
        Source {
            rename,
            url,
            fragment,
            signed,
        }
    }

    fn is_git(&self) -> bool {
        self.url.starts_with("git+") || self.url.starts_with("git://")
    }

    /// The name makepkg saves the source under: the given one, else the URL's last part, without `.git` for a clone.
    fn name(&self) -> String {
        if let Some(name) = self.rename {
            return name.to_string();
        }
        let last = self.url.trim_end_matches('/').rsplit('/').next().unwrap_or(self.url);
        match self.is_git() {
            true => last.strip_suffix(".git").unwrap_or(last).to_string(),
            false => last.to_string(),
        }
    }
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
        \tsource = svn+https://example.org/svn/trunk\n\
        \tsource = git://example.org/plain.git\n\
        \tsha256sums = SKIP\n\tsha256sums = SKIP\n\tsha256sums = SKIP\n\tsha256sums = SKIP\n\tsha256sums = abc\n\tsha256sums = SKIP\n\tsha256sums = SKIP\n\tsha256sums = SKIP\n\
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
                    "source",
                    7,
                    &Kind::Git {
                        name: "plain".into(),
                        branch: None
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
        git(&["clone", "-q", "--mirror", "work", "plain"]);
        std::fs::write(dir.path().join("data.txt"), "abc").unwrap();
        std::fs::write(dir.path().join("bin-x86_64"), "").unwrap();
        let pins = pins(dir.path(), &info());
        assert_eq!(pins["foo::git+https://example.org/foo.git"], main);
        assert_eq!(pins["git+https://example.org/bar.git#branch=dev?signed"], dev);
        assert_eq!(
            pins["data.txt::https://example.org/data"],
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(pins["git://example.org/plain.git"], main);
        assert_eq!(pins.len(), 5);
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
             source=('foo::git+https://example.org/foo.git#commit=c1' 'git+https://example.org/bar.git#commit=c2?signed' 'git+https://example.org/fixed.git#tag=v1' 'data.txt::https://example.org/data' 'https://example.org/checked.tar.gz' 'local.patch' 'svn+https://example.org/svn/trunk' 'git://example.org/plain.git')\n\
             sha256sums=('SKIP' 'SKIP' 'SKIP' 'h1' 'abc' 'SKIP' 'SKIP' 'SKIP')\n\
             sha256sums_x86_64=('h2')\n"
        );
        assert_eq!(pinned_tail(&info(), &BTreeMap::new()), "");
    }
}

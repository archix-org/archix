use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// Local mirrors of AUR git repositories, one per package base.
pub struct AurGit {
    url: String,
    dir: PathBuf,
}

impl AurGit {
    pub fn new(url: &str, cache: &Path) -> AurGit {
        AurGit {
            url: url.trim_end_matches('/').to_string(),
            dir: cache.join("aur-git").join(super::index::url_key(url)),
        }
    }

    fn repo(&self, base: &str) -> PathBuf {
        self.dir.join(format!("{base}.git"))
    }

    fn git(&self, base: &str, args: &[&str]) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.git_bytes(base, args)?).into_owned())
    }

    fn git_bytes(&self, base: &str, args: &[&str]) -> Result<Vec<u8>> {
        let output = Command::new("git")
            .args(["-c", "safe.directory=*", "-C"])
            .arg(self.repo(base))
            .args(args)
            .stderr(Stdio::piped())
            .output()
            .context("running git")?;
        if !output.status.success() {
            bail!(
                "git {} for {base}: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(output.stdout)
    }

    /// Clones or fetches the base's repository; the AUR serves a base's PKGBUILD history at `<url>/<base>.git`.
    pub fn sync(&self, base: &str) -> Result<()> {
        let repo = self.repo(base);
        if repo.is_dir() {
            self.git(
                base,
                &["fetch", "--quiet", "--prune", "origin", "+refs/heads/*:refs/heads/*"],
            )?;
            return Ok(());
        }
        fs::create_dir_all(&self.dir)?;
        let url = format!("{}/{base}.git", self.url);
        let tmp = self.dir.join(format!(".{base}.git.{}", std::process::id()));
        let status = Command::new("git")
            .args(["clone", "--quiet", "--mirror", &url])
            .arg(&tmp)
            .status()
            .context("running git clone")?;
        if !status.success() {
            let _ = fs::remove_dir_all(&tmp);
            bail!("could not clone {url}; the package base may have been deleted from the AUR");
        }
        crate::durable::rename(&tmp, &repo).or_else(|e| if repo.is_dir() { Ok(()) } else { Err(e) })?;
        Ok(())
    }

    pub fn head(&self, base: &str) -> Result<String> {
        Ok(self.git(base, &["rev-parse", "HEAD"])?.trim().to_string())
    }

    /// The full hash of `commit`, which may be abbreviated; an error when the base's history doesn't have it.
    pub fn full_commit(&self, base: &str, commit: &str) -> Result<String> {
        Ok(self
            .git(
                base,
                &["rev-parse", "--verify", "--quiet", &format!("{commit}^{{commit}}")],
            )?
            .trim()
            .to_string())
    }

    pub fn has_commit(&self, base: &str, commit: &str) -> bool {
        self.git(base, &["cat-file", "-e", &format!("{commit}^{{commit}}")])
            .is_ok()
    }

    pub fn show(&self, base: &str, commit: &str, path: &str) -> Result<String> {
        self.git(base, &["show", &format!("{commit}:{path}")])
    }

    /// What changed between two commits, .SRCINFO included: archix takes dependencies, sources and keys from it.
    pub fn diff(&self, base: &str, from: &str, to: &str) -> Result<String> {
        let raw = self.git(base, &["diff", "--raw", "-z", "--no-abbrev", "--no-renames", from, to])?;
        let mut fields = raw.split('\0');
        let (mut binary, mut text) = (String::new(), Vec::new());
        while let (Some(meta), Some(path)) = (fields.next(), fields.next()) {
            let mut ids = meta.split(' ').skip(2);
            let mut side = || -> Result<Option<Vec<u8>>> {
                let id = ids
                    .next()
                    .with_context(|| format!("git diff --raw for {base}: {meta}"))?;
                if id.bytes().all(|b| b == b'0') {
                    return Ok(None);
                }
                self.git_bytes(base, &["cat-file", "blob", id]).map(Some)
            };
            let (was, now) = (side()?, side()?);
            let binary_was = was.as_deref().filter(|content| as_text(content).is_none());
            let name = escaped(path);
            match (binary_was, now.as_deref().map(|content| (content, as_text(content)))) {
                (None, None | Some((_, Some(_)))) => text.push(path),
                (Some(was), None) => binary.push_str(&format!("==> {name}: binary file removed ({})\n", summary(was))),
                (Some(was), Some((_, Some(now)))) => binary.push_str(&format!(
                    "==> {name}: was a binary file ({}), now\n{}\n",
                    summary(was),
                    escaped(now)
                )),
                (_, Some((now, None))) => {
                    let was = was.map(|was| format!("was {}, ", summary(&was))).unwrap_or_default();
                    binary.push_str(&format!(
                        "==> {name}: binary file, not shown ({was}now {})\n",
                        summary(now)
                    ));
                }
            }
        }
        if text.is_empty() {
            return Ok(binary);
        }
        let mut args = vec![
            "--literal-pathspecs",
            "diff",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            from,
            to,
            "--",
        ];
        args.extend(text);
        Ok(format!("{}{binary}", escaped(&self.git(base, &args)?)))
    }

    /// Every file of a commit, PKGBUILD first and .SRCINFO last, so install scripts and patches are reviewed too.
    pub fn show_all(&self, base: &str, commit: &str) -> Result<String> {
        let listing = self.git(base, &["ls-tree", "-r", "-z", "--name-only", commit])?;
        let mut files: Vec<&str> = listing.split('\0').filter(|file| !file.is_empty()).collect();
        files.sort_by_key(|file| (*file != "PKGBUILD", *file == ".SRCINFO", file.to_string()));
        let mut shown = String::new();
        for file in files {
            let content = self.git_bytes(base, &["show", &format!("{commit}:{file}")])?;
            let name = escaped(file);
            match as_text(&content) {
                Some(text) => shown.push_str(&format!("==> {name}\n{}\n", escaped(text))),
                None => shown.push_str(&format!("==> {name}: binary file, not shown ({})\n", summary(&content))),
            }
        }
        Ok(shown)
    }

    /// Commits touching the base, newest first.
    pub fn history(&self, base: &str) -> Result<Vec<String>> {
        Ok(self
            .git(base, &["rev-list", "HEAD"])?
            .lines()
            .map(str::to_string)
            .collect())
    }

    /// Writes the files of `commit` into `dest`, which must not exist yet.
    pub fn export(&self, base: &str, commit: &str, dest: &Path) -> Result<()> {
        fs::create_dir_all(dest)?;
        let archive = Command::new("git")
            .args(["-c", "safe.directory=*", "-C"])
            .arg(self.repo(base))
            .args(["archive", "--format=tar", commit])
            .output()
            .context("running git archive")?;
        if !archive.status.success() {
            bail!(
                "git archive {commit} for {base}: {}",
                String::from_utf8_lossy(&archive.stderr).trim()
            );
        }
        tar::Archive::new(archive.stdout.as_slice())
            .unpack(dest)
            .with_context(|| format!("unpacking {base} at {commit}"))
    }
}

/// The content as text, unless it's binary: a NUL byte or invalid UTF-8.
fn as_text(content: &[u8]) -> Option<&str> {
    std::str::from_utf8(content).ok().filter(|text| !text.contains('\0'))
}

fn summary(content: &[u8]) -> String {
    format!(
        "{} bytes, sha256 {}",
        content.len(),
        crate::fetch::sha256_bytes(content)
    )
}

/// Text for a trust prompt: control and bidirectional-override characters are written as escapes, so a recipe can't drive the terminal.
fn escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        let hidden = matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}');
        if (c.is_control() && c != '\n' && c != '\t') || hidden {
            out.extend(c.escape_unicode());
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(["-c", "user.name=test", "-c", "user.email=test@example.com", "-C"])
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn a_review_summarises_binary_files_and_escapes_control_characters() {
        let dir = tempfile::TempDir::new().unwrap();
        let work = dir.path().join("work");
        fs::create_dir_all(&work).unwrap();
        run(&work, &["init", "--quiet", "-b", "master"]);
        let commit = |files: &[(&str, &[u8])]| {
            for (name, content) in files {
                fs::write(work.join(name), content).unwrap();
            }
            run(&work, &["add", "."]);
            run(&work, &["commit", "--quiet", "-m", "recipe"]);
        };
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\x1b[2Jraw";
        commit(&[
            ("PKGBUILD", b"pkgname=demo\n# \x1b[2J\x1b[31mhidden\r\n"),
            (".SRCINFO", b"pkgbase = demo\n"),
            ("icon.png", png),
            ("latin1.txt", b"caf\xe9 \x1b]0;title\x07\n"),
        ]);
        let url = format!("file://{}", dir.path().display());
        let git = AurGit::new(&url, &dir.path().join("cache"));
        run(&work, &["clone", "--quiet", "--bare", ".", "../demo.git"]);
        git.sync("demo").unwrap();
        let first = git.head("demo").unwrap();

        let shown = git.show_all("demo", &first).unwrap();
        assert!(
            !shown.contains('\x1b') && !shown.contains('\r') && !shown.contains('\0'),
            "{shown:?}"
        );
        assert!(
            !shown.contains("IHDR") && !shown.contains("caf"),
            "binary content is not printed: {shown:?}"
        );
        let digest = crate::fetch::sha256_bytes(png);
        assert!(
            shown.contains(&format!(
                "==> icon.png: binary file, not shown ({} bytes, sha256 {digest})\n",
                png.len()
            )),
            "{shown}"
        );
        assert!(
            shown.contains("==> latin1.txt: binary file, not shown (16 bytes, sha256 "),
            "{shown}"
        );
        assert!(shown.contains("# \\u{1b}[2J\\u{1b}[31mhidden\\u{d}\n"), "{shown}");
        assert!(
            shown.starts_with("==> PKGBUILD\n") && shown.contains("==> .SRCINFO\npkgbase = demo\n"),
            "{shown}"
        );

        commit(&[
            ("PKGBUILD", b"pkgname=demo\npkgver=2 # \x1b[2J\n"),
            ("icon.png", b"\x89PNG\0\x1b[2Jfresh"),
            ("latin1.txt", b"plain now\n"),
        ]);
        run(&work, &["push", "--quiet", "../demo.git", "master"]);
        git.sync("demo").unwrap();
        let second = git.head("demo").unwrap();
        let diff = git.diff("demo", &first, &second).unwrap();
        assert!(
            !diff.contains('\x1b') && !diff.contains('\r') && !diff.contains('\0'),
            "{diff:?}"
        );
        assert!(diff.contains("+pkgver=2 # \\u{1b}[2J\n"), "{diff}");
        assert!(
            diff.contains(&format!(
                "==> icon.png: binary file, not shown (was {} bytes, sha256 {digest}, now 14 bytes, sha256 ",
                png.len()
            )),
            "{diff}"
        );
        assert!(
            diff.contains("==> latin1.txt: was a binary file (16 bytes, sha256 ")
                && diff.contains("), now\nplain now\n"),
            "{diff}"
        );
        assert!(
            !diff.contains("caf") && !diff.contains("raw") && !diff.contains("fresh"),
            "{diff}"
        );
    }
}

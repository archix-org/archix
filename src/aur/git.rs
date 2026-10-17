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
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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
        fs::rename(&tmp, &repo).or_else(|e| if repo.is_dir() { Ok(()) } else { Err(e) })?;
        Ok(())
    }

    pub fn head(&self, base: &str) -> Result<String> {
        Ok(self.git(base, &["rev-parse", "HEAD"])?.trim().to_string())
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
        self.git(base, &["diff", from, to])
    }

    /// Every file of a commit, PKGBUILD first and .SRCINFO last, so install scripts and patches are reviewed too.
    pub fn show_all(&self, base: &str, commit: &str) -> Result<String> {
        let listing = self.git(base, &["ls-tree", "-r", "--name-only", commit])?;
        let mut files: Vec<&str> = listing.lines().collect();
        files.sort_by_key(|file| (*file != "PKGBUILD", *file == ".SRCINFO", file.to_string()));
        let mut shown = String::new();
        for file in files {
            shown.push_str(&format!("==> {file}\n{}\n", self.show(base, commit, file)?));
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

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::aur::build::{BUILD_USER, ensure_user};
use crate::aur::srcinfo::{self, SrcInfo, SrcPkg, dep_name};
use crate::fetch::sha256_bytes;
pub use crate::lock::LOCAL_REPO;

/// Arch's own build recipes, one repository per package base.
pub const ARCH_RECIPES: &str = "https://gitlab.archlinux.org/archlinux/packaging/packages";
const LOCAL: &str = "local:";
const PATCH: &str = "patch:";

/// A build recipe directory and what it builds.
#[derive(Debug, Clone)]
pub struct Recipe {
    /// `local:<dir>` or `patch:<pkgbase>`, the lock's `base`.
    pub id: String,
    pub dir: PathBuf,
    /// The lock's `commit`: the directory's hash, or Arch's commit plus the patches' hash.
    pub commit: String,
    pub info: SrcInfo,
}

/// The recipes a config brings: every directory in `pkgbuilds/`, and the patches it applies to Arch packages.
pub struct Recipes {
    local: BTreeMap<String, Recipe>,
    /// Package name to its patch files, relative to the config directory.
    patches: BTreeMap<String, Vec<String>>,
    config_dir: PathBuf,
    cache: PathBuf,
    arch_url: String,
}

impl Recipes {
    pub fn load(config_dir: &Path, patches: &BTreeMap<String, Vec<String>>, cache: &Path) -> Result<Recipes> {
        let mut recipes = Recipes {
            local: BTreeMap::new(),
            patches: patches.clone(),
            config_dir: config_dir.to_path_buf(),
            cache: cache.join("recipes"),
            arch_url: ARCH_RECIPES.to_string(),
        };
        let root = config_dir.join("pkgbuilds");
        let Ok(entries) = fs::read_dir(&root) else {
            return Ok(recipes);
        };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.join("PKGBUILD").is_file())
            .collect();
        dirs.sort();
        for dir in dirs {
            let name = dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let commit = hash_dir(&dir)?;
            let info = recipes
                .srcinfo(&dir, &commit)
                .with_context(|| format!("reading pkgbuilds/{name}/PKGBUILD"))?;
            let id = format!("{LOCAL}{name}");
            recipes.local.insert(id.clone(), Recipe { id, dir, commit, info });
        }
        Ok(recipes)
    }

    /// No recipes, for a config that doesn't exist yet.
    pub fn none() -> Recipes {
        Recipes {
            local: BTreeMap::new(),
            patches: BTreeMap::new(),
            config_dir: PathBuf::new(),
            cache: PathBuf::new(),
            arch_url: ARCH_RECIPES.to_string(),
        }
    }

    /// Points patched recipes at another Arch recipe server, for tests.
    pub fn with_arch_url(mut self, url: &str) -> Recipes {
        self.arch_url = url.trim_end_matches('/').to_string();
        self
    }

    /// What a lock depends on from the config: the hash of each recipe it uses, the names every other recipe builds, and each patched package's patch hash.
    pub fn fingerprints(&self, used: &BTreeSet<String>) -> Result<BTreeMap<String, String>> {
        let mut prints: BTreeMap<String, String> = self
            .local
            .iter()
            .map(|(id, recipe)| {
                let print = if used.contains(id) {
                    recipe.commit.clone()
                } else {
                    let names: Vec<String> = recipe
                        .info
                        .packages
                        .iter()
                        .flat_map(|pkg| std::iter::once(pkg.name.clone()).chain(pkg.provides.iter().cloned()))
                        .collect();
                    format!("builds:{}", sha256_bytes(names.join(" ").as_bytes()))
                };
                (id.clone(), print)
            })
            .collect();
        for name in self.patches.keys() {
            prints.insert(format!("{PATCH}{name}"), self.patch_hash(name)?);
        }
        Ok(prints)
    }

    pub fn is_local(&self, id: &str) -> bool {
        self.local.contains_key(id)
    }

    pub fn patched_packages(&self) -> impl Iterator<Item = &String> {
        self.patches.keys()
    }

    /// The local recipe building a package that satisfies `dep`, and that package.
    /// The local recipe building a package that satisfies `dep`, version constraint included, the way AUR packages are matched.
    pub fn satisfier(&self, dep: &str) -> Option<(&Recipe, &SrcPkg)> {
        use crate::aur::index::{constraint, meets};
        let wanted = dep_name(dep);
        let version = constraint(dep);
        let by_name = self.local.values().find_map(|r| {
            r.info
                .packages
                .iter()
                .find(|p| p.name == wanted && meets(Some(&r.info.version), version))
                .map(|p| (r, p))
        });
        by_name.or_else(|| {
            self.local.values().find_map(|r| {
                r.info
                    .packages
                    .iter()
                    .find(|p| {
                        p.provides
                            .iter()
                            .filter(|provided| dep_name(provided) == wanted)
                            .any(|provided| meets(provided.split_once('=').map(|(_, v)| v), version))
                    })
                    .map(|p| (r, p))
            })
        })
    }

    /// The recipe a lock entry was built from, checked against the lock's commit.
    pub fn locked(&self, id: &str, commit: &str, version: &str, names: &[String]) -> Result<Recipe> {
        if let Some(recipe) = self.local.get(id) {
            if recipe.commit != commit {
                bail!(
                    "pkgbuilds/{} changed since archix.lock was written; run `archix lock`",
                    &id[LOCAL.len()..]
                );
            }
            return Ok(recipe.clone());
        }
        let Some(base) = id.strip_prefix(PATCH) else {
            bail!(
                "{id} is locked, but pkgbuilds/{} is gone; run `archix lock`",
                id.trim_start_matches(LOCAL)
            );
        };
        let Some(name) = names.iter().find(|name| self.patches.contains_key(*name)) else {
            bail!("{base} is locked as patched, but the config patches none of its packages; run `archix lock`");
        };
        let upstream_version = unpatched_version(version);
        let recipe = self.patched(name, base, &upstream_version)?;
        if recipe.commit != commit {
            bail!(
                "the patches for {name} or Arch's recipe for {base} changed since archix.lock was written; run `archix lock`"
            );
        }
        Ok(recipe)
    }

    /// Arch's recipe for `base` at `version`, with the config's patches for `name` applied before its own prepare().
    pub fn patched(&self, name: &str, base: &str, version: &str) -> Result<Recipe> {
        let files = self.patch_files(name)?;
        let (upstream, upstream_commit) = self.upstream(base, version)?;
        let patch_hash = self.patch_hash(name)?;
        let commit = format!("{upstream_commit}+{}", &patch_hash[..16]);
        let dir = self
            .cache
            .join("patched")
            .join(format!("{base}-{}", sha256_bytes(commit.as_bytes())));
        if !dir.join("PKGBUILD").is_file() {
            let tmp = dir.with_extension(format!("tmp{}", std::process::id()));
            let _ = fs::remove_dir_all(&tmp);
            copy_dir(&upstream, &tmp)?;
            let mut names = Vec::new();
            for (i, (file, bytes)) in files.iter().enumerate() {
                let local = format!("archix-{i}-{}", file.file_name().unwrap_or_default().to_string_lossy());
                fs::write(tmp.join(&local), bytes)?;
                names.push(local);
            }
            let pkgbuild = tmp.join("PKGBUILD");
            let mut text = fs::read_to_string(&pkgbuild).with_context(|| format!("reading {}", pkgbuild.display()))?;
            text.push_str(&patch_tail(&bump_pkgrel(version), &names));
            fs::write(&pkgbuild, text)?;
            fs::create_dir_all(dir.parent().expect("has a parent"))?;
            let _ = fs::remove_dir_all(&dir);
            fs::rename(&tmp, &dir)?;
        }
        let info = self.srcinfo(&dir, &commit)?;
        Ok(Recipe {
            id: format!("{PATCH}{base}"),
            dir,
            commit,
            info,
        })
    }

    fn patch_files(&self, name: &str) -> Result<Vec<(PathBuf, Vec<u8>)>> {
        let Some(list) = self.patches.get(name) else {
            bail!("the config has no patches for {name}");
        };
        let root = self.config_dir.canonicalize()?;
        let mut files = Vec::new();
        for relative in list {
            let full = root
                .join(relative)
                .canonicalize()
                .with_context(|| format!("patches.{name}: {relative}"))?;
            if !full.starts_with(&root) {
                bail!("patches.{name}: {relative} leaves the config directory");
            }
            let bytes = fs::read(&full).with_context(|| format!("reading {}", full.display()))?;
            files.push((full, bytes));
        }
        Ok(files)
    }

    fn patch_hash(&self, name: &str) -> Result<String> {
        let mut all = Vec::new();
        for (file, bytes) in self.patch_files(name)? {
            all.extend(file.file_name().unwrap_or_default().as_encoded_bytes());
            all.push(0);
            all.extend(sha256_bytes(&bytes).as_bytes());
        }
        Ok(sha256_bytes(&all))
    }

    /// A checkout of Arch's recipe for `base` at the tag for `version`, and its commit.
    fn upstream(&self, base: &str, version: &str) -> Result<(PathBuf, String)> {
        let tag = version.replace(':', "-").replace('~', ".");
        let dir = self.cache.join("arch").join(base).join(&tag);
        let marker = dir.join(".archix-commit");
        if let Ok(commit) = fs::read_to_string(&marker) {
            return Ok((dir, commit.trim().to_string()));
        }
        let url = format!("{}/{}.git", self.arch_url, gitlab_path(base));
        let tmp = dir.with_extension(format!("tmp{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.parent().expect("has a parent"))?;
        let status = Command::new("git")
            .args(["clone", "--quiet", "--depth", "1", "--branch", &tag, &url])
            .arg(&tmp)
            .stdout(Stdio::null())
            .status()
            .context("running git clone")?;
        if !status.success() {
            let _ = fs::remove_dir_all(&tmp);
            bail!("could not fetch Arch's recipe for {base} {version} ({url}, tag {tag})");
        }
        let output = Command::new("git")
            .args(["-c", "safe.directory=*", "-C"])
            .arg(&tmp)
            .args(["rev-parse", "HEAD"])
            .output()
            .context("running git rev-parse")?;
        let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
        fs::remove_dir_all(tmp.join(".git"))?;
        fs::write(tmp.join(".archix-commit"), &commit)?;
        let _ = fs::remove_dir_all(&dir);
        fs::rename(&tmp, &dir)?;
        Ok((dir, commit))
    }

    /// The recipe's .SRCINFO from makepkg itself, cached by the recipe's hash.
    fn srcinfo(&self, dir: &Path, commit: &str) -> Result<SrcInfo> {
        let cached = self.cache.join("srcinfo").join(sha256_bytes(commit.as_bytes()));
        if let Ok(text) = fs::read_to_string(&cached) {
            return srcinfo::parse(&text);
        }
        let text = print_srcinfo(dir, &self.cache)?;
        let info = srcinfo::parse(&text)?;
        fs::create_dir_all(cached.parent().expect("has a parent"))?;
        fs::write(&cached, &text)?;
        Ok(info)
    }
}

/// Runs `makepkg --printsrcinfo` on a copy of the recipe, as the build user when archix runs as root, since makepkg refuses root.
fn print_srcinfo(dir: &Path, cache: &Path) -> Result<String> {
    let copy = cache.join(format!("printsrcinfo-{}", std::process::id()));
    let _ = fs::remove_dir_all(&copy);
    copy_dir(dir, &copy)?;
    let root = unsafe { libc::geteuid() } == 0;
    let mut cmd = if root {
        ensure_user()?;
        let chown = Command::new("chown")
            .arg("-R")
            .arg(format!("{BUILD_USER}:"))
            .arg(&copy)
            .status()
            .context("running chown")?;
        if !chown.success() {
            bail!("could not hand {} to {BUILD_USER}", copy.display());
        }
        let mut cmd = Command::new("runuser");
        cmd.args(["-u", BUILD_USER, "--", "makepkg", "--printsrcinfo"]);
        cmd
    } else {
        let mut cmd = Command::new("makepkg");
        cmd.arg("--printsrcinfo");
        cmd
    };
    let output = cmd.current_dir(&copy).output();
    let _ = fs::remove_dir_all(&copy);
    let output = match output {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("makepkg not found; local recipes need pacman's makepkg")
        }
        Err(e) => return Err(e).context("running makepkg --printsrcinfo"),
    };
    if !output.status.success() {
        bail!(
            "makepkg --printsrcinfo failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The recipe's files, skipping makepkg's output, hashed by path, executable bit and content.
pub fn hash_dir(dir: &Path) -> Result<String> {
    let mut entries = Vec::new();
    collect(dir, dir, &mut entries)?;
    entries.sort();
    let mut all = Vec::new();
    for (relative, executable, content) in entries {
        all.extend(relative.as_bytes());
        all.push(if executable { b'x' } else { b'-' });
        all.extend(sha256_bytes(&content).as_bytes());
    }
    Ok(sha256_bytes(&all))
}

/// makepkg's output only sits at the top of a recipe; deeper down, a `src/` is part of the recipe.
fn skipped(name: &str, top: bool) -> bool {
    name == ".git"
        || (top && (matches!(name, "src" | "pkg" | ".SRCINFO") || name.contains(".pkg.tar") || name.ends_with(".log")))
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, bool, Vec<u8>)>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if skipped(&name, dir == root) {
            continue;
        }
        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().into_owned();
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            out.push((
                relative,
                false,
                fs::read_link(&path)?.as_os_str().as_encoded_bytes().to_vec(),
            ));
        } else if meta.is_dir() {
            collect(root, &path, out)?;
        } else {
            out.push((relative, meta.permissions().mode() & 0o111 != 0, fs::read(&path)?));
        }
    }
    Ok(())
}

pub(crate) fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    let status = Command::new("cp")
        .arg("-a")
        .arg(format!("{}/.", from.display()))
        .arg(to)
        .status()
        .context("running cp")?;
    if !status.success() {
        bail!("could not copy {} to {}", from.display(), to.display());
    }
    Ok(())
}

/// Arch's GitLab project path for a package base, the way devtools' pkgctl derives it.
pub fn gitlab_path(base: &str) -> String {
    let chars: Vec<char> = base.chars().collect();
    let mut out = String::new();
    for (i, c) in chars.iter().enumerate() {
        match c {
            '+' if i > 0
                && chars[i - 1].is_ascii_alphanumeric()
                && chars.get(i + 1).is_some_and(|n| n.is_ascii_alphabetic()) =>
            {
                out.push('-')
            }
            '+' => out.push_str("plus"),
            c if c.is_ascii_alphanumeric() || "_-.".contains(*c) => out.push(*c),
            _ => out.push('-'),
        }
    }
    let mut collapsed = String::new();
    for c in out.chars() {
        let separator = |c: char| c == '_' || c == '-';
        if separator(c) && collapsed.ends_with(separator) {
            collapsed.pop();
            collapsed.push('-');
        } else {
            collapsed.push(c);
        }
    }
    if collapsed == "tree" {
        "unix-tree".to_string()
    } else {
        collapsed
    }
}

/// A patched build's version: a sub-release on top of Arch's pkgrel, so it sorts after Arch's own build of that version.
pub fn bump_pkgrel(version: &str) -> String {
    let (head, pkgrel) = version.rsplit_once('-').unwrap_or((version, "1"));
    let pkgrel = match pkgrel.split_once('.') {
        Some((major, minor)) => format!("{major}.{}", minor.parse::<u32>().unwrap_or(0) + 1),
        None => format!("{pkgrel}.1"),
    };
    format!("{head}-{pkgrel}")
}

/// The Arch version a patched build came from.
fn unpatched_version(version: &str) -> String {
    let (head, pkgrel) = version.rsplit_once('-').unwrap_or((version, "1"));
    match pkgrel.split_once('.') {
        Some((major, "1")) => format!("{head}-{major}"),
        Some((major, minor)) => format!("{head}-{major}.{}", minor.parse::<u32>().unwrap_or(1) - 1),
        None => version.to_string(),
    }
}

/// Lines appended to Arch's PKGBUILD: the new pkgrel, the patches as sources, and a prepare() that applies them first.
fn patch_tail(version: &str, patches: &[String]) -> String {
    let pkgrel = version.rsplit_once('-').map_or("1", |(_, rel)| rel);
    let quoted: Vec<String> = patches.iter().map(|p| format!("'{p}'")).collect();
    let skips = vec!["SKIP"; patches.len()].join(" ");
    let mut tail = String::from("\n# Added by archix for the config's patches.\n");
    let _ = writeln!(tail, "pkgrel={pkgrel}");
    let _ = writeln!(tail, "source+=({})", quoted.join(" "));
    let _ = writeln!(
        tail,
        "for _archix_sums in md5sums sha1sums sha224sums sha256sums sha384sums sha512sums b2sums cksums; do\n  \
         if declare -p \"$_archix_sums\" &>/dev/null; then eval \"$_archix_sums+=({skips})\"; fi\ndone"
    );
    tail.push_str(
        "if declare -f prepare >/dev/null; then eval \"_archix_prepare() $(declare -f prepare | tail -n +2)\"; fi\n",
    );
    let _ = writeln!(
        tail,
        "prepare() {{\n  local _patch _dir\n  for _patch in {}; do\n    \
         for _dir in \"$srcdir\"/*/; do\n      \
         if patch -d \"$_dir\" -p1 -N --dry-run --silent < \"$srcdir/$_patch\" >/dev/null 2>&1; then\n        \
         patch -d \"$_dir\" -p1 -N < \"$srcdir/$_patch\"\n        continue 2\n      fi\n    done\n    \
         echo \"archix: $_patch applies to no directory in \\$srcdir\" >&2\n    return 1\n  done\n  \
         if declare -f _archix_prepare >/dev/null; then cd \"$srcdir\" && _archix_prepare; fi\n}}",
        patches.join(" ")
    );
    tail
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gitlab_paths_match_pkgctl() {
        assert_eq!(gitlab_path("hyprland"), "hyprland");
        assert_eq!(gitlab_path("dvd+rw-tools"), "dvd-rw-tools");
        assert_eq!(gitlab_path("libsigc++"), "libsigcplusplus");
        assert_eq!(gitlab_path("gtk2+extra"), "gtk2-extra");
        assert_eq!(gitlab_path("tree"), "unix-tree");
        assert_eq!(gitlab_path("foo__bar"), "foo-bar");
    }

    #[test]
    fn patched_versions_sort_after_arch_and_map_back() {
        assert_eq!(bump_pkgrel("0.51.1-2"), "0.51.1-2.1");
        assert_eq!(bump_pkgrel("1:2.0-3.1"), "1:2.0-3.2");
        assert_eq!(unpatched_version("0.51.1-2.1"), "0.51.1-2");
        assert_eq!(unpatched_version("1:2.0-3.2"), "1:2.0-3.1");
    }

    #[test]
    fn the_hash_follows_content_and_ignores_build_output() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("PKGBUILD"), "pkgname=x\n").unwrap();
        let before = hash_dir(dir.path()).unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/built"), "x").unwrap();
        fs::write(dir.path().join("x-1-1-x86_64.pkg.tar.zst"), "x").unwrap();
        assert_eq!(hash_dir(dir.path()).unwrap(), before);
        fs::create_dir_all(dir.path().join("files/src")).unwrap();
        fs::write(dir.path().join("files/src/part.c"), "int x;").unwrap();
        assert_ne!(
            hash_dir(dir.path()).unwrap(),
            before,
            "a src/ below the top is part of the recipe"
        );
        let nested = hash_dir(dir.path()).unwrap();
        fs::write(dir.path().join("PKGBUILD"), "pkgname=y\n").unwrap();
        assert_ne!(hash_dir(dir.path()).unwrap(), nested);
    }

    #[test]
    fn the_patch_tail_applies_patches_before_the_recipes_prepare() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(src.join("proj-1.0")).unwrap();
        fs::write(src.join("proj-1.0/hello"), "old\n").unwrap();
        fs::write(
            src.join("archix-0-fix.patch"),
            "--- a/hello\n+++ b/hello\n@@ -1 +1 @@\n-old\n+new\n",
        )
        .unwrap();
        let pkgbuild = format!(
            "pkgrel=2\nsha256sums=('abc')\nprepare() {{ cat proj-1.0/hello > \"$srcdir/seen\"; }}\n{}",
            patch_tail("1.0-2.1", &["archix-0-fix.patch".to_string()])
        );
        let script = format!(
            "srcdir={}\n{pkgbuild}\nprepare\necho \"$pkgrel ${{#sha256sums[@]}}\"",
            src.display()
        );
        let output = Command::new("bash").args(["-e", "-c", &script]).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim().lines().last(),
            Some("2.1 2")
        );
        assert_eq!(
            fs::read_to_string(src.join("seen")).unwrap(),
            "new\n",
            "the recipe's prepare sees patched sources"
        );
    }
}

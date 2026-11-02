//! The config tree a root command applies: captured as the config directory's owner, never by root in the owner's repo, and kept in root's own repository with every applied generation.
use std::ffi::{CStr, OsStr};
use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// The account a config directory belongs to, with what dropping to it needs, computed before any fork.
#[derive(Debug, Clone)]
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
    pub name: String,
    pub home: PathBuf,
    groups: Vec<libc::gid_t>,
}

impl Owner {
    pub fn of(path: &Path) -> Result<Owner> {
        let meta = fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
        Owner::by_uid(meta.uid())
    }

    pub fn by_uid(uid: u32) -> Result<Owner> {
        let entry = unsafe { libc::getpwuid(uid) };
        if entry.is_null() {
            return Ok(Owner {
                uid,
                gid: uid,
                name: format!("uid {uid}"),
                home: PathBuf::from("/"),
                groups: vec![uid],
            });
        }
        let (name, gid, home) = unsafe {
            (
                CStr::from_ptr((*entry).pw_name).to_owned(),
                (*entry).pw_gid,
                PathBuf::from(CStr::from_ptr((*entry).pw_dir).to_string_lossy().into_owned()),
            )
        };
        let groups = group_list(&name, gid);
        Ok(Owner {
            uid,
            gid,
            name: name.to_string_lossy().into_owned(),
            home,
            groups,
        })
    }

    /// A command that runs `program` as this account: its own groups only (none of root's), then its gid and uid, with no_new_privs and a cleared environment.
    pub fn command<S: AsRef<OsStr>>(&self, program: S) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin")
            .env("HOME", &self.home)
            .env("LC_ALL", "C");
        drop_to(&mut command, self.uid, self.gid, self.groups.clone());
        command
    }
}

fn group_list(name: &CStr, gid: u32) -> Vec<libc::gid_t> {
    let mut count: libc::c_int = 64;
    loop {
        let mut groups = vec![0 as libc::gid_t; count as usize];
        let found = unsafe { libc::getgrouplist(name.as_ptr(), gid, groups.as_mut_ptr(), &mut count) };
        if found >= 0 {
            groups.truncate(count as usize);
            return groups;
        }
        if count as usize <= groups.len() {
            return vec![gid];
        }
    }
}

/// Makes `command` drop every privilege before it execs: setgroups, then setresgid, then setresuid, then no_new_privs; an uid other than root leaves it non-ptraceable by root's other children too.
pub fn drop_to(command: &mut Command, uid: u32, gid: u32, groups: Vec<libc::gid_t>) {
    if unsafe { libc::geteuid() } != 0 && uid == unsafe { libc::geteuid() } {
        unsafe {
            command.pre_exec(no_new_privs);
        }
        return;
    }
    unsafe {
        command.pre_exec(move || {
            if libc::setgroups(groups.len(), groups.as_ptr()) != 0
                || libc::setresgid(gid, gid, gid) != 0
                || libc::setresuid(uid, uid, uid) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            no_new_privs()
        });
    }
}

fn no_new_privs() -> std::io::Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Git's environment for runs archix starts: no system or global config, no pager, no hooks, so only the repository given decides.
fn git_env(command: &mut Command) -> &mut Command {
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .args(["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false"])
}

fn run(mut command: Command, what: &str) -> Result<Vec<u8>> {
    let output = command
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {what}"))?;
    if !output.status.success() {
        bail!("{what} failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(output.stdout)
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).trim().to_string()
}

/// A config tree captured for a root command: its git tree id and a root-owned copy nobody else can write.
#[derive(Debug, Clone)]
pub struct Captured {
    pub tree: String,
    pub dir: PathBuf,
    /// The owner's throwaway repository the tree was captured in, which the owner's own repository fetches it from.
    scratch: PathBuf,
}

/// Where root commands keep their captures of the config while they run.
pub const CAPTURES: &str = "/run/archix/captures";

/// A root-owned scratch directory for one capture, removed when dropped.
pub struct Work {
    pub dir: PathBuf,
}

impl Work {
    pub fn new(parent: &Path) -> Result<Work> {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o711))?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = parent.join(format!("capture-{}-{stamp}", std::process::id()));
        fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o711))?;
        Ok(Work { dir })
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Captures `config_dir` as its owner, in a throwaway repository the owner's own git config never reaches, and unpacks the tree into a root-owned copy whose tree id root checks itself.
pub fn capture(config_dir: &Path, work: &Work) -> Result<Captured> {
    let owner = Owner::of(config_dir)?;
    let scratch = work.dir.join("git");
    fs::create_dir(&scratch)?;
    std::os::unix::fs::chown(&scratch, Some(owner.uid), Some(owner.gid))?;
    fs::set_permissions(&scratch, fs::Permissions::from_mode(0o700))?;
    let git = |args: &[&str]| {
        let mut command = owner.command("git");
        git_env(&mut command)
            .env("GIT_DIR", &scratch)
            .env("GIT_WORK_TREE", config_dir)
            .current_dir(config_dir)
            .args(args);
        command
    };
    let mut init = owner.command("git");
    git_env(&mut init).args(["init", "-q", "--bare"]).arg(&scratch);
    run(init, "git init, as the config's owner")?;
    run(
        git(&["add", "-A"]),
        &format!("git add -A in {}, as {}", config_dir.display(), owner.name),
    )
    .map_err(|e| {
        e.context(format!(
            "{} must be readable by its owner {}, who archix reads it as",
            config_dir.display(),
            owner.name
        ))
    })?;
    let tree = text(run(git(&["write-tree"]), "git write-tree, as the config's owner")?);
    let mut archive = git(&["archive", "--format=tar", &tree]);
    let mut child = archive
        .stdout(Stdio::piped())
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("running git archive as the config's owner")?;
    let dir = work.dir.join("tree");
    fs::create_dir(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755))?;
    let mut tar = Vec::new();
    child
        .stdout
        .take()
        .context("git archive's output")?
        .read_to_end(&mut tar)?;
    let status = child.wait()?;
    if !status.success() {
        bail!("git archive of the config failed ({status})");
    }
    unpack(&tar, &dir)?;
    let checked = tree_id(&dir, work)?;
    if checked != tree {
        bail!(
            "the config tree root unpacked ({checked}) isn't the one its owner captured ({tree}); git submodules and files git can't archive aren't supported in the config"
        );
    }
    Ok(Captured { tree, dir, scratch })
}

/// Unpacks a tar of a config tree: regular files, directories and symlinks only, every path inside `dir`, owned by root.
pub fn unpack(tar: &[u8], dir: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(tar);
    archive.set_preserve_permissions(false);
    archive.set_preserve_ownerships(false);
    archive.set_unpack_xattrs(false);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        if kind.is_pax_global_extensions() || kind.is_pax_local_extensions() {
            continue;
        }
        if !(kind.is_file() || kind.is_dir() || kind.is_symlink()) {
            bail!(
                "the config tree holds {:?}, which isn't a file, directory or symlink",
                entry.path()?
            );
        }
        let path = entry.path()?.into_owned();
        if path.is_absolute() || path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
            bail!("the config tree holds a path outside it: {}", path.display());
        }
        let mode = entry.header().mode().unwrap_or(0o644);
        if !entry.unpack_in(dir)? {
            bail!("the config tree holds a path outside it: {}", path.display());
        }
        let target = dir.join(&path);
        if kind.is_file() {
            let exec = mode & 0o111 != 0;
            fs::set_permissions(&target, fs::Permissions::from_mode(if exec { 0o755 } else { 0o644 }))?;
        } else if kind.is_dir() {
            fs::set_permissions(&target, fs::Permissions::from_mode(0o755))?;
        }
    }
    Ok(())
}

/// The git tree id of `dir`, computed by root in a repository of its own with nothing from the config's owner.
fn tree_id(dir: &Path, work: &Work) -> Result<String> {
    let repo = work.dir.join("check.git");
    let git = |args: &[&str]| {
        let mut command = Command::new("git");
        command.env_clear().env("PATH", "/usr/bin").env("HOME", "/");
        git_env(&mut command)
            .env("GIT_DIR", &repo)
            .env("GIT_WORK_TREE", dir)
            .current_dir(dir)
            .args(args);
        command
    };
    let mut init = Command::new("git");
    init.env_clear().env("PATH", "/usr/bin").env("HOME", "/");
    git_env(&mut init).args(["init", "-q", "--bare"]).arg(&repo);
    run(init, "git init")?;
    run(git(&["add", "-A", "-f"]), "git add -A")?;
    Ok(text(run(git(&["write-tree"]), "git write-tree")?))
}

/// Points `refs/archix/generations/<number>` in the owner's repository at the applied tree, fetched as the owner from the capture, for browsing; root never reads it back.
pub fn record_for_owner(config_dir: &Path, captured: &Captured, number: u32) -> Result<()> {
    let owner = Owner::of(config_dir)?;
    let git = |args: &[&str]| {
        let mut command = owner.command("git");
        git_env(&mut command).current_dir(config_dir).args(args);
        command
    };
    if run(git(&["rev-parse", "--git-dir"]), "git rev-parse").is_err() {
        return Ok(());
    }
    let mut point = owner.command("git");
    git_env(&mut point)
        .env("GIT_DIR", &captured.scratch)
        .args(["update-ref", "refs/archix/applied", &captured.tree]);
    run(point, "git update-ref in the capture")?;
    let scratch = captured.scratch.to_string_lossy();
    run(
        git(&[
            "fetch",
            "-q",
            "--no-tags",
            &scratch,
            &format!("+refs/archix/applied:refs/archix/generations/{number}"),
        ]),
        "git fetch of the applied tree, as the config's owner",
    )?;
    Ok(())
}

/// Root's own repository of applied generations: each a commit holding the config tree, the lock and the plan.
pub struct Applied {
    pub git_dir: PathBuf,
}

/// The applied tree, lock and plan of one generation, as root stored them.
#[derive(Debug, Clone, PartialEq)]
pub struct Generation {
    pub commit: String,
    pub tree: String,
}

impl Applied {
    /// On the rollback layout it's @archix's, so it survives a keep; without one, under archix's state.
    pub fn at(root: &Path, state: &Path) -> Applied {
        let on_archix = root.join(crate::snapshot::MOUNT);
        let base = if on_archix.is_dir() {
            on_archix
        } else {
            state.to_path_buf()
        };
        Applied {
            git_dir: base.join("config.git"),
        }
    }

    fn git(&self, args: &[&str]) -> Command {
        let mut command = Command::new("git");
        command.env_clear().env("PATH", "/usr/bin").env("HOME", "/");
        git_env(&mut command).env("GIT_DIR", &self.git_dir).args(args);
        command
    }

    fn ensure(&self) -> Result<()> {
        if self.git_dir.join("HEAD").is_file() {
            return Ok(());
        }
        if let Some(parent) = self.git_dir.parent() {
            fs::create_dir_all(parent)?;
        }
        run(
            self.git(&["init", "-q", "--bare"]),
            "git init of root's config repository",
        )?;
        fs::set_permissions(&self.git_dir, fs::Permissions::from_mode(0o700))?;
        Ok(())
    }

    /// Stores `captured`'s tree into this repository, from root's own copy.
    pub fn store_tree(&self, captured: &Captured) -> Result<String> {
        self.ensure()?;
        let index = captured.dir.with_file_name("applied-index");
        let mut add = self.git(&["add", "-A", "-f"]);
        add.env("GIT_INDEX_FILE", &index)
            .env("GIT_WORK_TREE", &captured.dir)
            .current_dir(&captured.dir);
        run(add, "git add into root's config repository")?;
        let mut write = self.git(&["write-tree"]);
        write
            .env("GIT_INDEX_FILE", &index)
            .env("GIT_WORK_TREE", &captured.dir)
            .current_dir(&captured.dir);
        let tree = text(run(write, "git write-tree")?);
        if tree != captured.tree {
            bail!(
                "root's config repository stored tree {tree}, not the captured {}",
                captured.tree
            );
        }
        Ok(tree)
    }

    fn blob(&self, bytes: &[u8]) -> Result<String> {
        let mut child = self
            .git(&["hash-object", "-w", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        std::io::Write::write_all(&mut child.stdin.take().context("git's stdin")?, bytes)?;
        let output = child.wait_with_output()?;
        if !output.status.success() {
            bail!("git hash-object failed");
        }
        Ok(text(output.stdout))
    }

    /// Records generation `number`: a commit of the config tree beside its lock and plan, at refs/generations/<number>.
    pub fn record(&self, number: u32, tree: &str, lock: &[u8], plan: &[u8]) -> Result<Generation> {
        self.ensure()?;
        let lock = self.blob(lock)?;
        let plan = self.blob(plan)?;
        let listing =
            format!("040000 tree {tree}\tconfig\n100644 blob {lock}\tarchix.lock\n100644 blob {plan}\tplan.json\n");
        let mut child = self
            .git(&["mktree"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        std::io::Write::write_all(&mut child.stdin.take().context("git's stdin")?, listing.as_bytes())?;
        let output = child.wait_with_output()?;
        if !output.status.success() {
            bail!("git mktree failed");
        }
        let top = text(output.stdout);
        let mut commit = self.git(&["commit-tree", &top, "-m", &format!("generation {number}")]);
        commit
            .env("GIT_AUTHOR_NAME", "archix")
            .env("GIT_AUTHOR_EMAIL", "archix@localhost")
            .env("GIT_COMMITTER_NAME", "archix")
            .env("GIT_COMMITTER_EMAIL", "archix@localhost");
        let commit = text(run(commit, "git commit-tree")?);
        run(
            self.git(&["update-ref", &format!("refs/generations/{number}"), &commit]),
            "git update-ref",
        )?;
        run(self.git(&["update-ref", "refs/applied", &commit]), "git update-ref")?;
        Ok(Generation {
            commit,
            tree: tree.to_string(),
        })
    }

    /// The config tree last applied, if any.
    pub fn applied_tree(&self) -> Option<String> {
        let out = run(
            self.git(&["rev-parse", "--verify", "-q", "refs/applied^{commit}:config"]),
            "git rev-parse",
        )
        .ok()?;
        Some(text(out)).filter(|t| !t.is_empty())
    }

    /// Generation `number`'s config tree, lock and plan, read back from root's own objects, the commit checked against `expected` when given.
    pub fn read(&self, number: u32, expected: Option<&str>) -> Result<(String, Vec<u8>, Vec<u8>)> {
        let reference = format!("refs/generations/{number}");
        let commit = text(run(
            self.git(&["rev-parse", "--verify", &format!("{reference}^{{commit}}")]),
            "git rev-parse",
        )?);
        if let Some(expected) = expected
            && expected != commit
        {
            bail!(
                "generation {number} in root's config repository is {commit}, not the {expected} its snapshot recorded"
            );
        }
        let tree = text(run(
            self.git(&["rev-parse", &format!("{commit}:config")]),
            "git rev-parse",
        )?);
        let lock = run(
            self.git(&["cat-file", "blob", &format!("{commit}:archix.lock")]),
            "git cat-file",
        )?;
        let plan = run(
            self.git(&["cat-file", "blob", &format!("{commit}:plan.json")]),
            "git cat-file",
        )?;
        Ok((tree, lock, plan))
    }

    /// Unpacks a stored config tree into `dir`, root-owned.
    pub fn materialize(&self, tree: &str, dir: &Path) -> Result<()> {
        let tar = run(self.git(&["archive", "--format=tar", tree]), "git archive")?;
        fs::create_dir_all(dir)?;
        unpack(&tar, dir)
    }

    /// What changes from the last applied config tree to `tree`: a stat and the patch, at most `lines` lines of it.
    pub fn diff(&self, tree: &str, lines: usize) -> Result<Option<String>> {
        let Some(applied) = self.applied_tree() else {
            return Ok(None);
        };
        if applied == tree {
            return Ok(Some(String::new()));
        }
        let out = text(run(
            self.git(&[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--stat",
                "--patch",
                &applied,
                tree,
            ]),
            "git diff",
        )?);
        let out = redact_hashes(&out);
        let total = out.lines().count();
        let mut shown: Vec<&str> = out.lines().take(lines).collect();
        let more = format!(
            "... {} more lines; `git diff` in the config shows them",
            total - shown.len()
        );
        if total > lines {
            shown.push(&more);
        }
        Ok(Some(shown.join("\n")))
    }
}

/// `text` with every crypt(3) hash ($y$..., $6$..., $2b$...) replaced, so a diff of the config never prints a password hash.
pub fn redact_hashes(text: &str) -> String {
    const IDS: &[&str] = &[
        "$y$", "$gy$", "$7$", "$6$", "$5$", "$2a$", "$2b$", "$2y$", "$1$", "$sha1$", "$md5$",
    ];
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = IDS.iter().filter_map(|id| rest.find(id)).min() {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        let end = after
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | ']' | '}' | ')'))
            .unwrap_or(after.len());
        out.push_str("<hash>");
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// Writes `bytes` to `path` in a directory its owner controls, as that owner: no symlink followed, a temp file renamed over it, so root never opens a path in the owner's tree.
pub fn write_as_owner(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("a file to write needs a directory")?;
    let owner = Owner::of(dir)?;
    let name = path.file_name().context("a file to write needs a name")?;
    let mut command = owner.command(crate::testhooks::own_exe().context("finding archix's own binary")?);
    command
        .arg("write-file")
        .arg(dir)
        .arg(name)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = command.spawn().context("starting archix as the directory's owner")?;
    std::io::Write::write_all(&mut child.stdin.take().context("the writer's stdin")?, bytes)?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "writing {} as {} failed: {}",
            path.display(),
            owner.name,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// `archix write-file DIR NAME`, already running as DIR's owner: stdin into a temp file in DIR opened with O_NOFOLLOW and O_EXCL, then renamed over NAME; refuses a NAME that's a symlink or a DIR that is one.
pub fn write_file_here(dir: &Path, name: &OsStr, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if fs::symlink_metadata(dir)?.file_type().is_symlink() {
        bail!("{} is a symlink; nothing was written", dir.display());
    }
    let target = dir.join(name);
    if fs::symlink_metadata(&target).is_ok_and(|meta| !meta.file_type().is_file()) {
        bail!("{} isn't a regular file; nothing was written", target.display());
    }
    let temp = dir.join(format!(".{}.archix-{}", name.to_string_lossy(), std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o644)
        .open(&temp)
        .with_context(|| format!("creating {}", temp.display()))?;
    std::io::Write::write_all(&mut file, bytes)?;
    file.sync_all()?;
    fs::rename(&temp, &target).with_context(|| format!("renaming over {}", target.display()))?;
    Ok(())
}

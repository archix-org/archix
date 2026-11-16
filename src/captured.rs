//! The config tree a root command evaluates: captured as the config directory's owner, never by root in the owner's repo, into a root-owned copy.
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

/// A config tree captured for a root command: its git tree id and a root-owned copy nobody else can read, but for the evaluator while it runs.
#[derive(Debug, Clone)]
pub struct Captured {
    pub tree: String,
    pub dir: PathBuf,
    /// Files in the config directory a .gitignore keeps out of the tree, relative to it.
    pub ignored: Vec<String>,
}

impl Captured {
    /// `error`, an evaluation's, with what it names that's gitignored said so, as such a file isn't in the tree evaluated.
    pub fn explain(&self, error: String) -> String {
        let named: Vec<&str> = self
            .ignored
            .iter()
            .map(String::as_str)
            .filter(|path| {
                let module = path
                    .trim_start_matches("lua/")
                    .trim_end_matches(".lua")
                    .replace('/', ".");
                error.contains(*path)
                    || error.contains(&format!("'{module}'"))
                    || error.contains(&format!("\"{module}\""))
            })
            .collect();
        if named.is_empty() {
            return error;
        }
        format!(
            "{error}\n{} gitignored, so not in the capture of the config, which is what archix evaluates: a generation must be reproducible from its tree. Stop ignoring {}, or keep a secret in it as an encrypted file (secrets.items)",
            match named.as_slice() {
                [one] => format!("{one} is"),
                many => format!("{} are", many.join(", ")),
            },
            if named.len() == 1 { "it" } else { "them" }
        )
    }
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
    let config_dir = &std::path::absolute(config_dir).with_context(|| format!("resolving {}", config_dir.display()))?;
    let owner = Owner::of(config_dir)?;
    let private = work.dir.join("root");
    fs::create_dir(&private)?;
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700))?;
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
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
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
    let checked = tree_id(&dir, &private)?;
    if checked != tree {
        bail!(
            "the config tree root unpacked ({checked}) isn't the one its owner captured ({tree}); git submodules and files git can't archive aren't supported in the config"
        );
    }
    let ignored = run(
        git(&["ls-files", "-z", "--others", "--ignored", "--exclude-standard"]),
        "git ls-files, as the config's owner",
    )
    .map(|out| {
        String::from_utf8_lossy(&out)
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_string)
            .collect()
    })
    .unwrap_or_default();
    Ok(Captured { tree, dir, ignored })
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
fn tree_id(dir: &Path, private: &Path) -> Result<String> {
    let repo = private.join("check.git");
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

/// `text` with every crypt(3) hash ($y$..., $6$..., $2b$...) and the value of every `password` key replaced, so a diff of the config never prints a password hash, whichever scheme made it.
pub fn redact_hashes(text: &str) -> String {
    let lines: Vec<String> = redact_ids(text).split('\n').map(redact_password_key).collect();
    lines.join("\n")
}

fn redact_password_key(line: &str) -> String {
    let Some(key) = line.to_lowercase().find("password") else {
        return line.to_string();
    };
    let Some(equals) = line[key..].find('=').map(|at| at + key) else {
        return line.to_string();
    };
    let value = &line[equals..];
    let Some(open) = value.find(['"', '\'']) else {
        return line.to_string();
    };
    let quote = value[open..].chars().next().unwrap_or('"');
    let Some(close) = value[open + 1..].find(quote) else {
        return line.to_string();
    };
    format!(
        "{}{quote}<hash>{quote}{}",
        &line[..equals + open],
        redact_password_key(&value[open + close + 2..])
    )
}

fn redact_ids(text: &str) -> String {
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
    let existing = dir
        .ancestors()
        .find(|ancestor| fs::metadata(ancestor).is_ok())
        .context("a file to write needs a directory")?;
    let owner = Owner::of(existing)?;
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

/// A config file's text, refusing to follow a symlink there; none when it's missing.
pub fn read_no_follow(path: &Path) -> Result<Option<String>> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            bail!(
                "{} is a symlink, which archix doesn't follow; make it a file",
                path.display()
            )
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut text = String::new();
    std::io::Read::read_to_string(&mut file, &mut text).with_context(|| format!("reading {}", path.display()))?;
    Ok(Some(text))
}

/// `archix write-file DIR NAME`, already running as DIR's owner: makes DIR if it's missing, then stdin into a temp file in DIR opened with O_NOFOLLOW and O_EXCL, renamed over NAME; refuses a NAME that's a symlink or a DIR that is one.
pub fn write_file_here(dir: &Path, name: &OsStr, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
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
    crate::durable::rename(&temp, &target).with_context(|| format!("renaming over {}", target.display()))?;
    Ok(())
}

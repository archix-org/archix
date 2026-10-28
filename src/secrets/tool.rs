//! `archix secrets`: the machine's key, editing and setting secrets, re-encrypting them to the recipients every host's config names, and checking them.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fs;
use std::io::{IsTerminal, Read};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::memory::{MAX_SECRET, SecretBuf};
use super::{Declared, crypto};
use crate::value::Value;

/// The file archix keeps beside the secrets: who each was last encrypted to, and that ciphertext's sha256.
pub const RECIPIENTS: &str = "secrets/recipients.toml";

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Recorded {
    #[serde(flatten)]
    pub files: BTreeMap<String, RecordedFile>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RecordedFile {
    pub recipients: Vec<String>,
    pub sha256: String,
}

pub fn read_recorded(config_dir: &Path) -> Recorded {
    fs::read_to_string(config_dir.join(RECIPIENTS))
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

fn write_recorded(config_dir: &Path, recorded: &Recorded) -> Result<()> {
    let text = format!(
        "# Written by archix secrets: who each age file was last encrypted to; edit the config's secrets.machine_key and secrets.admins instead.\n{}",
        toml::to_string(recorded)?
    );
    crate::durable::write_config(&config_dir.join(RECIPIENTS), text.as_bytes())
}

/// Every host the config describes, from lua/hosts/<host>.lua and lua/hosts/<host>/, with `host` among them, each evaluated.
pub fn hosts(config_dir: &Path, host: &str) -> Result<Vec<(String, Value)>> {
    let mut names: BTreeSet<String> = BTreeSet::new();
    names.insert(host.to_string());
    for entry in fs::read_dir(config_dir.join("lua/hosts"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        match name.strip_suffix(".lua") {
            Some(stem) => names.insert(stem.to_string()),
            None if entry.path().is_dir() => names.insert(name),
            None => false,
        };
    }
    let mut out = Vec::new();
    for name in names {
        let evaluation = crate::eval::evaluate(&crate::eval::Input {
            config_dir: config_dir.to_path_buf(),
            hostname: name.clone(),
            ..Default::default()
        })
        .map_err(|errors| anyhow::anyhow!("evaluating host {name}, whose secrets' recipients count too: {errors}"))?;
        out.push((name, evaluation.config));
    }
    Ok(out)
}

/// What each age file must be encrypted to: the machine key of every host that declares it, and those hosts' admins.
pub fn wanted(hosts: &[(String, Value)]) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (host, config) in hosts {
        let declared = super::declared(config)?;
        for item in declared.items.values() {
            let set = out.entry(item.file.clone()).or_default();
            match &declared.machine_key {
                Some(key) => set.insert(key.trim().to_string()),
                None => bail!(
                    "host {host} declares secrets but no secrets.machine_key; run `sudo archix secrets init` there and put the key it prints in its config"
                ),
            };
            set.extend(declared.admins.iter().map(|admin| admin.trim().to_string()));
        }
    }
    Ok(out)
}

fn declared_here(config: &Value, name: &str) -> Result<(Declared, super::Item)> {
    let declared = super::declared(config)?;
    let item = declared.items.get(name).cloned().with_context(|| {
        format!("secrets.items doesn't declare {name:?}; add secrets.items.{name} = {{ file = \"secrets/{name}.age\" }} to the config first")
    })?;
    Ok((declared, item))
}

/// Encrypts `value` for the age file `file` to what every host wants and records it.
fn store(config_dir: &Path, hosts: &[(String, Value)], file: &str, value: &[u8]) -> Result<Vec<String>> {
    let wanted = wanted(hosts)?;
    let recipients: Vec<String> = wanted.get(file).cloned().unwrap_or_default().into_iter().collect();
    if recipients.is_empty() {
        bail!("no host declares {file}, so there's nobody to encrypt it to");
    }
    let ciphertext = crypto::encrypt(&recipients, value)?;
    let path = config_dir.join(file);
    crypto::write_ciphertext(&path, &ciphertext)?;
    let mut recorded = read_recorded(config_dir);
    recorded.files.insert(
        file.to_string(),
        RecordedFile {
            recipients: recipients.clone(),
            sha256: crate::fetch::sha256_bytes(&ciphertext),
        },
    );
    write_recorded(config_dir, &recorded)?;
    Ok(recipients)
}

/// The identities that decrypt for the caller: the machine's (and the retired ones) with `machine`, else the user's own.
fn identities(state: &Path, machine: bool) -> Result<Vec<crypto::Identity>> {
    if machine {
        return crypto::machine_identities(state);
    }
    let found = crypto::user_identities();
    if found.is_empty() {
        bail!(
            "no identity to decrypt with: set ARCHIX_AGE_IDENTITY to an age identity file, put one at ~/.config/archix/identity, or have ~/.ssh/id_ed25519; as root on a host, --machine uses its own key"
        );
    }
    Ok(found)
}

fn decrypt_file(path: &Path, identities: &[crypto::Identity], name: &str) -> Result<SecretBuf> {
    let ciphertext = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(crypto::decrypt(&ciphertext, identities, &format!("secret {name}"))?.0)
}

/// Whether swap could hold what an editor or this process has in memory.
fn swap_warning() -> Option<String> {
    let unencrypted = super::storage::unencrypted_swaps(Path::new("/"));
    (!unencrypted.is_empty()).then(|| {
        format!(
            "warning: swap isn't encrypted ({}), and the editor's memory can be swapped out to it with the plain text",
            unencrypted.join(", ")
        )
    })
}

const TMPFS_MAGIC: i64 = 0x0102_1994;
const RAMFS_MAGIC: i64 = 0x8584_58f6;

fn fs_type(path: &Path) -> Option<i64> {
    let c = CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statfs(c.as_ptr(), &mut stat) } == 0).then_some(stat.f_type as i64)
}

/// A private ramfs of this process's own mount namespace, which never swaps and is gone when the process and the editor are: as root, or through a user namespace.
fn private_ramfs(dir: &Path) -> Result<()> {
    let root = unsafe { libc::geteuid() } == 0;
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    let flags = if root {
        libc::CLONE_NEWNS
    } else {
        libc::CLONE_NEWUSER | libc::CLONE_NEWNS
    };
    if unsafe { libc::unshare(flags) } != 0 {
        bail!("unshare: {}", std::io::Error::last_os_error());
    }
    if !root {
        fs::write("/proc/self/setgroups", "deny")?;
        fs::write("/proc/self/uid_map", format!("{uid} {uid} 1"))?;
        fs::write("/proc/self/gid_map", format!("{gid} {gid} 1"))?;
    }
    let none = CString::new("none")?;
    let slash = CString::new("/")?;
    if unsafe {
        libc::mount(
            none.as_ptr(),
            slash.as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        )
    } != 0
    {
        bail!(
            "making the mount namespace private: {}",
            std::io::Error::last_os_error()
        );
    }
    let target = CString::new(dir.as_os_str().as_encoded_bytes())?;
    let (ramfs, mode) = (CString::new("ramfs")?, CString::new("mode=0700")?);
    if unsafe {
        libc::mount(
            ramfs.as_ptr(),
            target.as_ptr(),
            ramfs.as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV,
            mode.as_ptr().cast(),
        )
    } != 0
    {
        bail!("mounting a ramfs: {}", std::io::Error::last_os_error());
    }
    if !root {
        std::os::unix::fs::chown(dir, Some(uid), Some(gid))?;
    }
    Ok(())
}

/// The plain-text file `edit` hands the editor, and the directory that holds it.
struct Scratch {
    dir: PathBuf,
    file: PathBuf,
    ramfs: bool,
}

static CLEANUP: std::sync::OnceLock<(CString, CString)> = std::sync::OnceLock::new();

extern "C" fn clean_and_exit(_: libc::c_int) {
    if let Some((file, dir)) = CLEANUP.get() {
        unsafe {
            libc::unlink(file.as_ptr());
            libc::rmdir(dir.as_ptr());
        }
    }
    unsafe { libc::_exit(130) };
}

/// A fresh 0700 directory for the plain text: a private ramfs where one can be had, else $XDG_RUNTIME_DIR, which must be tmpfs or ramfs.
fn scratch(name: &str) -> Result<Scratch> {
    let forced = crate::testhooks::secrets_scratch();
    let base = match (&forced, unsafe { libc::geteuid() }) {
        (Some(base), _) => base.clone(),
        (None, 0) => PathBuf::from("/run"),
        (None, _) => std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).context(
            "XDG_RUNTIME_DIR isn't set, so there's no tmpfs to edit the plain text in; log in through a session that sets it",
        )?,
    };
    let dir = base.join(format!("archix-secret-{}", std::process::id()));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let private = match forced {
        Some(_) => Err(anyhow::anyhow!("a test asked for none")),
        None => private_ramfs(&dir),
    };
    let ramfs = match private {
        Ok(()) => true,
        Err(why) => {
            if !matches!(fs_type(&dir), Some(TMPFS_MAGIC | RAMFS_MAGIC)) {
                let _ = fs::remove_dir(&dir);
                bail!(
                    "{} isn't a tmpfs or ramfs, and no private ramfs could be mounted ({why:#}), so the plain text would land on disk; refusing",
                    base.display()
                );
            }
            false
        }
    };
    let file = dir.join(name);
    let _ = CLEANUP.set((
        CString::new(file.as_os_str().as_encoded_bytes())?,
        CString::new(dir.as_os_str().as_encoded_bytes())?,
    ));
    unsafe {
        libc::signal(libc::SIGTERM, clean_and_exit as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, clean_and_exit as *const () as libc::sighandler_t);
    }
    Ok(Scratch { dir, file, ramfs })
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Ok(mut bytes) = fs::read(&self.file) {
            bytes.iter_mut().for_each(|b| unsafe { std::ptr::write_volatile(b, 0) });
            let _ = fs::write(&self.file, &bytes);
        }
        let _ = fs::remove_file(&self.file);
        if self.ramfs {
            let c = CString::new(self.dir.as_os_str().as_encoded_bytes()).unwrap_or_default();
            unsafe { libc::umount2(c.as_ptr(), libc::MNT_DETACH) };
        }
        let _ = fs::remove_dir(&self.dir);
    }
}

/// The editor's command with what keeps it from leaving copies of the plain text, for vim and nvim; others get a warning.
fn editor_command(file: &Path) -> Command {
    let editor = std::env::var("VISUAL")
        .ok()
        .or_else(|| std::env::var("EDITOR").ok())
        .filter(|e| !e.trim().is_empty())
        .unwrap_or_else(|| "vi".to_string());
    let program = editor.split_whitespace().next().unwrap_or("vi");
    let base = Path::new(program)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let flags = match base.as_str() {
        "vim" | "gvim" | "vi" | "rvim" => {
            "-n -i NONE --cmd 'set noundofile nobackup nowritebackup noswapfile viminfo='"
        }
        "nvim" => "-n -i NONE --cmd 'set noundofile nobackup nowritebackup noswapfile shada='",
        _ => {
            eprintln!(
                "warning: archix knows how to keep vim and nvim from writing undo, swap and backup copies of the plain text, not {base}; check that it keeps none, or set EDITOR=vim"
            );
            ""
        }
    };
    let mut command = Command::new("sh");
    command
        .args(["-c", &format!("{editor} {flags} \"$1\""), "archix-secrets-edit"])
        .arg(file);
    command
}

/// Opens secret `name` in an editor on a private ramfs and encrypts what's saved to every recipient; an unchanged file changes nothing.
pub fn edit(config_dir: &Path, host: &str, state: &Path, name: &str, machine: bool) -> Result<()> {
    super::memory::harden(false)?;
    let hosts = hosts(config_dir, host)?;
    let config = &hosts.iter().find(|(h, _)| h == host).context("this host")?.1;
    let (_, item) = declared_here(config, name)?;
    let path = config_dir.join(&item.file);
    let before = match path.exists() {
        true => Some(decrypt_file(&path, &identities(state, machine)?, name)?),
        false => None,
    };
    if let Some(warning) = swap_warning() {
        eprintln!("{warning}");
    }
    let scratch = scratch(name)?;
    let mut file = fs::OpenOptions::new();
    std::os::unix::fs::OpenOptionsExt::mode(&mut file, 0o600);
    file.write(true)
        .create_new(true)
        .open(&scratch.file)
        .and_then(|mut f| std::io::Write::write_all(&mut f, before.as_ref().map_or(&[][..], |b| b.as_slice())))
        .with_context(|| format!("writing {}", scratch.file.display()))?;
    fs::set_permissions(&scratch.file, fs::Permissions::from_mode(0o600))?;
    let old_int = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
    let status = editor_command(&scratch.file).status();
    unsafe { libc::signal(libc::SIGINT, old_int) };
    let status = status.context("running the editor")?;
    if !status.success() {
        bail!("the editor exited with {status}; {} is unchanged", item.file);
    }
    let after = SecretBuf::read_all(
        fs::File::open(&scratch.file).with_context(|| format!("reading {}", scratch.file.display()))?,
        MAX_SECRET,
        &format!("secret {name}"),
    )?;
    drop(scratch);
    if before
        .as_ref()
        .is_some_and(|before| before.as_slice() == after.as_slice())
    {
        println!("secret {name} is unchanged; {} stays as it was", item.file);
        return Ok(());
    }
    let recipients = store(config_dir, &hosts, &item.file, after.as_slice())?;
    println!(
        "encrypted secret {name} to {} recipients into {}",
        recipients.len(),
        item.file
    );
    Ok(())
}

/// Encrypts the value from `from` (a file) or stdin as secret `name`.
pub fn set(config_dir: &Path, host: &str, name: &str, from: Option<&Path>) -> Result<()> {
    super::memory::harden(false)?;
    let hosts = hosts(config_dir, host)?;
    let config = &hosts.iter().find(|(h, _)| h == host).context("this host")?.1;
    let (_, item) = declared_here(config, name)?;
    let value = match from {
        Some(path) => SecretBuf::read_all(
            fs::File::open(path).with_context(|| format!("reading {}", path.display()))?,
            MAX_SECRET,
            &format!("secret {name}"),
        )?,
        None if std::io::stdin().is_terminal() => {
            let line = rpassword_line(&format!("value of secret {name} (not echoed): "))?;
            let buf = SecretBuf::from_slice(line.as_slice());
            drop(line);
            buf?
        }
        None => SecretBuf::read_all(
            std::io::stdin().lock().take(MAX_SECRET as u64 + 1),
            MAX_SECRET,
            &format!("secret {name}"),
        )?,
    };
    if value.is_empty() {
        bail!("the value is empty; {} is unchanged", item.file);
    }
    let recipients = store(config_dir, &hosts, &item.file, value.as_slice())?;
    println!(
        "encrypted secret {name} to {} recipients into {}",
        recipients.len(),
        item.file
    );
    Ok(())
}

/// One line from the terminal without echo, in locked memory.
fn rpassword_line(prompt: &str) -> Result<SecretBuf> {
    use std::io::Write;
    let stdin = std::io::stdin();
    let fd = 0;
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    let have = unsafe { libc::tcgetattr(fd, &mut saved) } == 0;
    if have {
        let mut quiet = saved;
        quiet.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &quiet) };
    }
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let mut out = SecretBuf::with_capacity(MAX_SECRET)?;
    let mut byte = [0u8; 1];
    let mut lock = stdin.lock();
    while lock.read(&mut byte).is_ok_and(|n| n == 1) && byte[0] != b'\n' {
        out.push(&byte)?;
    }
    if have {
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) };
    }
    eprintln!();
    Ok(out)
}

/// Re-encrypts `names` (every declared secret when empty) to what every host's config wants now.
pub fn rekey(config_dir: &Path, host: &str, state: &Path, names: &[String], machine: bool) -> Result<()> {
    super::memory::harden(false)?;
    let hosts = hosts(config_dir, host)?;
    let wanted = wanted(&hosts)?;
    let config = &hosts.iter().find(|(h, _)| h == host).context("this host")?.1;
    let declared = super::declared(config)?;
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    for (_, other) in &hosts {
        for (name, item) in super::declared(other)?.items {
            files.entry(item.file).or_insert(name);
        }
    }
    let chosen: Vec<(String, String)> = match names.is_empty() {
        true => files.iter().map(|(file, name)| (name.clone(), file.clone())).collect(),
        false => names
            .iter()
            .map(|name| {
                let item = declared
                    .items
                    .get(name)
                    .with_context(|| format!("secrets.items doesn't declare {name:?}"))?;
                Ok((name.clone(), item.file.clone()))
            })
            .collect::<Result<_>>()?,
    };
    let identities = identities(state, machine)?;
    let recorded = read_recorded(config_dir);
    let mut done = 0;
    for (name, file) in chosen {
        let path = config_dir.join(&file);
        if !path.exists() {
            println!("{file} doesn't exist yet; `archix secrets edit {name}` writes it");
            continue;
        }
        let want: Vec<String> = wanted.get(&file).cloned().unwrap_or_default().into_iter().collect();
        let current = fs::read(&path)?;
        let up_to_date = recorded
            .files
            .get(&file)
            .is_some_and(|r| r.recipients == want && r.sha256 == crate::fetch::sha256_bytes(&current));
        if up_to_date && !names.contains(&name) {
            continue;
        }
        let value = decrypt_file(&path, &identities, &name)?;
        store(config_dir, &hosts, &file, value.as_slice())?;
        println!("re-encrypted {file} to {} recipients", want.len());
        done += 1;
    }
    if done == 0 {
        println!("every secret is encrypted to the recipients the config names");
    } else {
        println!(
            "the old ciphertexts stay in the config's git history: if a key was lost or leaked, change the credentials themselves, not only their encryption"
        );
    }
    Ok(())
}

/// Per secret: who it should be encrypted to against who it was, whether this machine decrypts it, and what uses it; fails when any needs a rekey or doesn't decrypt.
pub fn check(config_dir: &Path, host: &str, state: &Path, rendered: Option<&super::Manifest>) -> Result<bool> {
    let hosts = hosts(config_dir, host)?;
    let wanted = wanted(&hosts)?;
    let config = &hosts.iter().find(|(h, _)| h == host).context("this host")?.1;
    let declared = super::declared(config)?;
    let recorded = read_recorded(config_dir);
    let machine = crypto::machine_identities(state).ok();
    let mut fine = true;
    if declared.items.is_empty() {
        println!("{host} declares no secrets");
    }
    for (name, item) in &declared.items {
        let path = config_dir.join(&item.file);
        println!("{name}  {}", item.file);
        let Ok(ciphertext) = fs::read(&path) else {
            println!("  missing: `archix secrets edit {name}` writes it");
            fine = false;
            continue;
        };
        let want: Vec<String> = wanted
            .get(&item.file)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        match recorded.files.get(&item.file) {
            Some(r) if r.sha256 != crate::fetch::sha256_bytes(&ciphertext) => {
                println!(
                    "  recipients: unknown, as it was encrypted outside archix; `archix secrets rekey {name}` encrypts it to the config's"
                );
                fine = false;
            }
            Some(r) if r.recipients == want => println!("  recipients: the {} the config names", want.len()),
            Some(r) => {
                let missing: Vec<&String> = want.iter().filter(|w| !r.recipients.contains(w)).collect();
                let extra: Vec<&String> = r.recipients.iter().filter(|w| !want.contains(w)).collect();
                println!(
                    "  recipients: differ from the config's{}{}; run `archix secrets rekey {name}`",
                    if missing.is_empty() {
                        String::new()
                    } else {
                        format!(
                            ", not encrypted to {}",
                            missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                        )
                    },
                    if extra.is_empty() {
                        String::new()
                    } else {
                        format!(
                            ", still encrypted to {}",
                            extra.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                        )
                    }
                );
                fine = false;
            }
            None => {
                println!(
                    "  recipients: unknown, as {RECIPIENTS} doesn't record it; `archix secrets rekey {name}` encrypts it to the config's"
                );
                fine = false;
            }
        }
        match &machine {
            Some(identities) => match crypto::opener(&ciphertext, identities) {
                Ok(by) if by.recipient == identities[0].recipient => println!("  this machine: decrypts it"),
                Ok(by) => println!(
                    "  this machine: decrypts it with the retired key {}; rekey before that key goes",
                    by.recipient
                ),
                Err(why) => {
                    println!("  this machine: doesn't decrypt it ({why})");
                    fine = false;
                }
            },
            None => println!("  this machine: can't tell without its key (run as root on the host)"),
        }
        let users = rendered
            .and_then(|m| m.secrets.get(name))
            .map(|e| e.consumers.clone())
            .unwrap_or_default();
        println!(
            "  used by: {}",
            if users.is_empty() {
                super::link_target(name)
            } else {
                users.join(", ")
            }
        );
    }
    Ok(fine)
}

/// `archix secrets init`: makes this machine's identity, or replaces it, and says what to do with the key.
pub fn init(state: &Path, replace: bool) -> Result<String> {
    let (recipient, made) = crypto::init(state, replace)?;
    let mut text = String::new();
    if made {
        text.push_str(&format!(
            "made this machine's secrets identity in {}\n",
            crypto::dir(state).display()
        ));
    } else {
        text.push_str("this machine already has a secrets identity; --replace makes a new one\n");
    }
    text.push_str(&format!(
        "put this in the host's config:\n\n  secrets = {{ machine_key = \"{recipient}\", admins = {{ \"<your age or ssh-ed25519 key>\" }} }}\n\n"
    ));
    if made {
        text.push_str(
            "archix never copies this identity anywhere: your admin recipients are the backup, so keep the admin key safe (on paper, a YubiKey or in a password manager). Without one, losing this machine's key loses every secret. Leave /var/lib/archix/secrets out of backups, or encrypt them.",
        );
    }
    Ok(text)
}

/// `archix secrets rotate-machine-key`: a new identity, the old one retired until no kept deployment needs it, or dropped at once when it leaked.
pub fn rotate(state: &Path, compromised: bool) -> Result<String> {
    let old = crypto::machine(state)?.recipient;
    let retired = crypto::retired_dir(state).join(&old);
    if compromised {
        fs::remove_file(crypto::identity_path(state)).context("removing the old identity")?;
    } else {
        crypto::retire(state)?;
    }
    let (new, _) = crypto::init(state, false)?;
    let mut text = format!(
        "this machine's new key is {new}\n1. put it in the host's secrets.machine_key, in place of {old}\n2. `archix secrets rekey` where an admin key is"
    );
    if compromised {
        text.push_str(
            "\nthe old identity is deleted: deployments whose secrets only it decrypts lose them at boot until a switch replaces them. Its ciphertexts in the config's git history are readable by whoever has it, so change every credential it protected",
        );
    } else {
        text.push_str(&format!(
            " (or `sudo archix secrets rekey --machine` here, which decrypts with the retired key)\n3. `archix switch` and reboot\nthe old key stays in {} while a kept deployment needs it, and gc deletes it after",
            retired.parent().unwrap_or(&retired).display()
        ));
    }
    Ok(text)
}

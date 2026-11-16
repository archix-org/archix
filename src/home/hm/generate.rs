//! The flake dir for one user: flake.nix, home.nix and files/, written as that user (SCOPE 7).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::exec::{Cmd, Exec, System, checked};
use super::{Account, Refusal, SECRET_MESSAGE, account, as_user, euid, flake_dir, nix, option_path, users};
use crate::lock::HmLock;
use crate::secrets::live::Values;
use crate::value::Value;

const MAX_DEPTH: usize = 64;
const AGE_HEADERS: [&[u8]; 2] = [b"age-encryption.org/v1", b"-----BEGIN AGE ENCRYPTED FILE-----"];

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Dir(String),
    File {
        rel: String,
        data: Vec<u8>,
        executable: bool,
    },
}

/// The flake dir's content before it's written; `files` paths are relative to files/.
#[derive(Debug, Clone, PartialEq)]
pub struct Rendered {
    pub home_nix: String,
    pub flake_nix: String,
    pub files: Vec<Entry>,
}

impl Rendered {
    /// A hash of everything written, copied files included, for the plan fingerprint.
    pub fn digest(&self) -> String {
        let mut bytes = Vec::new();
        for part in [&self.home_nix, &self.flake_nix] {
            bytes.extend((part.len() as u64).to_le_bytes());
            bytes.extend(part.as_bytes());
        }
        for entry in &self.files {
            match entry {
                Entry::Dir(rel) => bytes.extend(format!("d {rel}\0").as_bytes()),
                Entry::File { rel, data, executable } => {
                    bytes.extend(format!("f {rel} {executable} {}\0", data.len()).as_bytes());
                    bytes.extend(data);
                }
            }
        }
        crate::fetch::sha256_bytes(&bytes)
    }

    /// The flake dir as a tar stream: home.nix, flake.nix and files/.
    pub fn tar(&self) -> Result<Vec<u8>> {
        let mut builder = tar::Builder::new(Vec::new());
        let mut add = |path: &str, data: &[u8], mode: u32, dir: bool| -> Result<()> {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(if dir {
                tar::EntryType::Directory
            } else {
                tar::EntryType::Regular
            });
            header.set_size(data.len() as u64);
            header.set_mode(mode);
            header.set_mtime(0);
            header.set_uid(0);
            header.set_gid(0);
            builder
                .append_data(&mut header, path, data)
                .with_context(|| format!("adding {path} to the flake dir"))
        };
        add("home.nix", self.home_nix.as_bytes(), 0o644, false)?;
        add("flake.nix", self.flake_nix.as_bytes(), 0o644, false)?;
        add("files", &[], 0o755, true)?;
        for entry in &self.files {
            match entry {
                Entry::Dir(rel) => add(&format!("files/{rel}"), &[], 0o755, true)?,
                Entry::File { rel, data, executable } => add(
                    &format!("files/{rel}"),
                    data,
                    if *executable { 0o755 } else { 0o644 },
                    false,
                )?,
            }
        }
        Ok(builder.into_inner()?)
    }
}

/// What a copied file is checked against: the declared secrets' age files and, when known, their values.
#[derive(Default)]
pub struct Secrets<'a> {
    pub files: BTreeSet<PathBuf>,
    pub values: Option<&'a Values>,
}

impl<'a> Secrets<'a> {
    /// The age files secrets.items names, resolved in `config_dir`.
    pub fn declared(config: &Value, config_dir: &Path, values: Option<&'a Values>) -> Secrets<'a> {
        let mut files = BTreeSet::new();
        if let Value::Map(root) = config
            && let Some(Value::Map(secrets)) = root.get("secrets")
            && let Some(Value::Map(items)) = secrets.get("items")
        {
            for item in items.values() {
                if let Value::Map(item) = item
                    && let Some(Value::Str(file)) = item.get("file")
                    && let Ok(path) = config_dir.join(file).canonicalize()
                {
                    files.insert(path);
                }
            }
        }
        Secrets { files, values }
    }

    fn check(&self, source: &Path, data: &[u8], option: &str, rel: &str) -> Result<()> {
        let refuse = |why: String| -> Result<()> {
            Err(Refusal(format!("{option}: hm.path copies {rel}, which {why}; {SECRET_MESSAGE}")).into())
        };
        if self.files.contains(source) {
            return refuse("is a declared secret's file".into());
        }
        if AGE_HEADERS.iter().any(|h| data.starts_with(h)) {
            return refuse("is age-encrypted".into());
        }
        let lower = data.to_ascii_lowercase();
        if crate::secrets::memory::contains(&lower, b"{{archix-secret:") {
            return refuse("holds an archix.secret() placeholder".into());
        }
        if let Some(names) = self.values.and_then(|v| v.in_bytes(data)) {
            return refuse(format!("holds the value of secret {names}"));
        }
        Ok(())
    }
}

/// flake.nix pinning nixpkgs and home-manager at the lock's revs, with homeConfigurations.<user>.
pub fn flake_nix(user: &str, home: &Path, lock: &HmLock, system: &str) -> Result<String> {
    for (name, rev) in [("home-manager", &lock.home_manager), ("nixpkgs", &lock.nixpkgs)] {
        if !super::update::is_rev(rev) {
            bail!("archix.lock has no {name} rev for home-manager; run `archix update`");
        }
    }
    let quote = |s: &str| nix::string(s, "flake.nix");
    let home = home.to_str().context("the home directory isn't UTF-8")?;
    Ok(format!(
        r#"# Generated by archix (SCOPE 7); a switch overwrites it.
{{
  description = {description};
  inputs = {{
    nixpkgs.url = "github:NixOS/nixpkgs/{nixpkgs}";
    home-manager.url = "github:nix-community/home-manager/{home_manager}";
    home-manager.inputs.nixpkgs.follows = "nixpkgs";
  }};
  outputs = {{ nixpkgs, home-manager, ... }}: {{
    homeConfigurations.{user_attr} = home-manager.lib.homeManagerConfiguration {{
      pkgs = nixpkgs.legacyPackages.{system};
      modules = [
        ./home.nix
        {{
          home.username = nixpkgs.lib.mkDefault {user};
          home.homeDirectory = nixpkgs.lib.mkDefault {home};
        }}
      ];
    }};
  }};
}}
"#,
        description = quote(&format!("home-manager for {user}, generated by archix"))?,
        nixpkgs = lock.nixpkgs,
        home_manager = lock.home_manager,
        user_attr = nix::attr_name(user, "flake.nix")?,
        system = nix::attr_name(system, "flake.nix")?,
        user = quote(user)?,
        home = quote(home)?,
    ))
}

/// Nix's name for this machine's platform.
pub fn nix_system() -> String {
    format!("{}-linux", std::env::consts::ARCH)
}

/// The flake dir's content for `user`, whose home is `home`, from the evaluated `config`.
pub fn render(
    user: &str,
    home: &Path,
    config: &Value,
    config_dir: &Path,
    lock: &HmLock,
    secrets: &Secrets,
) -> Result<Rendered> {
    let users = users(config);
    let value = users
        .get(user)
        .with_context(|| format!("{user} has no home.{user}.home_manager"))?;
    let (home_nix, found) = nix::home_nix(value, &option_path(user))?;
    let flake_nix = flake_nix(user, home, lock, &nix_system())?;
    let root = config_dir
        .canonicalize()
        .with_context(|| format!("config directory {}", config_dir.display()))?;
    let mut entries = BTreeMap::new();
    for (option, rel) in &found.paths {
        copy(&root, rel, option, secrets, &mut entries)?;
    }
    Ok(Rendered {
        home_nix,
        flake_nix,
        files: entries.into_values().collect(),
    })
}

fn copy(root: &Path, rel: &str, option: &str, secrets: &Secrets, out: &mut BTreeMap<String, Entry>) -> Result<()> {
    let source = root.join(rel);
    source
        .symlink_metadata()
        .with_context(|| format!("{option}: hm.path {rel:?}: no such file in the config directory"))?;
    let real = source
        .canonicalize()
        .with_context(|| format!("{option}: hm.path {rel:?}"))?;
    if !real.starts_with(root) {
        bail!("{option}: hm.path {rel:?} leaves the config directory");
    }
    let mut prefix = String::new();
    for part in rel
        .split('/')
        .collect::<Vec<_>>()
        .split_last()
        .map(|(_, parents)| parents)
        .unwrap_or(&[])
    {
        prefix = if prefix.is_empty() {
            part.to_string()
        } else {
            format!("{prefix}/{part}")
        };
        out.entry(prefix.clone()).or_insert_with(|| Entry::Dir(prefix.clone()));
    }
    walk(root, &source, rel, option, secrets, out, 0)
}

fn walk(
    root: &Path,
    path: &Path,
    rel: &str,
    option: &str,
    secrets: &Secrets,
    out: &mut BTreeMap<String, Entry>,
    depth: usize,
) -> Result<()> {
    if depth > MAX_DEPTH {
        bail!("{option}: hm.path goes more than {MAX_DEPTH} levels deep at {rel}; is there a symlink loop?");
    }
    let link = path
        .symlink_metadata()
        .with_context(|| format!("{option}: reading {rel}"))?;
    let real = path
        .canonicalize()
        .with_context(|| format!("{option}: {rel} is a dangling symlink"))?;
    if link.file_type().is_symlink() && !real.starts_with(root) {
        bail!(
            "{option}: {rel} is a symlink to {}, outside the config directory",
            real.display()
        );
    }
    let meta = fs::metadata(path).with_context(|| format!("{option}: reading {rel}"))?;
    if meta.is_dir() {
        out.insert(rel.to_string(), Entry::Dir(rel.to_string()));
        let mut names: Vec<_> = fs::read_dir(path)
            .with_context(|| format!("{option}: listing {rel}"))?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<std::io::Result<_>>()
            .with_context(|| format!("{option}: listing {rel}"))?;
        names.sort();
        for name in names {
            let name_str = name
                .to_str()
                .with_context(|| format!("{option}: a file name in {rel} isn't UTF-8"))?;
            walk(
                root,
                &path.join(&name),
                &format!("{rel}/{name_str}"),
                option,
                secrets,
                out,
                depth + 1,
            )?;
        }
        return Ok(());
    }
    if !meta.is_file() {
        bail!("{option}: {rel} is neither a file nor a directory");
    }
    let data = fs::read(path).with_context(|| format!("{option}: reading {rel}"))?;
    secrets.check(&real, &data, option, rel)?;
    out.insert(
        rel.to_string(),
        Entry::File {
            rel: rel.to_string(),
            data,
            executable: meta.permissions().mode() & 0o111 != 0,
        },
    );
    Ok(())
}

/// Replaces home.nix, flake.nix and files/ in $1 from the tar on stdin, keeping flake.lock while flake.nix is unchanged and the gcroot.
pub const WRITE_SCRIPT: &str = r#"set -eu
d=$1
umask 022
mkdir -p -- "$d"
t=$(mktemp -d -- "$d/.archix-new.XXXXXX")
trap 'rm -rf -- "$t"' EXIT
tar -x --no-same-owner -f - -C "$t"
cmp -s -- "$t/flake.nix" "$d/flake.nix" || rm -f -- "$d/flake.lock"
rm -rf -- "$d/.archix-old"
if [ -e "$d/files" ] || [ -L "$d/files" ]; then mv -- "$d/files" "$d/.archix-old"; fi
mv -- "$t/files" "$d/files"
mv -f -- "$t/home.nix" "$d/home.nix"
mv -f -- "$t/flake.nix" "$d/flake.nix"
rm -rf -- "$d/.archix-old"
"#;

/// The command that writes the flake dir `dir` as `user`.
pub fn write_cmd(user: &str, account: &Account, self_uid: u32, dir: &Path, tar: Vec<u8>) -> Result<Cmd> {
    if self_uid != 0 && self_uid != account.uid {
        bail!("writing {user}'s home-manager flake needs root or {user}");
    }
    let dir = dir.to_str().context("the flake dir isn't UTF-8")?;
    let argv = ["sh", "-c", WRITE_SCRIPT, "archix-hm-write", dir]
        .map(String::from)
        .to_vec();
    Ok(Cmd {
        argv: as_user(user, account.uid, self_uid, argv),
        stdin: Some(tar),
    })
}

/// generate() with everything it reads from the machine passed in.
#[allow(clippy::too_many_arguments)]
pub fn generate_into(
    user: &str,
    config: &Value,
    config_dir: &Path,
    lock: &HmLock,
    account: &Account,
    dir: &Path,
    self_uid: u32,
    values: Option<&Values>,
    exec: &dyn Exec,
) -> Result<PathBuf> {
    let secrets = Secrets::declared(config, config_dir, values);
    let rendered = render(user, &account.home, config, config_dir, lock, &secrets)?;
    let cmd = write_cmd(user, account, self_uid, dir, rendered.tar()?)?;
    checked(
        exec,
        &cmd,
        &format!("writing {user}'s home-manager flake to {}", dir.display()),
    )?;
    Ok(dir.to_path_buf())
}

/// Writes ~<user>/.config/archix/home-manager/<user>/ as that user from the evaluated config `value`, and returns it.
pub fn generate(user: &str, value: &Value, config_dir: &Path, lock: &HmLock) -> Result<PathBuf> {
    let account = account(Path::new("/"), user)?;
    let dir = flake_dir(&account.home, user);
    let values = crate::secrets::live::read(Path::new("/")).ok().flatten();
    generate_into(
        user,
        value,
        config_dir,
        lock,
        &account,
        &dir,
        euid(),
        values.as_ref(),
        &System,
    )
}

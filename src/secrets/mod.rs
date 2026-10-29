//! Declared secrets: age files in the config, encrypted to this machine and the admins, decrypted only into archix's locked memory and, at boot, the ramfs at /run/archix/secrets.

pub mod activate;
pub mod crypto;
pub mod live;
pub mod memory;
pub mod prepare;
pub mod scan;
pub mod storage;
pub mod tool;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::value::Value;

/// The ramfs the boot decrypts into; a secret's link points at RUN_DIR/<name>.
pub const RUN_DIR: &str = "/run/archix/secrets";
/// Where each deployment keeps its secrets' ciphertexts, manifest and templates.
pub const DEPLOY_DIR: &str = "/usr/lib/archix/secrets";
pub const MANIFEST: &str = "/usr/lib/archix/secrets/manifest.json";
const OPEN: &str = "{{archix-secret:";
const CLOSE: &str = "}}";

/// Where secrets live on a running system that builds must never reach: the decrypted values and the machine's identity.
pub const BUILD_HIDDEN: [&str; 2] = [RUN_DIR, "/var/lib/archix/secrets"];

/// Runs `command` in a mount namespace of its own in which an empty read-only tmpfs covers each of BUILD_HIDDEN that exists, so a build (an AUR PKGBUILD is arbitrary code) can't read a value or the identity whatever its permissions.
pub fn hide_from(command: &mut std::process::Command) {
    use std::ffi::CString;
    use std::os::unix::process::CommandExt;
    let hidden: Vec<CString> = BUILD_HIDDEN
        .iter()
        .filter(|dir| Path::new(dir).is_dir())
        .filter_map(|dir| CString::new(*dir).ok())
        .collect();
    let (none, slash, tmpfs, options) = (c"none", c"/", c"tmpfs", c"mode=000,size=4k");
    if crate::testhooks::broken("secrets-build-mask") {
        return;
    }
    unsafe {
        command.pre_exec(move || {
            if libc::unshare(libc::CLONE_NEWNS) != 0
                || libc::mount(
                    none.as_ptr(),
                    slash.as_ptr(),
                    std::ptr::null(),
                    libc::MS_REC | libc::MS_PRIVATE,
                    std::ptr::null(),
                ) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            for dir in &hidden {
                let flags = libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
                if libc::mount(
                    tmpfs.as_ptr(),
                    dir.as_ptr(),
                    tmpfs.as_ptr(),
                    flags,
                    options.as_ptr().cast(),
                ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

/// What `archix.secret(name)` evaluates to: a placeholder, never the value.
pub fn placeholder(name: &str) -> String {
    format!("{OPEN}{name}{CLOSE}")
}

pub fn link_target(name: &str) -> String {
    format!("{RUN_DIR}/{name}")
}

pub fn template_target(id: &str) -> String {
    format!("{RUN_DIR}/files/{id}")
}

/// Whether a managed link points into the secrets' ramfs.
pub fn is_secret_link(target: &str) -> bool {
    target.starts_with(&format!("{RUN_DIR}/"))
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && name != "files"
}

/// The placeholders in `text`, in order, as (start, end, name).
pub fn placeholders(text: &str) -> Vec<(usize, usize, String)> {
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(start) = text[at..].find(OPEN).map(|i| at + i) {
        let name_start = start + OPEN.len();
        let Some(end) = text[name_start..].find(CLOSE).map(|i| name_start + i) else {
            break;
        };
        found.push((start, end + CLOSE.len(), text[name_start..end].to_string()));
        at = end + CLOSE.len();
    }
    found
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    File,
    CryptHash,
}

/// One declared secret as a generation records it: never its value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// The age file, relative to the config directory.
    pub file: String,
    /// The ciphertext's sha256, which names it in the deployment's DEPLOY_DIR.
    pub sha256: String,
    pub kind: Kind,
    pub owner: String,
    pub group: String,
    pub mode: u32,
    /// A path under /run the boot also writes it to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The files and accounts that use it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub consumers: Vec<String>,
    /// HMAC of the value under the machine's hash key, set when the switch could decrypt it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hmac: Option<String>,
    /// Left out of this deployment by `switch --without-secrets`: nothing decrypts it at boot.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub absent: bool,
}

/// A file whose text holds placeholders, rendered at boot into RUN_DIR/files/<id>.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Template {
    pub path: String,
    pub owner: String,
    pub group: String,
    pub mode: u32,
    pub secrets: Vec<String>,
}

/// Everything a deployment knows about its secrets, in its DEPLOY_DIR and its generation record.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub machine_key: String,
    #[serde(default)]
    pub admins: Vec<String>,
    #[serde(default)]
    pub secrets: BTreeMap<String, Entry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub templates: BTreeMap<String, Template>,
    /// Users whose password hash is a crypt_hash secret, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hashes: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_hibernation: bool,
}

impl Manifest {
    /// The paths whose files are links into the ramfs.
    pub fn links(&self) -> BTreeMap<String, String> {
        let mut links = BTreeMap::new();
        for (name, entry) in &self.secrets {
            for consumer in entry.consumers.iter().filter(|c| c.starts_with('/')) {
                links.insert(consumer.clone(), link_target(name));
            }
        }
        for (id, template) in &self.templates {
            links.insert(template.path.clone(), template_target(id));
        }
        links
    }

    pub fn read(root: &Path) -> Option<Manifest> {
        let path = root.join(MANIFEST.trim_start_matches('/'));
        serde_json::from_slice(&fs::read(path).ok()?).ok()
    }
}

/// `secrets` as the config declares it.
#[derive(Debug, Default)]
pub struct Declared {
    pub machine_key: Option<String>,
    pub admins: Vec<String>,
    pub without_admin: bool,
    pub allow_hibernation: bool,
    pub items: BTreeMap<String, Item>,
}

#[derive(Debug, Clone)]
pub struct Item {
    pub file: String,
    pub path: Option<String>,
    pub owner: String,
    pub group: String,
    pub mode: u32,
    pub kind: Kind,
}

fn get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Map(map) => map.get(key),
        _ => None,
    }
}

fn str_of<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    match get(value, key) {
        Some(Value::Str(s)) => Some(s),
        _ => None,
    }
}

/// The config's `secrets`.
pub fn declared(config: &Value) -> Result<Declared> {
    let Some(secrets) = get(config, "secrets") else {
        return Ok(Declared::default());
    };
    let mut out = Declared {
        machine_key: str_of(secrets, "machine_key").map(str::to_string),
        admins: match get(secrets, "admins") {
            Some(Value::List(items)) => items
                .iter()
                .filter_map(|item| match item {
                    Value::Str(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        },
        without_admin: get(secrets, "without_admin") == Some(&Value::Bool(true)),
        allow_hibernation: get(secrets, "allow_hibernation") == Some(&Value::Bool(true)),
        items: BTreeMap::new(),
    };
    if let Some(Value::Map(items)) = get(secrets, "items") {
        for (name, spec) in items {
            if !valid_name(name) {
                bail!("secrets.items: {name:?} can't name a secret; use letters, digits, - and _ (and not \"files\")");
            }
            let file = str_of(spec, "file")
                .with_context(|| format!("secrets.items.{name} needs file, its age file in the config"))?;
            let mode = u32::from_str_radix(str_of(spec, "mode").unwrap_or("0400"), 8)
                .with_context(|| format!("secrets.items.{name}.mode must be octal, like \"0400\""))?;
            let path = str_of(spec, "path").map(str::to_string);
            if let Some(path) = &path
                && (!path.starts_with("/run/")
                    || path.starts_with("/run/archix/")
                    || path.ends_with('/')
                    || path.split('/').any(|part| part == ".."))
            {
                bail!(
                    "secrets.items.{name}.path {path:?} must be a file under /run (outside /run/archix), which is gone at every shutdown; files elsewhere link to the secret with files[path] = {{ secret = \"{name}\" }}"
                );
            }
            let kind = match str_of(spec, "kind").unwrap_or("file") {
                "crypt_hash" => Kind::CryptHash,
                _ => Kind::File,
            };
            out.items.insert(
                name.clone(),
                Item {
                    file: file.to_string(),
                    path,
                    owner: str_of(spec, "owner").unwrap_or("root").to_string(),
                    group: str_of(spec, "group").unwrap_or("root").to_string(),
                    mode,
                    kind,
                },
            );
        }
    }
    Ok(out)
}

/// Refuses a placeholder anywhere but a file's text and a user's hashed_password, naming the option.
pub fn check_placement(config: &Value) -> Result<()> {
    fn walk(value: &Value, path: &mut Vec<String>, found: &mut Option<String>) {
        if found.is_some() {
            return;
        }
        match value {
            Value::Str(text) if text.contains(OPEN) => {
                let error = place_error(path);
                if !error.is_empty() {
                    *found = Some(error);
                }
            }
            Value::List(items) => {
                for (i, item) in items.iter().enumerate() {
                    path.push(format!("[{}]", i + 1));
                    walk(item, path, found);
                    path.pop();
                }
            }
            Value::Map(map) => {
                for (key, item) in map {
                    path.push(key.clone());
                    walk(item, path, found);
                    path.pop();
                }
            }
            _ => {}
        }
    }
    let mut found = None;
    walk(config, &mut Vec::new(), &mut found);
    match found {
        Some(error) => bail!("{error}"),
        None => Ok(()),
    }
}

/// The error for a placeholder at `path`, or "" where one may go.
fn place_error(path: &[String]) -> String {
    let parts: Vec<&str> = path.iter().map(String::as_str).collect();
    let name = path.join(".");
    match parts[..] {
        ["files", _, "text"] => String::new(),
        ["users", _, "hashed_password"] => String::new(),
        ["files", file, "merge", ..] => format!(
            "files.{file}.merge: a secret can't be merged into a file the app also writes, which is in a deployment's /etc; give the app the secret through an environment file or credential instead"
        ),
        ["units" | "user_units", unit, ..] => format!(
            "{}.{unit}: a unit file is loaded before any secret is decrypted, so it can't hold one; give the service EnvironmentFile= or LoadCredential=NAME:{RUN_DIR}/<secret> (archix.secret_path)",
            parts[0]
        ),
        ["home", "files", ..] => format!("{name}: home files can't hold secrets yet; link a file in /etc instead"),
        _ => format!(
            "{name}: a secret can't go here; secrets go into files (secret = name, or archix.secret in text) and users.<name>.hashed_password, and units take them with EnvironmentFile= or LoadCredential="
        ),
    }
}

/// The id of the template at `path`.
pub fn template_id(path: &str) -> String {
    crate::fetch::sha256_bytes(path.as_bytes())[..16].to_string()
}

/// What render collected about the files and users that use secrets, before the ciphertexts are read.
#[derive(Debug, Default)]
pub struct Uses {
    /// Secret name to the paths linked to it.
    pub links: BTreeMap<String, Vec<String>>,
    pub templates: BTreeMap<String, (Template, String)>,
    pub hashes: BTreeMap<String, String>,
}

/// A secret used in `what` that the config doesn't declare, or of the wrong kind.
fn check_use(declared: &Declared, name: &str, kind: Kind, what: &str) -> Result<()> {
    let Some(item) = declared.items.get(name) else {
        bail!("{what} uses secret {name:?}, which secrets.items doesn't declare");
    };
    if item.kind != kind {
        bail!(
            "{what} uses secret {name:?}, whose kind is {}; {}",
            if item.kind == Kind::CryptHash {
                "crypt_hash"
            } else {
                "file"
            },
            if kind == Kind::CryptHash {
                "a password hash comes from a secret of kind = \"crypt_hash\""
            } else {
                "a crypt_hash secret goes only into users.<name>.hashed_password"
            }
        );
    }
    Ok(())
}

/// A deployment's secret files: each ciphertext, the templates and the manifest, at their paths with their contents; None when nothing is declared.
pub struct Files {
    pub manifest: Manifest,
    pub contents: Vec<(String, Vec<u8>)>,
}

/// The manifest file's content.
pub fn manifest_bytes(manifest: &Manifest) -> Vec<u8> {
    let mut text = serde_json::to_string_pretty(manifest).expect("a manifest serializes");
    text.push('\n');
    text.into_bytes()
}

/// Reads every declared secret's ciphertext from the config and makes the deployment's secret files.
pub fn files(declared: &Declared, uses: &Uses, config_dir: &Path) -> Result<Option<Files>> {
    if declared.items.is_empty() {
        return Ok(None);
    }
    let Some(machine_key) = &declared.machine_key else {
        bail!(
            "secrets.machine_key isn't set; `sudo archix secrets init` makes this machine's key and prints the line for the host's config"
        );
    };
    crypto::parse_recipient(machine_key).context("secrets.machine_key")?;
    if declared.admins.is_empty() && !declared.without_admin {
        bail!(
            "secrets.admins is empty: an admin recipient (your own age or ssh-ed25519 key) is the only way back to these secrets if this machine's key is lost; add one, or set secrets.without_admin = true to go on knowing that losing the key loses every secret"
        );
    }
    for (i, admin) in declared.admins.iter().enumerate() {
        crypto::parse_recipient(admin).with_context(|| format!("secrets.admins[{}]", i + 1))?;
    }
    let root = config_dir.canonicalize()?;
    let mut manifest = Manifest {
        machine_key: machine_key.clone(),
        admins: declared.admins.clone(),
        allow_hibernation: declared.allow_hibernation,
        hashes: uses.hashes.clone(),
        ..Manifest::default()
    };
    let mut contents = Vec::new();
    let mut seen = BTreeSet::new();
    for (name, item) in &declared.items {
        let full = root.join(&item.file);
        if !full.starts_with(&root) || item.file.split('/').any(|part| part == "..") {
            bail!("secrets.items.{name}.file {:?} leaves the config directory", item.file);
        }
        let ciphertext = match fs::read(&full) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(
                "secrets.items.{name}.file {} doesn't exist yet; `archix secrets edit {name}` or `archix secrets set {name}` writes it",
                item.file
            ),
            Err(e) => return Err(e).with_context(|| format!("reading {}", full.display())),
        };
        if !crypto::is_age(&ciphertext) {
            bail!(
                "secrets.items.{name}.file {} isn't an age file; never put a plaintext file there, `archix secrets set {name}` encrypts one",
                item.file
            );
        }
        if ciphertext.len() > 2 * memory::MAX_SECRET {
            bail!("secrets.items.{name}.file {} is too big for a secret", item.file);
        }
        let sha256 = crate::fetch::sha256_bytes(&ciphertext);
        if seen.insert(sha256.clone()) {
            contents.push((format!("{DEPLOY_DIR}/{sha256}.age"), ciphertext));
        }
        let mut consumers = uses.links.get(name).cloned().unwrap_or_default();
        consumers.extend(
            uses.templates
                .values()
                .filter(|(template, _)| template.secrets.contains(name))
                .map(|(template, _)| template.path.clone()),
        );
        consumers.extend(
            uses.hashes
                .iter()
                .filter(|(_, secret)| *secret == name)
                .map(|(user, _)| format!("users.{user}.hashed_password")),
        );
        consumers.sort();
        consumers.dedup();
        manifest.secrets.insert(
            name.clone(),
            Entry {
                file: item.file.clone(),
                sha256,
                kind: item.kind,
                owner: item.owner.clone(),
                group: item.group.clone(),
                mode: item.mode,
                path: item.path.clone(),
                consumers,
                hmac: None,
                absent: false,
            },
        );
    }
    for (id, (template, text)) in &uses.templates {
        manifest.templates.insert(id.clone(), template.clone());
        contents.push((format!("{DEPLOY_DIR}/templates/{id}"), text.as_bytes().to_vec()));
    }
    Ok(Some(Files { manifest, contents }))
}

/// Records that `path`'s text uses secrets, as a template; returns the link target the file becomes.
pub fn add_template(
    declared: &Declared,
    uses: &mut Uses,
    path: &str,
    text: &str,
    owner: &str,
    group: &str,
    mode: u32,
) -> Result<String> {
    let mut names = Vec::new();
    for (_, _, name) in placeholders(text) {
        check_use(declared, &name, Kind::File, &format!("files.{path}.text"))?;
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let id = template_id(path);
    let template = Template {
        path: path.to_string(),
        owner: owner.to_string(),
        group: group.to_string(),
        mode: mode & !0o007,
        secrets: names,
    };
    uses.templates.insert(id.clone(), (template, text.to_string()));
    Ok(template_target(&id))
}

/// Records that `path` is a link to secret `name`; returns its target.
pub fn add_link(declared: &Declared, uses: &mut Uses, path: &str, name: &str) -> Result<String> {
    check_use(declared, name, Kind::File, &format!("files.{path}"))?;
    uses.links.entry(name.to_string()).or_default().push(path.to_string());
    Ok(link_target(name))
}

/// The secret a user's hashed_password names, if it's a placeholder; refuses anything else that holds one.
pub fn hash_secret(declared: &Declared, uses: &mut Uses, user: &str, value: &str) -> Result<Option<String>> {
    let found = placeholders(value);
    let Some((start, end, name)) = found.first() else {
        return Ok(None);
    };
    if found.len() > 1 || *start != 0 || *end != value.len() {
        bail!("users.{user}.hashed_password must be archix.secret(\"name\") alone, not text around it");
    }
    check_use(
        declared,
        name,
        Kind::CryptHash,
        &format!("users.{user}.hashed_password"),
    )?;
    uses.hashes.insert(user.to_string(), name.clone());
    Ok(Some(name.clone()))
}

/// What `archix status` says about secrets: those this boot couldn't decrypt or a switch left out, links someone replaced with plain files, and where values could reach a disk; `current` is the current generation's manifest.
pub fn status_lines(paths: &crate::paths::Paths, current: Option<&Manifest>) -> Vec<String> {
    let root = &paths.root;
    let booted = Manifest::read(root);
    let mut lines = Vec::new();
    let declared = booted.iter().chain(current).any(|m| !m.secrets.is_empty());
    if !declared {
        return lines;
    }
    if let Some(manifest) = &booted {
        match activate::read_report(root) {
            Some(report) => {
                for (name, why) in &report.missing {
                    lines.push(format!("secret {name} is missing at this boot: {why}"));
                }
            }
            None if !manifest.secrets.is_empty() => lines.push(format!(
                "secrets: this boot didn't decrypt them ({} hasn't run); `systemctl status {}` says why",
                crate::integration::SECRETS_UNIT,
                crate::integration::SECRETS_UNIT
            )),
            None => {}
        }
        let deployment = crate::system::boot::booted_deployment(root);
        for (path, target) in manifest.links() {
            let at = root.join(path.trim_start_matches('/'));
            if fs::symlink_metadata(&at).is_ok_and(|meta| meta.is_file()) {
                let what = manifest
                    .secrets
                    .iter()
                    .find(|(name, _)| target == link_target(name))
                    .map_or("a template of secrets".to_string(), |(name, _)| {
                        format!("secret {name}")
                    });
                lines.push(format!(
                    "warning: {path} should be a link to {what} but is a file; it holds plain text on disk{}, and the next switch refuses until it's moved away",
                    deployment.map_or(String::new(), |n| format!(" in deployment {n}"))
                ));
            }
        }
        let hashed: Vec<&String> = manifest.hashes.keys().collect();
        if !hashed.is_empty() {
            lines.push(format!(
                "secrets: the password hashes of {} come from secrets and are in /etc/shadow, as passwd would put them; the encryption protects the config, not the machine",
                hashed.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
    }
    if let Some(manifest) = booted.as_ref().or(current) {
        lines.extend(plaintext_lines(paths, manifest));
    }
    for (name, entry) in current.map(|m| &m.secrets).into_iter().flatten() {
        if entry.absent {
            lines.push(format!(
                "secret {name} is left out of the current deployment (switched --without-secrets); `archix secrets rekey` where an admin key is, then switch"
            ));
        }
    }
    if !crypto::identity_path(&paths.state).exists() && unsafe { libc::geteuid() } == 0 {
        lines.push("warning: secrets are declared but this machine has no secrets identity; `sudo archix secrets init` makes one".to_string());
    }
    if !storage::root_encrypted() {
        lines.push(
            "warning: secrets are declared and the root filesystem isn't encrypted, so whoever reads the disk reads the identity in /var/lib/archix/secrets and every secret with it; put the root on LUKS".to_string(),
        );
    }
    let swaps = storage::unencrypted_swaps(root);
    if !swaps.is_empty() {
        lines.push(format!(
            "warning: swap isn't encrypted ({}): what services read from their secrets can be swapped out to it",
            swaps.join(", ")
        ));
        if !crate::system::sleep::secrets_at_risk(root).is_empty() {
            lines.push(crate::system::sleep::secrets_note(&swaps));
        }
    }
    lines
}

/// Files in the running /etc, and the pending deployment's, that hold a value of the booted deployment's file secrets on disk, as the /etc merge refuses to carry; reading the values takes root, and `manifest` is the booted one when it can be read.
fn plaintext_lines(paths: &crate::paths::Paths, manifest: &Manifest) -> Vec<String> {
    let root = &paths.root;
    if !manifest
        .secrets
        .values()
        .any(|entry| entry.kind == Kind::File && !entry.absent)
    {
        return Vec::new();
    }
    if unsafe { libc::geteuid() } != 0 {
        return vec![format!(
            "secrets: whether a file in /etc holds a secret's value takes root to check, as the values in {RUN_DIR} are root's: `sudo archix status`"
        )];
    }
    let values = match live::read(root) {
        Ok(Some(values)) => values,
        Ok(None) => return Vec::new(),
        Err(e) => {
            return vec![format!(
                "secrets: reading their values to check /etc for them failed: {e:#}"
            )];
        }
    };
    let skip: BTreeSet<String> = manifest
        .links()
        .into_keys()
        .filter_map(|path| path.strip_prefix("/etc/").map(str::to_string))
        .collect();
    let mut places = vec![(root.join("etc"), crate::system::boot::booted_deployment(root))];
    if let Some((pending, _)) = crate::etcmerge::pending(root) {
        places.push((crate::etcmerge::deployment_etc(root, pending), Some(pending)));
    }
    let mut lines = Vec::new();
    for (etc, number) in places {
        for (path, names) in live::scan_etc(&etc, &values, &skip) {
            lines.push(format!(
                "warning: {path} holds secret {names} in plaintext on disk{}; remove it (change a NetworkManager connection whose secret archix places with `nmcli connection modify --temporary`, or in the config)",
                number.map_or(String::new(), |n| format!(" (deployment {n})"))
            ));
        }
    }
    lines
}

/// A template's text with each placeholder replaced by its value, in locked memory.
pub fn render_template(text: &[u8], values: &BTreeMap<String, &memory::SecretBuf>) -> Result<memory::SecretBuf> {
    let text = std::str::from_utf8(text).context("a template isn't UTF-8")?;
    let found = placeholders(text);
    let mut size = text.len();
    for (_, _, name) in &found {
        size += values.get(name).map_or(0, |value| value.len());
    }
    let mut out = memory::SecretBuf::with_capacity(size)?;
    let mut at = 0;
    for (start, end, name) in found {
        out.push(&text.as_bytes()[at..start])?;
        let value = values
            .get(&name)
            .with_context(|| format!("the template needs secret {name}, which isn't here"))?;
        out.push(value.as_slice())?;
        at = end;
    }
    out.push(&text.as_bytes()[at..])?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, Value)]) -> Value {
        Value::Map(pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect())
    }

    #[test]
    fn a_placeholder_goes_only_into_file_text_and_hashed_password() {
        let token = Value::Str(placeholder("wg0"));
        let fine = map(&[
            (
                "files",
                map(&[(
                    "/etc/x",
                    map(&[("text", Value::Str(format!("key={}", placeholder("wg0"))))]),
                )]),
            ),
            ("users", map(&[("alice", map(&[("hashed_password", token.clone())]))])),
        ]);
        check_placement(&fine).unwrap();
        for (config, says) in [
            (
                map(&[("kernel_params", Value::List(vec![token.clone()]))]),
                "kernel_params.[1]: a secret can't go here",
            ),
            (
                map(&[("units", map(&[("x.service", token.clone())]))]),
                "LoadCredential=",
            ),
            (
                map(&[(
                    "files",
                    map(&[("/etc/x", map(&[("merge", map(&[("values", token.clone())]))]))]),
                )]),
                "can't be merged",
            ),
            (
                map(&[("hooks", map(&[("h", map(&[("run", token.clone())]))]))]),
                "hooks.h.run",
            ),
            (
                map(&[("environment", map(&[("variables", map(&[("K", token.clone())]))]))]),
                "environment.variables.K",
            ),
            (map(&[("packages", Value::List(vec![token.clone()]))]), "packages.[1]"),
            (
                map(&[
                    ("files", map(&[("/etc/x", map(&[("text", token.clone())]))])),
                    ("kernel_params", Value::List(vec![token.clone()])),
                ]),
                "kernel_params.[1]",
            ),
        ] {
            let error = check_placement(&config).unwrap_err().to_string();
            assert!(error.contains(says), "{error}");
        }
    }

    #[test]
    fn templates_render_each_placeholder_and_hashes_must_be_one_whole_placeholder() {
        let text = format!("a={}\nb={}\n", placeholder("one"), placeholder("two"));
        assert_eq!(
            placeholders(&text).iter().map(|p| p.2.as_str()).collect::<Vec<_>>(),
            ["one", "two"]
        );
        let (one, two) = (
            memory::SecretBuf::from_slice(b"1111").unwrap(),
            memory::SecretBuf::from_slice(b"22").unwrap(),
        );
        let values: BTreeMap<String, &memory::SecretBuf> =
            [("one".to_string(), &one), ("two".to_string(), &two)].into();
        assert_eq!(
            render_template(text.as_bytes(), &values).unwrap().as_slice(),
            b"a=1111\nb=22\n"
        );
        let mut declared = Declared::default();
        declared.items.insert(
            "pw".into(),
            Item {
                file: "pw.age".into(),
                path: None,
                owner: "root".into(),
                group: "root".into(),
                mode: 0o400,
                kind: Kind::CryptHash,
            },
        );
        let mut uses = Uses::default();
        assert_eq!(
            hash_secret(&declared, &mut uses, "alice", &placeholder("pw"))
                .unwrap()
                .as_deref(),
            Some("pw")
        );
        assert!(hash_secret(&declared, &mut uses, "alice", &format!("x{}", placeholder("pw"))).is_err());
        let error = add_link(&declared, &mut uses, "/etc/x", "pw").unwrap_err().to_string();
        assert!(error.contains("crypt_hash"), "{error}");
        assert!(
            add_link(&declared, &mut uses, "/etc/x", "nope")
                .unwrap_err()
                .to_string()
                .contains("doesn't declare")
        );
    }
}

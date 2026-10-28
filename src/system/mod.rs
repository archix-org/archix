pub mod activate;
pub mod apply;
pub mod authenticode;
pub mod blobs;
pub mod boot;
pub mod btrfs;
pub mod capabilities;
pub mod credentials;
pub mod deploy_boot;
pub mod emergency;
pub mod firewall;
pub mod fstab;
pub mod health;
pub mod hooks;
pub mod merge;
pub mod password;
pub mod reboot;
pub mod rescue;
pub mod rescue_auth;
pub mod revoke;
pub mod secureboot;
pub mod sleep;
pub mod usr_local;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::fetch::sha256_bytes;
use crate::value::Value;

/// Everything outside packages that archix manages, as one generation applied it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SystemState {
    #[serde(default)]
    pub files: BTreeMap<String, ManagedFile>,
    #[serde(default)]
    pub users: BTreeMap<String, User>,
    #[serde(default)]
    pub groups: BTreeMap<String, Group>,
    #[serde(default)]
    pub services: BTreeMap<String, bool>,
    #[serde(default)]
    pub user_services: BTreeMap<String, bool>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hooks: BTreeMap<String, Hook>,
    /// Scripts for managed files, keyed by path, run when a switch changes that file.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub on_change: BTreeMap<String, Hook>,
    /// Extra versions per package that a downgrade mustn't cross without asking.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub unsafe_downgrades: BTreeMap<String, Vec<String>>,
    /// File capabilities in libcap's text form, keyed by a file's path in the deployment's /usr or /opt.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub capabilities: BTreeMap<String, String>,
    /// The /etc/fstab lines file_systems and swap declare, after archix's own for the root and the ESP.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fstab: Vec<fstab::FstabLine>,
    /// The declared secrets, as the deployment's manifest in /usr/lib/archix/secrets has them; never a value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<crate::secrets::Manifest>,
}

/// A script run after a switch; `user` None means root.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hook {
    pub run: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
}

/// A file archix writes: either content (by sha256 in the blob store) or a symlink.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManagedFile {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    pub mode: u32,
    pub owner: String,
    pub group: String,
    /// Keys to set in a file an app also writes, instead of owning the whole file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge: Option<merge::Merge>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct User {
    pub uid: Option<u32>,
    pub group: Option<String>,
    pub groups: Vec<String>,
    pub shell: String,
    pub home: Option<String>,
    pub description: String,
    pub system: bool,
    /// Set at every switch; it's in the config, so in every generation record too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hashed_password: Option<String>,
    /// Set only when the account is created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_hashed_password: Option<String>,
    /// A crypt_hash secret, by name, whose value the deployment's boot sets as the password.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hashed_password_secret: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Group {
    pub gid: Option<u32>,
    pub system: bool,
}

/// The rendered target system plus the contents of every file it writes, keyed by sha256.
pub struct Rendered {
    pub state: SystemState,
    pub blobs: BTreeMap<String, Vec<u8>>,
    /// Contents that come from a download instead: sha256 to url, fetched into the blob store at switch.
    pub fetches: BTreeMap<String, String>,
    /// What a switch and diff should warn about the config, such as tmpfiles lines that delete under /home.
    pub warnings: Vec<String>,
}

impl Rendered {
    /// Replaces the secrets' manifest, as a switch fills in what it decrypted, in the state and in the file the deployment gets.
    pub fn set_secrets(&mut self, manifest: crate::secrets::Manifest) {
        let content = crate::secrets::manifest_bytes(&manifest);
        let sha256 = sha256_bytes(&content);
        if let Some(file) = self.state.files.get_mut(crate::secrets::MANIFEST) {
            file.sha256 = sha256.clone();
        }
        self.blobs.insert(sha256, content);
        self.state.secrets = Some(manifest);
    }

    /// A secret's ciphertext by its sha256, as render read it from the config.
    pub fn ciphertext(&self, sha256: &str) -> Option<Vec<u8>> {
        let path = format!("{}/{sha256}.age", crate::secrets::DEPLOY_DIR);
        let file = self.state.files.get(&path)?;
        self.blobs.get(&file.sha256).cloned()
    }
}

const FORBIDDEN: &[&str] = &[
    "/usr/", "/proc/", "/sys/", "/dev/", "/run/", "/tmp/", "/home/", "/boot/",
];

struct Builder {
    state: SystemState,
    blobs: BTreeMap<String, Vec<u8>>,
    fetches: BTreeMap<String, String>,
}

impl Builder {
    fn text(&mut self, path: &str, content: &str) {
        self.content(path, content.as_bytes().to_vec(), 0o644, "root", "root");
    }

    fn content(&mut self, path: &str, content: Vec<u8>, mode: u32, owner: &str, group: &str) {
        let sha256 = sha256_bytes(&content);
        self.blobs.insert(sha256.clone(), content);
        let file = ManagedFile {
            sha256,
            link: None,
            mode,
            owner: owner.to_string(),
            group: group.to_string(),
            merge: None,
        };
        self.state.files.insert(path.to_string(), file);
    }

    fn fetched(&mut self, path: &str, url: &str, sha256: &str, mode: u32, owner: &str, group: &str) -> Result<()> {
        if sha256.len() != 64 || !sha256.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("{path}: sha256 must be 64 hex digits; `archix prefetch {url}` prints it");
        }
        let sha256 = sha256.to_ascii_lowercase();
        self.fetches.insert(sha256.clone(), url.to_string());
        let file = ManagedFile {
            sha256,
            link: None,
            mode,
            owner: owner.to_string(),
            group: group.to_string(),
            merge: None,
        };
        self.state.files.insert(path.to_string(), file);
        Ok(())
    }

    fn merged(&mut self, path: &str, merge: merge::Merge, owner: &str, group: &str) {
        let file = ManagedFile {
            sha256: String::new(),
            link: None,
            mode: 0o644,
            owner: owner.to_string(),
            group: group.to_string(),
            merge: Some(merge),
        };
        self.state.files.insert(path.to_string(), file);
    }

    fn on_change(&mut self, path: &str, spec: &Value, user: Option<&str>) {
        if let Some(run) = str_of(spec, "on_change") {
            let hook = Hook {
                run: run.to_string(),
                user: user.map(str::to_string),
                after: Vec::new(),
            };
            self.state.on_change.insert(path.to_string(), hook);
        }
    }

    fn link(&mut self, path: &str, target: &str) {
        self.owned_link(path, target, "root", "root");
    }

    fn owned_link(&mut self, path: &str, target: &str, owner: &str, group: &str) {
        let file = ManagedFile {
            sha256: String::new(),
            link: Some(target.to_string()),
            mode: 0o777,
            owner: owner.to_string(),
            group: group.to_string(),
            merge: None,
        };
        self.state.files.insert(path.to_string(), file);
    }
}

fn map<'a>(config: &'a Value, key: &str) -> Option<&'a BTreeMap<String, Value>> {
    match config {
        Value::Map(root) => match root.get(key) {
            Some(Value::Map(map)) => Some(map),
            _ => None,
        },
        _ => None,
    }
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

fn strings(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::List(items)) => items
            .iter()
            .filter_map(|item| match item {
                Value::Str(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn unit_name(name: &str) -> Result<&str> {
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        bail!("{name:?} is not a valid file name");
    }
    Ok(name)
}

fn lines(value: &Value) -> String {
    match value {
        Value::Str(s) if s.ends_with('\n') => s.clone(),
        Value::Str(s) => format!("{s}\n"),
        _ => String::new(),
    }
}

/// Turns the evaluated config into the system state archix should apply.
pub fn render(config: &Value, config_dir: &Path) -> Result<Rendered> {
    let mut b = Builder {
        state: SystemState::default(),
        blobs: BTreeMap::new(),
        fetches: BTreeMap::new(),
    };
    crate::secrets::check_placement(config)?;
    let secrets = crate::secrets::declared(config)?;
    let mut uses = crate::secrets::Uses::default();

    for (path, spec) in map(config, "files").into_iter().flatten() {
        if !path.starts_with('/') || path.ends_with('/') || path.contains("/../") {
            bail!("files: {path:?} must be an absolute file path");
        }
        if path == "/etc/fstab" {
            bail!(
                "files: /etc/fstab is archix's to write, with its own lines for the root and the ESP; declare mounts in file_systems and swap instead"
            );
        }
        if FORBIDDEN.iter().any(|prefix| path.starts_with(prefix)) && !path.starts_with("/usr/local/") {
            bail!(
                "files: {path} is in a directory archix doesn't write to; home files go in home.files, and boot entries in boot.extra_entries"
            );
        }
        let mut mode = u32::from_str_radix(str_of(spec, "mode").unwrap_or("0644"), 8)
            .with_context(|| format!("files.{path}.mode must be octal, like \"0644\""))?;
        let owner = str_of(spec, "owner").unwrap_or("root");
        let group = str_of(spec, "group").unwrap_or("root");
        let immutable = read_only(config, spec);
        if immutable {
            mode &= !0o222;
        }
        let merge = merge_of(spec, immutable, &format!("files.{path}"))?;
        let sources = [
            str_of(spec, "text").is_some(),
            str_of(spec, "source").is_some(),
            str_of(spec, "link").is_some(),
            str_of(spec, "url").is_some(),
            merge.is_some(),
            str_of(spec, "secret").is_some(),
        ];
        if sources.iter().filter(|set| **set).count() != 1 {
            bail!("files.{path}: give exactly one of text, source, link, url, merge or secret");
        }
        let templated = str_of(spec, "text").is_some_and(|text| !crate::secrets::placeholders(text).is_empty());
        if (templated || str_of(spec, "secret").is_some()) && path.starts_with("/usr/local/") {
            bail!(
                "files.{path}: a secret can't go into /usr/local, which every deployment shares; link it from /etc or /srv"
            );
        }
        if let Some(name) = str_of(spec, "secret") {
            let target = crate::secrets::add_link(&secrets, &mut uses, path, name)?;
            b.link(path, &target);
        } else if templated {
            let text = lines(&Value::Str(str_of(spec, "text").unwrap_or_default().to_string()));
            let target = crate::secrets::add_template(&secrets, &mut uses, path, &text, owner, group, mode)?;
            b.link(path, &target);
        } else if let Some(merge) = merge {
            b.merged(path, merge, owner, group);
        } else if let Some(url) = str_of(spec, "url") {
            let sha256 = str_of(spec, "sha256").with_context(|| format!("files.{path}: url needs sha256"))?;
            b.fetched(path, url, sha256, mode, owner, group)?;
        } else if let Some(text) = str_of(spec, "text") {
            b.content(
                path,
                lines(&Value::Str(text.to_string())).into_bytes(),
                mode,
                owner,
                group,
            );
        } else if let Some(source) = str_of(spec, "source") {
            let root = config_dir.canonicalize()?;
            let full = root
                .join(source)
                .canonicalize()
                .with_context(|| format!("files.{path}.source {source:?}"))?;
            if !full.starts_with(&root) {
                bail!("files.{path}.source {source:?} leaves the config directory");
            }
            b.content(
                path,
                fs::read(&full).with_context(|| format!("reading {}", full.display()))?,
                mode,
                owner,
                group,
            );
        } else if let Some(link) = str_of(spec, "link") {
            b.link(path, link);
        }
        b.on_change(path, spec, None);
    }

    for (key, dir) in [("units", "/etc/systemd/system"), ("user_units", "/etc/systemd/user")] {
        for (name, text) in map(config, key).into_iter().flatten() {
            b.text(&format!("{dir}/{}", unit_name(name)?), &lines(text));
        }
    }
    for (name, text) in map(config, "udev_rules").into_iter().flatten() {
        b.text(&format!("/etc/udev/rules.d/{}.rules", unit_name(name)?), &lines(text));
    }
    for (name, text) in map(config, "modprobe").into_iter().flatten() {
        b.text(&format!("/etc/modprobe.d/{}.conf", unit_name(name)?), &lines(text));
    }
    if let Some(sysctl) = map(config, "sysctl").filter(|m| !m.is_empty()) {
        let mut text = String::new();
        for (key, value) in sysctl {
            let value = match value {
                Value::Str(s) => s.clone(),
                Value::Bool(v) => u8::from(*v).to_string(),
                other => other.to_string(),
            };
            text.push_str(&format!("{key} = {value}\n"));
        }
        b.text("/etc/sysctl.d/90-archix.conf", &text);
    }
    let root = |key: &str| match config {
        Value::Map(root) => root.get(key),
        _ => None,
    };
    let modules = strings(root("kernel_modules"));
    if !modules.is_empty() {
        b.text("/etc/modules-load.d/archix.conf", &format!("{}\n", modules.join("\n")));
    }
    let params = strings(root("kernel_params"));
    if !params.is_empty() {
        b.text("/etc/kernel/cmdline", &format!("{}\n", params.join(" ")));
    }
    if let Some(Value::Str(name)) = root("hostname") {
        b.text("/etc/hostname", &format!("{name}\n"));
    }
    if let Some(Value::Str(zone)) = root("timezone") {
        if zone.contains("..") || zone.starts_with('/') {
            bail!("timezone {zone:?} is not a zoneinfo name");
        }
        b.link("/etc/localtime", &format!("/usr/share/zoneinfo/{zone}"));
    }
    let locales = strings(root("locales"));
    if let Some(first) = locales.first() {
        let generated: Vec<String> = locales
            .iter()
            .map(|locale| {
                let charset = locale.split_once('.').map_or("ISO-8859-1", |(_, charset)| charset);
                format!("{locale} {charset}")
            })
            .collect();
        b.text("/etc/locale.gen", &format!("{}\n", generated.join("\n")));
        b.text("/etc/locale.conf", &format!("LANG={first}\n"));
    }
    if let Some(Value::Str(keymap)) = root("keymap") {
        b.text("/etc/vconsole.conf", &format!("KEYMAP={keymap}\n"));
    }
    for unit in strings(root("masked_units")) {
        b.link(&format!("/etc/systemd/system/{}", unit_name(&unit)?), "/dev/null");
    }
    if let Some(Value::Str(target)) = root("default_target") {
        if !target.ends_with(".target") {
            bail!("default_target {target:?} isn't a .target unit");
        }
        b.link(
            "/etc/systemd/system/default.target",
            &format!("/usr/lib/systemd/system/{}", unit_name(target)?),
        );
    }
    render_environment(&mut b, root("environment"));
    render_appimages(&mut b, root("appimages"));
    for (path, text) in map(config, "capabilities").into_iter().flatten() {
        if let Value::Str(text) = text {
            b.state.capabilities.insert(path.clone(), text.clone());
        }
    }
    capabilities::validate(&b.state.capabilities)?;
    if let Some(boot) = root("boot") {
        let esp = str_of(boot, "esp").unwrap_or("/boot").trim_end_matches('/').to_string();
        for (name, text) in map(boot, "extra_entries").into_iter().flatten() {
            if !name.ends_with(".conf") {
                bail!("boot.extra_entries: {name:?} must end in .conf, as systemd-boot reads only those");
            }
            if name.starts_with("archix-") {
                bail!(
                    "boot.extra_entries: {name:?} starts with archix-, which archix's own generation entries use; pick another name"
                );
            }
            b.text(&format!("{esp}/loader/entries/{}", unit_name(name)?), &lines(text));
        }
        if !strings(get(boot, "health_check").and_then(|check| get(check, "reachable"))).is_empty() {
            b.text(
                &format!("/etc/systemd/system/{}.d/network.conf", crate::integration::HEALTH_UNIT),
                "[Unit]\nWants=network-online.target\nAfter=network-online.target\n",
            );
        }
    }
    let mounts = fstab::render(config, fstab::esp_of(config))?;
    for (path, text, from) in mounts.files {
        if b.state.files.contains_key(&path) {
            bail!("files: {path} is written by archix from {from}; leave it out of files");
        }
        b.text(&path, &text);
    }
    b.state.fstab = mounts.lines;
    if let Some(rules) = firewall::ruleset(root("firewall").unwrap_or(&Value::Null))? {
        b.text(firewall::RULES, &rules);
        b.text(&format!("/etc/systemd/system/{}", firewall::UNIT), &firewall::unit());
        b.state.services.insert(firewall::UNIT.to_string(), true);
    }

    render_home(&mut b, config, config_dir)?;

    for (name, spec) in map(config, "groups").into_iter().flatten() {
        let gid = match get(spec, "gid") {
            Some(Value::Int(gid)) => Some(u32::try_from(*gid).context("groups: gid out of range")?),
            _ => None,
        };
        b.state.groups.insert(
            name.clone(),
            Group {
                gid,
                system: get(spec, "system") == Some(&Value::Bool(true)),
            },
        );
    }
    for (name, spec) in map(config, "users").into_iter().flatten() {
        let uid = match get(spec, "uid") {
            Some(Value::Int(uid)) => Some(u32::try_from(*uid).context("users: uid out of range")?),
            _ => None,
        };
        let declared_hash = str_of(spec, "hashed_password");
        let hashed_password_secret = match declared_hash {
            Some(value) => crate::secrets::hash_secret(&secrets, &mut uses, name, value)?,
            None => None,
        };
        let user = User {
            uid,
            group: str_of(spec, "group").map(str::to_string),
            groups: strings(get(spec, "groups")),
            shell: str_of(spec, "shell").unwrap_or("/bin/bash").to_string(),
            home: str_of(spec, "home").map(str::to_string),
            description: str_of(spec, "description").unwrap_or("").to_string(),
            system: get(spec, "system") == Some(&Value::Bool(true)),
            hashed_password: declared_hash
                .filter(|_| hashed_password_secret.is_none())
                .map(str::to_string),
            initial_hashed_password: str_of(spec, "initial_hashed_password").map(str::to_string),
            hashed_password_secret,
        };
        if user.hashed_password_secret.is_some() && user.initial_hashed_password.is_some() {
            bail!("users.{name}: give hashed_password or initial_hashed_password, not both");
        }
        if user.hashed_password.is_some() && user.initial_hashed_password.is_some() {
            bail!("users.{name}: give hashed_password or initial_hashed_password, not both");
        }
        for (key, hash) in [
            ("hashed_password", &user.hashed_password),
            ("initial_hashed_password", &user.initial_hashed_password),
        ] {
            if let Some(hash) = hash {
                credentials::check_hash(&format!("users.{name}.{key}"), hash)?;
            }
        }
        let keys = strings(get(spec, "ssh_authorized_keys"));
        for (i, key) in keys.iter().enumerate() {
            credentials::check_key(&format!("users.{name}.ssh_authorized_keys[{}]", i + 1), key)?;
        }
        if !keys.is_empty() {
            b.text(
                &format!("{}/{}", credentials::KEYS_DIR, unit_name(name)?),
                &format!("{}\n", keys.join("\n")),
            );
        }
        b.state.users.insert(name.clone(), user);
    }
    if b.state.files.keys().any(|path| path.starts_with(credentials::KEYS_DIR)) {
        b.text(
            credentials::SSHD_DROP_IN,
            &format!(
                "# Written by archix: users' keys from the config, besides their own ~/.ssh/authorized_keys.\nAuthorizedKeysFile .ssh/authorized_keys {}/%u\n",
                credentials::KEYS_DIR
            ),
        );
    }
    if let Some(sudo) = root("sudo")
        && get(sudo, "enable") == Some(&Value::Bool(true))
    {
        let text = credentials::sudoers(
            get(sudo, "wheel") != Some(&Value::Bool(false)),
            get(sudo, "wheel_needs_password") != Some(&Value::Bool(false)),
            &strings(get(sudo, "extra_rules")),
        )?;
        b.content(credentials::SUDOERS, text.into_bytes(), 0o440, "root", "root");
    }
    let tmpfiles = strings(root("tmpfiles"));
    let mut warnings = Vec::new();
    if !tmpfiles.is_empty() {
        let esp = root("boot").and_then(|boot| str_of(boot, "esp")).unwrap_or("/efi");
        let own = ["/archix", "/var/lib/archix", "/var/usrlocal", esp].map(str::to_string);
        warnings.extend(credentials::check_tmpfiles(&tmpfiles, &own)?);
        b.text(credentials::TMPFILES, &format!("{}\n", tmpfiles.join("\n")));
    }
    for (key, target) in [
        ("services", &mut b.state.services),
        ("user_services", &mut b.state.user_services),
    ] {
        for (name, enabled) in map(config, key).into_iter().flatten() {
            target.insert(unit_name(name)?.to_string(), *enabled == Value::Bool(true));
        }
    }
    for (name, versions) in map(config, "unsafe_downgrades").into_iter().flatten() {
        b.state.unsafe_downgrades.insert(name.clone(), strings(Some(versions)));
    }
    for (name, spec) in map(config, "hooks").into_iter().flatten() {
        let hook = Hook {
            run: str_of(spec, "run").unwrap_or_default().to_string(),
            user: str_of(spec, "user").map(str::to_string),
            after: strings(get(spec, "after")),
        };
        b.state.hooks.insert(name.clone(), hook);
    }
    hooks::order(&b.state.hooks)?;
    if let Some(files) = crate::secrets::files(&secrets, &uses, config_dir)? {
        for (path, content) in files.contents {
            b.content(&path, content, 0o600, "root", "root");
        }
        b.content(
            crate::secrets::MANIFEST,
            crate::secrets::manifest_bytes(&files.manifest),
            0o600,
            "root",
            "root",
        );
        b.state.secrets = Some(files.manifest);
    }
    Ok(Rendered {
        state: b.state,
        blobs: b.blobs,
        fetches: b.fetches,
        warnings,
    })
}

/// Where the appimages module puts AppImage `name` in each deployment's /opt.
pub fn appimage_path(name: &str) -> String {
    format!("/opt/archix/appimages/{name}.AppImage")
}

/// Each AppImage's command in /usr/bin and menu entry in /usr/share/applications, inside the deployment beside the AppImage in its /opt, so they come and go with it on a switch or rollback; the appimages module checks the names.
fn render_appimages(b: &mut Builder, appimages: Option<&Value>) {
    let Some(Value::Map(appimages)) = appimages else {
        return;
    };
    for (name, image) in appimages {
        let target = appimage_path(name);
        b.link(&format!("/usr/bin/{name}"), &target);
        if get(image, "desktop") == Some(&Value::Bool(false)) {
            continue;
        }
        let mut lines = vec![
            "[Desktop Entry]".to_string(),
            "Type=Application".to_string(),
            format!("Name={}", str_of(image, "name").unwrap_or(name)),
            format!("Exec={target} %U"),
        ];
        if let Some(icon) = str_of(image, "icon") {
            lines.push(format!("Icon={icon}"));
        }
        let categories = strings(get(image, "categories"));
        if !categories.is_empty() {
            lines.push(format!("Categories={};", categories.join(";")));
        }
        b.text(
            &format!("/usr/share/applications/archix-{name}.desktop"),
            &format!("{}\n", lines.join("\n")),
        );
    }
}

/// `environment.variables` for login shells and systemd user sessions, and `environment.shell_aliases` for login shells.
fn render_environment(b: &mut Builder, environment: Option<&Value>) {
    let as_text = |value: &Value| match value {
        Value::Str(s) => s.clone(),
        Value::List(items) => items
            .iter()
            .map(|item| match item {
                Value::Str(s) => s.clone(),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(":"),
        other => other.to_string(),
    };
    let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    // Double quotes let $VAR expand in shells the way environment.d expands it, so both mean the same.
    let expanding = |s: &str| {
        format!(
            "\"{}\"",
            s.replace('\\', "\\\\").replace('"', "\\\"").replace('`', "\\`")
        )
    };
    if let Some(Value::Map(variables)) = environment.and_then(|e| get(e, "variables"))
        && !variables.is_empty()
    {
        let (mut shell, mut session) = (String::new(), String::new());
        for (name, value) in variables {
            let value = as_text(value);
            shell.push_str(&format!("export {name}={}\n", expanding(&value)));
            session.push_str(&format!("{name}={value}\n"));
        }
        b.text("/etc/profile.d/archix-environment.sh", &shell);
        b.text("/etc/environment.d/90-archix.conf", &session);
    }
    if let Some(Value::Map(aliases)) = environment.and_then(|e| get(e, "shell_aliases"))
        && !aliases.is_empty()
    {
        let text: String = aliases
            .iter()
            .map(|(name, command)| format!("alias {name}={}\n", quote(&as_text(command))))
            .collect();
        b.text("/etc/profile.d/archix-aliases.sh", &text);
    }
}

/// Home files of `home.user`, owned by them, keyed by absolute path.
fn render_home(b: &mut Builder, config: &Value, config_dir: &Path) -> Result<()> {
    let Some(home) = config_get(config, "home") else {
        return Ok(());
    };
    let files = match get(home, "files") {
        Some(Value::Map(files)) => files.clone(),
        _ => BTreeMap::new(),
    };
    let Some(user) = str_of(home, "user") else {
        if !files.is_empty() {
            bail!("home.files needs home.user, the user whose home it is");
        }
        return Ok(());
    };
    let declared = map(config, "users").and_then(|users| users.get(user));
    let dir = str_of(home, "directory")
        .map(str::to_string)
        .or_else(|| declared.and_then(|u| str_of(u, "home")).map(str::to_string))
        .unwrap_or_else(|| format!("/home/{user}"));
    let group = declared.and_then(|u| str_of(u, "group")).unwrap_or(user).to_string();
    let root = config_dir.canonicalize()?;
    for (relative, spec) in &files {
        let relative = relative.strip_prefix("~/").unwrap_or(relative);
        if relative.is_empty() || relative.starts_with('/') || relative.split('/').any(|part| part == "..") {
            bail!("home.files: {relative:?} must be a path inside the home directory");
        }
        let path = format!("{}/{relative}", dir.trim_end_matches('/'));
        let mut mode = if get(spec, "executable") == Some(&Value::Bool(true)) {
            0o755
        } else {
            0o644
        };
        let immutable = read_only(config, spec);
        if immutable {
            mode &= !0o222;
        }
        let merge = merge_of(spec, immutable, &format!("home.files.{relative}"))?;
        let live = get(spec, "live") == Some(&Value::Bool(true));
        let sources = [
            str_of(spec, "text").is_some(),
            str_of(spec, "source").is_some(),
            str_of(spec, "link").is_some(),
            str_of(spec, "url").is_some(),
            merge.is_some(),
        ];
        if sources.iter().filter(|set| **set).count() != 1 {
            bail!("home.files.{relative}: give exactly one of text, source, link, url or merge");
        }
        if let Some(merge) = merge {
            b.merged(&path, merge, user, &group);
            b.on_change(&path, spec, Some(user));
            continue;
        }
        if live && str_of(spec, "source").is_none() {
            bail!("home.files.{relative}: live = true needs source, the file in the config to link to");
        }
        if let Some(url) = str_of(spec, "url") {
            let sha256 = str_of(spec, "sha256").with_context(|| format!("home.files.{relative}: url needs sha256"))?;
            b.fetched(&path, url, sha256, mode, user, &group)?;
        } else if let Some(text) = str_of(spec, "text") {
            b.content(&path, text.as_bytes().to_vec(), mode, user, &group);
        } else if let Some(link) = str_of(spec, "link") {
            b.owned_link(&path, link, user, &group);
        } else if let Some(source) = str_of(spec, "source") {
            let full = root
                .join(source)
                .canonicalize()
                .with_context(|| format!("home.files.{relative}.source {source:?}"))?;
            if !full.starts_with(&root) {
                bail!("home.files.{relative}.source {source:?} leaves the config directory");
            }
            if live {
                b.owned_link(&path, &full.to_string_lossy(), user, &group);
            } else {
                let content = fs::read(&full)
                    .with_context(|| format!("reading {}; directories need live = true", full.display()))?;
                b.content(&path, content, mode, user, &group);
            }
        }
        b.on_change(&path, spec, Some(user));
    }
    Ok(())
}

/// Whether a file is written read-only: its own `mutable`, else the config's `mutable_files`.
fn read_only(config: &Value, spec: &Value) -> bool {
    match get(spec, "mutable") {
        Some(Value::Bool(mutable)) => !mutable,
        _ => config_get(config, "mutable_files") == Some(&Value::Bool(false)),
    }
}

/// The `merge = { format, values }` of a file spec, if it has one.
fn merge_of(spec: &Value, immutable: bool, what: &str) -> Result<Option<merge::Merge>> {
    let Some(Value::Map(merge)) = get(spec, "merge") else {
        return Ok(None);
    };
    let format = match merge.get("format") {
        Some(Value::Str(format)) => format.clone(),
        _ => bail!("{what}.merge needs a format: ini, kconfig or json"),
    };
    let values = serde_json::to_value(merge.get("values").unwrap_or(&Value::Null))?;
    let merge = merge::Merge {
        immutable: immutable && format == "kconfig",
        format,
        values,
    };
    merge.apply("").with_context(|| format!("{what}.merge"))?;
    Ok(Some(merge))
}

/// Whether the config lets a switch leave nobody a password, as `allow_no_password_login`.
pub fn allows_no_password_login(config: &Value) -> bool {
    config_get(config, "allow_no_password_login") == Some(&Value::Bool(true))
}

fn config_get<'a>(config: &'a Value, key: &str) -> Option<&'a Value> {
    match config {
        Value::Map(root) => root.get(key),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(json: serde_json::Value) -> Value {
        fn convert(v: serde_json::Value) -> Value {
            match v {
                serde_json::Value::Null => Value::Null,
                serde_json::Value::Bool(b) => Value::Bool(b),
                serde_json::Value::Number(n) => n.as_i64().map_or(Value::Float(n.as_f64().unwrap()), Value::Int),
                serde_json::Value::String(s) => Value::Str(s),
                serde_json::Value::Array(items) => Value::List(items.into_iter().map(convert).collect()),
                serde_json::Value::Object(map) => Value::Map(map.into_iter().map(|(k, v)| (k, convert(v))).collect()),
            }
        }
        convert(json)
    }

    #[test]
    fn a_health_check_with_hosts_to_reach_waits_for_the_network_first() {
        let dropin = "/etc/systemd/system/archix-health-check.service.d/network.conf";
        let with = |check: serde_json::Value| {
            render(
                &config(serde_json::json!({ "boot": { "health_check": check } })),
                Path::new("/"),
            )
            .unwrap()
            .state
            .files
        };
        assert!(with(serde_json::json!({ "reachable": ["gateway"] })).contains_key(dropin));
        assert!(!with(serde_json::json!({ "reachable": [], "listening": [22] })).contains_key(dropin));
    }

    #[test]
    fn renders_every_kind_of_setting_to_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("motd"), "hello\n").unwrap();
        let rendered = render(
            &config(serde_json::json!({
                "files": {
                    "/etc/motd": { "source": "motd", "mode": "0600", "owner": "root", "group": "root" },
                    "/etc/issue": { "text": "Arch", "mode": "0644", "owner": "root", "group": "root" },
                },
                "units": { "hello.service": "[Service]\nExecStart=/bin/true" },
                "sysctl": { "vm.swappiness": 10, "net.ipv4.ip_forward": true },
                "kernel_modules": ["i2c-dev"],
                "kernel_params": ["quiet", "splash"],
                "hostname": "box",
                "timezone": "Europe/Istanbul",
                "locales": ["en_US.UTF-8", "tr_TR.UTF-8"],
                "services": { "sshd.service": true, "cups.service": false },
            })),
            dir.path(),
        )
        .unwrap();
        let files = &rendered.state.files;
        let text = |path: &str| String::from_utf8(rendered.blobs[&files[path].sha256].clone()).unwrap();
        assert_eq!(text("/etc/motd"), "hello\n");
        assert_eq!(files["/etc/motd"].mode, 0o600);
        assert_eq!(text("/etc/issue"), "Arch\n");
        assert_eq!(
            text("/etc/systemd/system/hello.service"),
            "[Service]\nExecStart=/bin/true\n"
        );
        assert_eq!(
            text("/etc/sysctl.d/90-archix.conf"),
            "net.ipv4.ip_forward = 1\nvm.swappiness = 10\n"
        );
        assert_eq!(text("/etc/kernel/cmdline"), "quiet splash\n");
        assert_eq!(text("/etc/locale.gen"), "en_US.UTF-8 UTF-8\ntr_TR.UTF-8 UTF-8\n");
        assert_eq!(text("/etc/locale.conf"), "LANG=en_US.UTF-8\n");
        assert_eq!(
            files["/etc/localtime"].link.as_deref(),
            Some("/usr/share/zoneinfo/Europe/Istanbul")
        );
        assert!(!rendered.state.services["cups.service"]);
    }

    #[test]
    fn masked_units_and_the_default_target_are_links() {
        let dir = tempfile::tempdir().unwrap();
        let rendered = render(
            &config(serde_json::json!({
                "masked_units": ["sleep.target", "suspend.target"],
                "default_target": "multi-user.target",
            })),
            dir.path(),
        )
        .unwrap();
        let link = |path: &str| rendered.state.files[path].link.clone();
        assert_eq!(link("/etc/systemd/system/sleep.target").as_deref(), Some("/dev/null"));
        assert_eq!(link("/etc/systemd/system/suspend.target").as_deref(), Some("/dev/null"));
        assert_eq!(
            link("/etc/systemd/system/default.target").as_deref(),
            Some("/usr/lib/systemd/system/multi-user.target")
        );
        let wrong = serde_json::json!({ "default_target": "sshd.service" });
        assert!(render(&config(wrong), dir.path()).is_err());
    }

    #[test]
    fn on_change_runs_as_the_file_owner_and_hooks_must_order() {
        let dir = tempfile::tempdir().unwrap();
        let rendered = render(
            &config(serde_json::json!({
                "files": { "/etc/x": { "text": "x", "on_change": "systemctl restart x" } },
                "home": { "user": "u", "files": { ".y": { "text": "y", "on_change": "echo y" } } },
                "hooks": { "late": { "run": "true", "after": ["early"] }, "early": { "run": "true", "user": "u" } },
            })),
            dir.path(),
        )
        .unwrap();
        let state = &rendered.state;
        assert_eq!(state.on_change["/etc/x"].user, None);
        assert_eq!(state.on_change["/home/u/.y"].user.as_deref(), Some("u"));
        assert_eq!(hooks::order(&state.hooks).unwrap(), ["early", "late"]);
        let cycle = serde_json::json!({ "hooks": { "a": { "run": "true", "after": ["a"] } } });
        assert!(render(&config(cycle), dir.path()).is_err());
    }

    #[test]
    fn rejects_unsafe_paths_and_ambiguous_files() {
        let dir = tempfile::tempdir().unwrap();
        for files in [
            serde_json::json!({ "relative": { "text": "x" } }),
            serde_json::json!({ "/home/u/.bashrc": { "text": "x" } }),
            serde_json::json!({ "/etc/x": { "text": "x", "link": "/y" } }),
            serde_json::json!({ "/etc/x": { "source": "../../etc/passwd" } }),
        ] {
            assert!(
                render(&config(serde_json::json!({ "files": files })), dir.path()).is_err(),
                "{files}"
            );
        }
        let live_text =
            serde_json::json!({ "home": { "user": "u", "files": { ".x": { "text": "x", "live": true } } } });
        assert!(render(&config(live_text), dir.path()).is_err());
        for (name, why) in [
            ("archix-5.conf", "starts with archix-"),
            ("memtest", "must end in .conf"),
        ] {
            let boot =
                serde_json::json!({ "boot": { "loader": "systemd-boot", "extra_entries": { name: "title x" } } });
            let error = render(&config(boot), dir.path())
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            assert!(error.contains(why), "{name}: {error}");
        }
    }
}

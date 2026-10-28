//! What the config declares about logging in and the files every boot sets up: password hashes, SSH keys, sudo rules and tmpfiles lines, checked before anything is staged, and the shadow entries a build sets.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use super::User;
use crate::etcmerge::accounts;

/// Where each user's declared keys go in a deployment.
pub const KEYS_DIR: &str = "/etc/ssh/authorized_keys.d";
/// The sshd drop-in that reads them besides ~/.ssh/authorized_keys.
pub const SSHD_DROP_IN: &str = "/etc/ssh/sshd_config.d/10-archix-authorized-keys.conf";
pub const SUDOERS: &str = "/etc/sudoers.d/archix";
pub const TMPFILES: &str = "/etc/tmpfiles.d/archix.conf";

const KEY_TYPES: &[&str] = &[
    "ssh-ed25519",
    "ssh-rsa",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "sk-ssh-ed25519@openssh.com",
    "sk-ecdsa-sha2-nistp256@openssh.com",
];

fn crypt_alphabet(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'/')
}

/// Refuses anything but a crypt(3) hash libxcrypt accepts for a login, or "!" or "*" for a locked account; never prints the value.
pub fn check_hash(what: &str, hash: &str) -> Result<()> {
    if hash.is_empty() {
        bail!(
            "{what} is empty, which would let anyone log in without a password; give a hash, or \"!\" to lock the account"
        );
    }
    if hash.chars().any(|c| c == ':' || c.is_whitespace() || c.is_control()) {
        bail!("{what} holds a colon, a space or a line break, so it isn't a crypt(3) hash; `mkpasswd` prints one");
    }
    if matches!(hash, "!" | "*" | "!*") {
        return Ok(());
    }
    let body = hash.strip_prefix('!').unwrap_or(hash);
    let Some(rest) = body.strip_prefix('$') else {
        bail!(
            "{what} isn't a crypt(3) hash and looks like a plaintext password; hash it with `mkpasswd` (yescrypt) or `openssl passwd -6`, or give \"!\" to lock the account"
        );
    };
    let parts: Vec<&str> = rest.split('$').collect();
    let well_formed = match parts[0] {
        "y" | "gy" => parts.len() == 4 && crypt_alphabet(parts[1]) && crypt_alphabet(parts[2]) && parts[3].len() == 43,
        "7" => parts.len() == 3 && crypt_alphabet(parts[1]) && parts[2].len() == 43,
        "6" | "5" => {
            let (salt, digest) = match parts[..] {
                [_, rounds, salt, digest]
                    if rounds.strip_prefix("rounds=").is_some_and(|n| n.parse::<u32>().is_ok()) =>
                {
                    (salt, digest)
                }
                [_, salt, digest] => (salt, digest),
                _ => ("", ""),
            };
            let length = if parts[0] == "6" { 86 } else { 43 };
            crypt_alphabet(salt) && salt.len() <= 16 && digest.len() == length
        }
        "2a" | "2b" | "2y" => {
            parts.len() == 3
                && parts[1].len() == 2
                && parts[1].bytes().all(|b| b.is_ascii_digit())
                && parts[2].len() == 53
        }
        "1" | "md5" | "sha1" | "3" | "sun-md5" | "nt" => {
            bail!(
                "{what} is an old hash type a stolen /etc/shadow gives up quickly; use yescrypt ($y$, `mkpasswd`) or SHA-512 ($6$, `openssl passwd -6`)"
            )
        }
        _ => bail!(
            "{what} is a crypt(3) hash of a type archix doesn't know; use yescrypt ($y$, `mkpasswd`) or SHA-512 ($6$, `openssl passwd -6`)"
        ),
    };
    let last = parts.last().copied().unwrap_or_default();
    if !well_formed || !crypt_alphabet(last) {
        bail!(
            "{what} starts like a ${}$ hash but isn't a whole one; copy all of what `mkpasswd` printed",
            parts[0]
        );
    }
    Ok(())
}

fn base64(text: &str) -> Option<Vec<u8>> {
    let value = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let text = text.trim_end_matches('=');
    let (mut out, mut bits, mut acc) = (Vec::new(), 0, 0u32);
    for c in text.bytes() {
        acc = (acc << 6) | value(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Refuses a line sshd wouldn't take as a key: options are allowed, then a key type it knows and that type's key.
pub fn check_key(what: &str, line: &str) -> Result<()> {
    if line.trim().is_empty() || line.chars().any(|c| c == '\n' || c == '\r') {
        bail!("{what} must be one key on one line, such as `ssh-ed25519 AAAA... me@laptop`");
    }
    let tokens: Vec<&str> = line.split_whitespace().collect();
    if tokens.contains(&"ssh-dss") {
        bail!(
            "{what} is an ssh-dss key, which OpenSSH no longer accepts; make an ed25519 key with `ssh-keygen -t ed25519`"
        );
    }
    let Some(at) = tokens.iter().position(|token| KEY_TYPES.contains(token)) else {
        bail!(
            "{what} names no key type sshd knows ({}); it should look like `ssh-ed25519 AAAA... me@laptop`, as in a .pub file",
            KEY_TYPES.join(", ")
        );
    };
    let kind = tokens[at];
    let decoded = tokens.get(at + 1).and_then(|blob| base64(blob));
    let named = decoded.as_deref().and_then(|bytes| {
        let length = u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?) as usize;
        bytes.get(4..4 + length)
    });
    if named != Some(kind.as_bytes()) {
        bail!("{what}: what follows {kind} isn't a {kind} key; copy the whole line of the .pub file");
    }
    Ok(())
}

/// The sudoers drop-in: wheel's rule and the config's own lines.
pub fn sudoers(wheel: bool, wheel_needs_password: bool, rules: &[String]) -> Result<String> {
    let mut text = String::from("# Written by archix from the config's sudo settings; edit those instead.\n");
    if wheel {
        text.push_str(match wheel_needs_password {
            true => "%wheel ALL=(ALL:ALL) ALL\n",
            false => "%wheel ALL=(ALL:ALL) NOPASSWD: ALL\n",
        });
    }
    for rule in rules {
        if rule.contains('\n') || rule.trim().is_empty() {
            bail!("sudo.extra_rules: {rule:?} must be one sudoers line");
        }
        text.push_str(rule.trim());
        text.push('\n');
    }
    Ok(text)
}

const TMPFILES_TYPES: &str = "fFwdDevqQpLcbCxXrRzZtThHaA";

/// Checks tmpfiles lines against archix's own paths, which no line may create, clean, change or remove, nor reach recursively from above; returns warnings for removals under /home.
pub fn check_tmpfiles(lines: &[String], protected: &[String]) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.contains('\n') {
            bail!("tmpfiles: {trimmed:?} must be one line each");
        }
        let fields: Vec<&str> = trimmed.split_whitespace().collect();
        let kind = fields[0];
        let base = kind.chars().next().unwrap_or(' ');
        if !TMPFILES_TYPES.contains(base) || !kind[1..].chars().all(|c| "+!-=~^$?".contains(c)) {
            bail!(
                "tmpfiles: {trimmed:?} starts with {kind:?}, which isn't a tmpfiles.d type (d, f, L, z, r, R, ...); see man tmpfiles.d"
            );
        }
        let Some(path) = fields.get(1) else {
            bail!("tmpfiles: {trimmed:?} has no path");
        };
        if !path.starts_with('/') && !path.starts_with('%') {
            bail!("tmpfiles: {trimmed:?}: the path must be absolute");
        }
        if matches!(base, 'x' | 'X') {
            continue;
        }
        let literal = path[..path.find(['*', '?', '[', '%']).unwrap_or(path.len())].trim_end_matches('/');
        let cleans = fields.get(5).is_some_and(|age| *age != "-");
        let recursive = matches!(base, 'R' | 'D' | 'Z' | 'T' | 'A' | 'e') || cleans;
        for own in protected {
            let own = own.trim_end_matches('/');
            let inside = literal == own || literal.starts_with(&format!("{own}/"));
            let above = recursive && (literal.is_empty() || own.starts_with(&format!("{literal}/")));
            if inside || above {
                bail!(
                    "tmpfiles: {trimmed:?} {} {own}, which archix keeps itself; nothing was changed",
                    if inside { "touches" } else { "would reach into" }
                );
            }
        }
        if matches!(base, 'r' | 'R' | 'D')
            && (literal == "/home" || literal.starts_with("/home/") || literal.is_empty())
        {
            warnings.push(format!("tmpfiles: {trimmed:?} deletes files under /home at every boot"));
        }
    }
    Ok(warnings)
}

/// The enforced and initial hashes of the users that declare them.
#[derive(Debug, Default, Clone)]
pub struct Passwords {
    /// Set at every switch unless the account is locked or expired, or the rescue entry set its password since.
    pub enforced: BTreeMap<String, String>,
    /// Set only when the account is created.
    pub initial: BTreeMap<String, String>,
    /// Enforced users whose password the rescue entry set while their declared hash stayed the same, left as they are.
    pub overridden: BTreeSet<String>,
    /// Users whose rescue password gives way, as their declared hash changed since; a committed switch forgets those overrides.
    pub superseded: BTreeSet<String>,
    /// Users whose password a crypt_hash secret declares, set at each boot of the deployment, by secret.
    pub secret: BTreeMap<String, String>,
}

/// The hashes `users` declare, minus those a password set in the rescue entry overrides; `state` is archix's state directory.
pub fn resolve(users: &BTreeMap<String, User>, state: &Path) -> Passwords {
    let mut out = Passwords::default();
    for (name, user) in users {
        if let Some(hash) = &user.initial_hashed_password {
            out.initial.insert(name.clone(), hash.clone());
        }
        let Some(hash) = &user.hashed_password else { continue };
        match fs::read_to_string(override_path(state, name)) {
            Ok(recorded) if recorded.trim() == digest(Some(hash)) => {
                out.overridden.insert(name.clone());
            }
            Ok(_) => {
                out.superseded.insert(name.clone());
                out.enforced.insert(name.clone(), hash.clone());
            }
            Err(_) => {
                out.enforced.insert(name.clone(), hash.clone());
            }
        }
    }
    out
}

fn override_path(state: &Path, user: &str) -> std::path::PathBuf {
    state.join("accounts/password-override").join(user)
}

fn digest(hash: Option<&str>) -> String {
    hash.map_or_else(
        || "none".to_string(),
        |hash| crate::fetch::sha256_bytes(hash.as_bytes()),
    )
}

/// Records that the rescue entry set `user`'s password while the current generation declared `declared` for it, so switches keep that password until the declared hash changes.
pub fn record_override(state: &Path, user: &str, declared: Option<&str>) -> Result<()> {
    let path = override_path(state, user);
    let dir = path.parent().expect("a file in a directory");
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    crate::durable::write_secret(&path, format!("{}\n", digest(declared)).as_bytes())
}

/// What the rescue entry records for a password a crypt_hash secret declares: the value's HMAC when a switch decrypted it, and its ciphertext's sha256.
pub fn secret_mark(hmac: Option<&str>, sha256: &str) -> String {
    format!("secret {} {sha256}", hmac.unwrap_or("-"))
}

/// Records that the rescue entry set `user`'s password while a crypt_hash secret declared it, as `secret_mark` gives.
pub fn record_secret_override(state: &Path, user: &str, mark: &str) -> Result<()> {
    let path = override_path(state, user);
    let dir = path.parent().expect("a file in a directory");
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    crate::durable::write_secret(&path, format!("{mark}\n").as_bytes())
}

/// Whether the rescue entry set `user`'s password and no switch has replaced it since; a boot then leaves it.
pub fn overridden(state: &Path, user: &str) -> bool {
    override_path(state, user).exists()
}

/// Sorts the users whose password a crypt_hash secret declares into those whose rescue password holds, as their secret's value (or, undecrypted, its ciphertext) stayed the same, and those whose changed; `secrets` is the new deployment's manifest.
pub fn resolve_secrets(passwords: &mut Passwords, secrets: Option<&crate::secrets::Manifest>, state: &Path) {
    let Some(manifest) = secrets else { return };
    for (user, name) in &manifest.hashes {
        passwords.secret.insert(user.clone(), name.clone());
        let Ok(recorded) = fs::read_to_string(override_path(state, user)) else {
            continue;
        };
        let Some(entry) = manifest.secrets.get(name) else {
            continue;
        };
        let words: Vec<&str> = recorded.split_whitespace().collect();
        let holds = match words[..] {
            ["secret", hmac, sha] => (hmac != "-" && entry.hmac.as_deref() == Some(hmac)) || sha == entry.sha256,
            _ => false,
        };
        match holds {
            true => passwords.overridden.insert(user.clone()),
            false => passwords.superseded.insert(user.clone()),
        };
    }
}

/// Forgets the rescue overrides a committed switch replaced with the declared hashes.
pub fn forget_overrides(state: &Path, passwords: &Passwords) {
    for name in &passwords.superseded {
        let _ = fs::remove_file(override_path(state, name));
    }
}

/// What a switch says about passwords the rescue entry set: kept over an unchanged declared hash, or replaced by a changed one.
pub fn override_lines(passwords: &Passwords) -> Vec<String> {
    let kept = passwords.overridden.iter().map(|name| match passwords.secret.get(name) {
        Some(secret) => format!(
            "keeping {name}'s password set in the rescue entry over the crypt_hash secret {secret}, until that secret changes"
        ),
        None => format!(
            "keeping {name}'s password set in the rescue entry over users.{name}.hashed_password, until that hash changes"
        ),
    });
    let replaced = passwords.superseded.iter().map(|name| match passwords.secret.get(name) {
        Some(secret) => format!(
            "secret {secret} changed since the rescue entry set {name}'s password, so the new deployment's boot sets the declared one"
        ),
        None => format!(
            "users.{name}.hashed_password changed since the rescue entry set {name}'s password, so the declared one replaces it"
        ),
    });
    kept.chain(replaced).collect()
}

fn shadow_entries(etc: &Path) -> Option<BTreeMap<String, Vec<String>>> {
    let text = fs::read_to_string(etc.join("shadow")).ok()?;
    Some(
        text.lines()
            .filter(|line| line.contains(':'))
            .map(|line| {
                let fields: Vec<String> = line.split(':').map(str::to_string).collect();
                (fields[0].clone(), fields)
            })
            .collect(),
    )
}

fn is_lock(hash: &str) -> bool {
    hash.starts_with('!') || hash.starts_with('*')
}

/// Why a declared hash isn't applied to `name`, from the entries in the /etc of each of `etcs`: the account is locked or expired there.
fn kept_lock(name: &str, hash: &str, etcs: &[&Path]) -> Option<String> {
    if is_lock(hash) {
        return None;
    }
    etcs.iter()
        .filter_map(|etc| fs::read_to_string(etc.join("shadow")).ok())
        .any(|shadow| !accounts::may_set_hash(&shadow, name, hash))
        .then(|| {
            format!(
                "{name}'s account is locked or expired, so archix keeps it that way and doesn't apply the declared password; `usermod -U {name}` or `chage -E -1 {name}` lets the next switch apply it"
            )
        })
}

/// One line per password a switch would set, without the hash; the /etc at `etc` holds the accounts as they are.
pub fn describe(passwords: &Passwords, etc: &Path) -> Vec<String> {
    let mut lines: Vec<String> = passwords
        .secret
        .iter()
        .filter(|(name, _)| !passwords.overridden.contains(*name))
        .map(|(name, secret)| {
            format!("password   {name}'s comes from secret {secret}, set when the new deployment boots")
        })
        .collect();
    if passwords.enforced.is_empty() && passwords.initial.is_empty() {
        return lines;
    }
    let Some(entries) = shadow_entries(etc) else {
        lines.push("password   not compared: reading /etc/shadow needs root (`sudo archix diff`)".to_string());
        return lines;
    };
    for (name, hash) in &passwords.enforced {
        match entries.get(name) {
            None => lines.push(format!("password   {name} gets the declared password")),
            Some(_) if kept_lock(name, hash, &[etc]).is_some() => {}
            Some(fields) if fields.get(1) != Some(hash) => {
                lines.push(format!("password   resetting {name}'s password to the declared one"))
            }
            Some(_) => {}
        }
    }
    for name in passwords.initial.keys() {
        if !entries.contains_key(name) {
            lines.push(format!("password   {name} gets its initial password"));
        }
    }
    lines
}

/// Warnings for declared passwords a switch leaves alone because the account is locked or expired in one of `etcs`.
pub fn kept_locks(passwords: &Passwords, etcs: &[&Path]) -> Vec<String> {
    passwords
        .enforced
        .iter()
        .filter_map(|(name, hash)| kept_lock(name, hash, etcs))
        .collect()
}

/// Sets `name`'s declared `hash` in the /etc at `etc`, a staging root's at a switch or the booted one's for a crypt_hash secret, unless the account is locked or expired there or in `also`; returns why it was kept.
pub fn enforce(etc: &Path, also: &[&Path], name: &str, hash: &str) -> Result<Option<String>> {
    let mut etcs = vec![etc];
    etcs.extend_from_slice(also);
    if let Some(why) = kept_lock(name, hash, &etcs) {
        return Ok(Some(why));
    }
    let entries = shadow_entries(etc).unwrap_or_default();
    if entries.get(name).and_then(|fields| fields.get(1)).map(String::as_str) != Some(hash) {
        super::password::set(etc, name, hash).with_context(|| format!("setting {name}'s declared password"))?;
    }
    Ok(None)
}

/// Sets the declared hashes in a staging root's /etc: enforced ones unless the account is locked or expired there or in `live`, initial ones for the accounts `existed` says this build created; returns why any was kept.
pub fn apply(etc: &Path, live: &Path, passwords: &Passwords, existed: &dyn Fn(&str) -> bool) -> Result<Vec<String>> {
    let mut kept = Vec::new();
    for (name, hash) in &passwords.enforced {
        kept.extend(enforce(etc, &[live], name, hash)?);
    }
    for (name, hash) in &passwords.initial {
        if !existed(name) {
            super::password::set(etc, name, hash).with_context(|| format!("setting {name}'s initial password"))?;
        }
    }
    Ok(kept)
}

/// Whether `user` in the /etc at `etc` is root or a wheel or sudo member whose account isn't locked or expired, so a password set there at boot would open the rescue entry.
fn could_open_rescue(etc: &Path, user: &str) -> bool {
    let passwd = fs::read_to_string(etc.join("passwd")).unwrap_or_default();
    let group = fs::read_to_string(etc.join("group")).unwrap_or_default();
    let Some(fields) = passwd
        .lines()
        .map(|line| line.split(':').collect::<Vec<_>>())
        .find(|fields| fields.first() == Some(&user))
    else {
        return false;
    };
    let gid = fields.get(3).copied().unwrap_or_default();
    let member = group.lines().any(|line| {
        let g: Vec<&str> = line.split(':').collect();
        g.len() >= 4
            && super::rescue_auth::GROUPS.contains(&g[0])
            && (g[2] == gid || g[3].split(',').any(|m| m == user))
    });
    let shadow = fs::read_to_string(etc.join("shadow")).unwrap_or_default();
    (fields.get(2) == Some(&"0") || member) && accounts::may_set_hash(&shadow, user, "$y$")
}

/// Refuses a deployment whose /etc at `etc` leaves no account the rescue entry and a console login can use (root, or a wheel or sudo member with a password) when the running system at `live` has one; with none on either, a warning. `logins` are users whose crypt_hash secret the switch decrypted and checked, which the deployment's boot sets; no other secret counts.
pub fn lockout(etc: &Path, live: &Path, allowed: bool, logins: &BTreeSet<String>) -> Result<Option<String>> {
    let usable = |etc: &Path| super::rescue_auth::accounts(etc).is_ok_and(|found| !found.is_empty());
    if allowed || usable(etc) || logins.iter().any(|user| could_open_rescue(etc, user)) {
        return Ok(None);
    }
    let what = "root is locked and no wheel or sudo member has a yescrypt or SHA-512 password";
    if usable(live) {
        bail!(
            "this config leaves no account that can log in with a password: {what} in the new deployment, though the running system has one. The rescue boot entry needs one, else recovering needs a live USB. Give root or a wheel user a password (users.<name>.hashed_password, or passwd), or set allow_no_password_login = true; nothing was changed"
        );
    }
    Ok(Some(format!(
        "warning: no account can log in with a password ({what}), here or in the new deployment, and the rescue boot entry needs one; give root or a wheel user a password with passwd or users.<name>.hashed_password, or set allow_no_password_login = true to silence this"
    )))
}

/// A warning when a hash sits in the config itself and the config is a git repository with a remote, where whoever reads the remote can crack it offline.
pub fn remote_warning(users: &BTreeMap<String, User>, config_dir: &Path) -> Option<String> {
    let literal: Vec<String> = users
        .iter()
        .filter(|(_, user)| {
            [&user.hashed_password, &user.initial_hashed_password]
                .into_iter()
                .flatten()
                .any(|hash| !is_lock(hash))
        })
        .map(|(name, _)| name.clone())
        .collect();
    if literal.is_empty() {
        return None;
    }
    let remotes = Command::new("git")
        .arg("-C")
        .arg(config_dir)
        .arg("remote")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default();
    if remotes.is_empty() {
        return None;
    }
    Some(format!(
        "warning: the config holds password hashes for {}, and {} pushes to a git remote ({}); anyone who reads it can try to crack them offline. Use a long passphrase, or keep the config's repository private",
        literal.join(", "),
        config_dir.display(),
        remotes.replace('\n', ", ")
    ))
}

/// The declared users whose password is set by the config, whose new account needs no `passwd`.
pub fn with_password(users: &BTreeMap<String, User>) -> BTreeSet<String> {
    users
        .iter()
        .filter(|(_, user)| user.hashed_password.is_some() || user.initial_hashed_password.is_some())
        .map(|(name, _)| name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const YESCRYPT: &str = "$y$j9T$u2f4nll1JxfH3hjxKg.8P1$iQmwHBe.iDFgnhAJVdpekC.aTQ8uIWtwoFNGILtAHV9";
    const SHA512: &str =
        "$6$saltstring$svn8UoSVapNtMuq1ukKS4tPQd8iKwSMHWjl/O817G3uBnIFNjnQJuesI68u4OTLiBFdcbYEdFCoEOfaS35inz1";
    const ED25519: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl me@laptop";

    #[test]
    fn crypt_hashes_and_locks_pass_and_plaintext_or_weak_or_torn_ones_are_refused() {
        for good in [YESCRYPT, SHA512, "!", "*", "!*", &format!("!{SHA512}")] {
            check_hash("h", good).unwrap_or_else(|e| panic!("{good}: {e}"));
        }
        check_hash("h", "$2b$12$R9h/cIPz0gi.URNNX3kh2OPST9/PgBkqquzi.Ss7KIUgO2t0jWMUW").unwrap();
        for (bad, says) in [
            ("hunter2", "plaintext"),
            ("", "empty"),
            ("$1$salt$qJH7.N4xYta3aEG/dfqo/0", "old hash type"),
            (&SHA512[..40], "isn't a whole one"),
            ("$y$j9T$salt$short", "isn't a whole one"),
            ("$6$a:b$c", "colon"),
            ("$zz$a$b", "doesn't know"),
        ] {
            let error = check_hash("users.alice.hashed_password", bad).unwrap_err().to_string();
            assert!(error.contains(says), "{bad:?}: {error}");
            assert!(bad.len() < 4 || !error.contains(bad), "the hash isn't printed: {error}");
        }
    }

    #[test]
    fn key_lines_need_a_known_type_and_its_key_and_may_carry_options() {
        check_key("k", ED25519).unwrap();
        check_key("k", &format!("from=\"10.0.0.0/8\",no-pty {ED25519}")).unwrap();
        for (bad, says) in [
            (
                "AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl",
                "no key type",
            ),
            (
                "ssh-rsa AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl",
                "isn't a ssh-rsa key",
            ),
            ("ssh-ed25519", "isn't a ssh-ed25519 key"),
            ("ssh-dss AAAAB3NzaC1kc3M=", "ssh-dss"),
            (&format!("{ED25519}\nssh-ed25519 x"), "one line"),
        ] {
            let error = check_key("users.alice.ssh_authorized_keys[1]", bad)
                .unwrap_err()
                .to_string();
            assert!(error.contains(says), "{bad:?}: {error}");
        }
    }

    #[test]
    fn tmpfiles_lines_stay_out_of_archixs_paths_and_removals_under_home_warn() {
        let own: Vec<String> = ["/archix", "/var/lib/archix", "/efi", "/var/usrlocal"]
            .map(String::from)
            .to_vec();
        let lines = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let warned = check_tmpfiles(
            &lines(&[
                "# comment",
                "d /var/lib/foo 0750 foo foo -",
                "d /var/lib 0755 root root -",
                "L+ /etc/foo - - - - /usr/share/foo",
                "R /home/*/.cache/thumbnails",
                "x /var/lib/archix",
            ]),
            &own,
        )
        .unwrap();
        assert_eq!(warned.len(), 1, "{warned:?}");
        assert!(warned[0].contains("/home"), "{warned:?}");
        for (bad, says) in [
            ("d /var/lib/archix/x 0700 root root -", "touches /var/lib/archix"),
            ("R /var/lib", "would reach into /var/lib/archix"),
            ("d /var 0755 root root 10d", "would reach into /var/lib/archix"),
            ("Z / - root root", "would reach into /archix"),
            ("r /efi/loader/loader.conf", "touches /efi"),
            ("y /x", "isn't a tmpfiles.d type"),
            ("d", "has no path"),
            ("d relative", "absolute"),
        ] {
            let error = check_tmpfiles(&lines(&[bad]), &own).unwrap_err().to_string();
            assert!(error.contains(says), "{bad:?}: {error}");
        }
    }

    #[test]
    fn sudoers_has_wheels_rule_and_the_extra_lines() {
        let text = sudoers(true, false, &["alice ALL=(ALL) NOPASSWD: /usr/bin/systemctl".into()]).unwrap();
        assert!(
            text.contains("%wheel ALL=(ALL:ALL) NOPASSWD: ALL\nalice ALL=(ALL) NOPASSWD: /usr/bin/systemctl\n"),
            "{text}"
        );
        assert!(sudoers(true, true, &[]).unwrap().contains("%wheel ALL=(ALL:ALL) ALL\n"));
        assert!(!sudoers(false, true, &[]).unwrap().contains("%wheel"));
        assert!(sudoers(true, true, &["a\nb".into()]).is_err());
    }

    fn etc(shadow: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("passwd"),
            "root:x:0:0::/root:/bin/bash\nalice:x:1000:1000::/home/alice:/bin/bash\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("group"),
            "root:x:0:\nwheel:x:998:alice\nalice:x:1000:\n",
        )
        .unwrap();
        fs::write(dir.path().join("shadow"), shadow).unwrap();
        dir
    }

    fn enforced(name: &str, hash: &str) -> Passwords {
        Passwords {
            enforced: [(name.to_string(), hash.to_string())].into(),
            ..Passwords::default()
        }
    }

    #[test]
    fn an_enforced_hash_resets_a_changed_password_but_never_undoes_a_lock_or_an_expiry() {
        an_enforced_hash_follows_the_lock_rule(&|etc: &Path, live: &Path, hash: &str| {
            apply(etc, live, &enforced("alice", hash), &|_| true).unwrap()
        });
    }

    #[test]
    fn a_crypt_hash_secret_set_at_boot_follows_the_same_lock_rule() {
        an_enforced_hash_follows_the_lock_rule(&|etc: &Path, live: &Path, hash: &str| {
            enforce(etc, &[live], "alice", hash).unwrap().into_iter().collect()
        });
    }

    /// The switch's apply and the boot's enforce for a secret: `set` sets alice's declared `hash` in the first /etc, the second the running one, returning why it was kept.
    fn an_enforced_hash_follows_the_lock_rule(set: &dyn Fn(&Path, &Path, &str) -> Vec<String>) {
        let other = SHA512.replace("saltstring", "othersalt");
        let dir = etc(&format!("root:*:1::::::\nalice:{other}:1:0:99999:7:::\n"));
        assert!(set(dir.path(), dir.path(), YESCRYPT).is_empty());
        assert!(
            fs::read_to_string(dir.path().join("shadow"))
                .unwrap()
                .contains(&format!("alice:{YESCRYPT}:"))
        );
        for locked in [
            format!("alice:!{other}:1:0:99999:7:::\n"),
            format!("alice:{other}:1:0:99999:7::1:\n"),
        ] {
            let dir = etc(&format!("root:*:1::::::\n{locked}"));
            let live = etc("");
            assert_eq!(set(dir.path(), live.path(), YESCRYPT).len(), 1, "{locked}");
            assert!(
                fs::read_to_string(dir.path().join("shadow"))
                    .unwrap()
                    .ends_with(&locked),
                "{locked}"
            );
        }
        let live_locked = etc(&format!("alice:!{other}:1:0:99999:7:::\n"));
        let staged = etc(&format!("alice:{other}:1:0:99999:7:::\n"));
        set(staged.path(), live_locked.path(), YESCRYPT);
        assert!(
            !fs::read_to_string(staged.path().join("shadow"))
                .unwrap()
                .contains(YESCRYPT),
            "a lock in the other /etc counts too"
        );
        let fresh = etc("alice:!:1:0:99999:7:::\n");
        set(fresh.path(), fresh.path(), YESCRYPT);
        assert!(
            fs::read_to_string(fresh.path().join("shadow"))
                .unwrap()
                .contains(YESCRYPT),
            "a never-set password isn't a lock"
        );
        let declared_lock = etc(&format!("alice:!{other}:1:0:99999:7:::\n"));
        set(declared_lock.path(), declared_lock.path(), "!");
        assert!(
            fs::read_to_string(declared_lock.path().join("shadow"))
                .unwrap()
                .contains("alice:!:")
        );
    }

    #[test]
    fn describing_an_enforced_hash_never_shows_it_and_a_kept_lock_is_no_change() {
        let other = SHA512.replace("saltstring", "othersalt");
        let dir = etc(&format!("root:*:1::::::\nalice:{other}:1:0:99999:7:::\n"));
        let lines = describe(&enforced("alice", YESCRYPT), dir.path());
        assert_eq!(lines, ["password   resetting alice's password to the declared one"]);
        assert!(!lines.concat().contains('$'), "no hash is printed");
        apply(dir.path(), dir.path(), &enforced("alice", YESCRYPT), &|_| true).unwrap();
        assert!(
            fs::read_to_string(dir.path().join("shadow"))
                .unwrap()
                .contains(&format!("alice:{YESCRYPT}:"))
        );
        assert!(describe(&enforced("alice", YESCRYPT), dir.path()).is_empty());

        for locked in [
            format!("alice:!{other}:1:0:99999:7:::\n"),
            format!("alice:{other}:1:0:99999:7::1:\n"),
        ] {
            let dir = etc(&format!("root:*:1::::::\n{locked}"));
            let live = etc("");
            let kept = apply(dir.path(), live.path(), &enforced("alice", YESCRYPT), &|_| true).unwrap();
            assert_eq!(kept.len(), 1, "{locked}");
            assert!(
                fs::read_to_string(dir.path().join("shadow"))
                    .unwrap()
                    .ends_with(&locked),
                "{locked}"
            );
            assert!(
                describe(&enforced("alice", YESCRYPT), dir.path()).is_empty(),
                "a kept lock isn't a change"
            );
            assert_eq!(kept_locks(&enforced("alice", YESCRYPT), &[dir.path()]).len(), 1);
        }
        let live_locked = etc(&format!("alice:!{other}:1:0:99999:7:::\n"));
        let staged = etc(&format!("alice:{other}:1:0:99999:7:::\n"));
        apply(staged.path(), live_locked.path(), &enforced("alice", YESCRYPT), &|_| {
            true
        })
        .unwrap();
        assert!(
            !fs::read_to_string(staged.path().join("shadow"))
                .unwrap()
                .contains(YESCRYPT),
            "a lock made live counts too"
        );
        let fresh = etc("alice:!:1:0:99999:7:::\n");
        apply(fresh.path(), fresh.path(), &enforced("alice", YESCRYPT), &|_| true).unwrap();
        assert!(
            fs::read_to_string(fresh.path().join("shadow"))
                .unwrap()
                .contains(YESCRYPT),
            "a never-set password isn't a lock"
        );
        let declared_lock = etc(&format!("alice:!{other}:1:0:99999:7:::\n"));
        apply(
            declared_lock.path(),
            declared_lock.path(),
            &enforced("alice", "!"),
            &|_| true,
        )
        .unwrap();
        assert!(
            fs::read_to_string(declared_lock.path().join("shadow"))
                .unwrap()
                .contains("alice:!:")
        );
    }

    #[test]
    fn an_initial_hash_is_set_only_on_the_account_the_build_created() {
        let initial = Passwords {
            initial: [("alice".to_string(), YESCRYPT.to_string())].into(),
            ..Passwords::default()
        };
        let dir = etc("root:*:1::::::\nalice:$6$x$y:1:0:99999:7:::\n");
        apply(dir.path(), dir.path(), &initial, &|_| true).unwrap();
        assert!(
            !fs::read_to_string(dir.path().join("shadow"))
                .unwrap()
                .contains(YESCRYPT)
        );
        assert!(describe(&initial, dir.path()).is_empty());
        apply(dir.path(), dir.path(), &initial, &|_| false).unwrap();
        assert!(
            fs::read_to_string(dir.path().join("shadow"))
                .unwrap()
                .contains(YESCRYPT)
        );
    }

    #[test]
    fn a_deployment_nobody_can_log_into_is_refused_only_when_the_running_system_has_a_login() {
        let none = etc("root:*:1::::::\nalice:!:1:0:99999:7:::\n");
        let some = etc(&format!("root:*:1::::::\nalice:{YESCRYPT}:1:0:99999:7:::\n"));
        let no = BTreeSet::new();
        let error = lockout(none.path(), some.path(), false, &no).unwrap_err().to_string();
        assert!(error.contains("allow_no_password_login"), "{error}");
        assert!(lockout(none.path(), some.path(), true, &no).unwrap().is_none());
        assert!(
            lockout(none.path(), none.path(), false, &no)
                .unwrap()
                .is_some_and(|w| w.starts_with("warning"))
        );
        assert!(lockout(some.path(), none.path(), false, &no).unwrap().is_none());
        let alice: BTreeSet<String> = ["alice".to_string()].into();
        assert!(
            lockout(none.path(), some.path(), false, &alice).unwrap().is_none(),
            "a checked crypt_hash secret for a wheel member counts"
        );
        let bob: BTreeSet<String> = ["bob".to_string()].into();
        assert!(
            lockout(none.path(), some.path(), false, &bob).is_err(),
            "one for a user outside wheel doesn't"
        );
        let locked = etc(&format!("root:*:1::::::\nalice:!{YESCRYPT}:1:0:99999:7:::\n"));
        assert!(
            lockout(locked.path(), some.path(), false, &alice).is_err(),
            "nor one the boot won't set, as the account is locked"
        );
    }

    #[test]
    fn a_hash_in_a_config_with_a_git_remote_is_warned_about() {
        let dir = tempfile::tempdir().unwrap();
        let mut user = User {
            uid: None,
            group: None,
            groups: Vec::new(),
            shell: "/bin/bash".into(),
            home: None,
            description: String::new(),
            system: false,
            hashed_password: Some(YESCRYPT.into()),
            initial_hashed_password: None,
            hashed_password_secret: None,
        };
        let users = |user: &User| [("alice".to_string(), user.clone())].into();
        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(dir.path())
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            )
        };
        git(&["init", "-q"]);
        assert!(remote_warning(&users(&user), dir.path()).is_none(), "no remote");
        git(&["remote", "add", "origin", "https://example.com/me/config.git"]);
        let warning = remote_warning(&users(&user), dir.path()).unwrap();
        assert!(
            warning.contains("alice") && warning.contains("origin") && !warning.contains("$y$"),
            "{warning}"
        );
        user.hashed_password = Some("!".into());
        assert!(
            remote_warning(&users(&user), dir.path()).is_none(),
            "a lock is no secret"
        );
    }

    #[test]
    fn a_rescue_password_holds_until_the_declared_hash_changes() {
        let state = tempfile::tempdir().unwrap();
        let user = |hash: &str| User {
            uid: None,
            group: None,
            groups: Vec::new(),
            shell: "/bin/bash".into(),
            home: None,
            description: String::new(),
            system: false,
            hashed_password: Some(hash.to_string()),
            initial_hashed_password: None,
            hashed_password_secret: None,
        };
        let users = |hash: &str| [("alice".to_string(), user(hash))].into();
        assert_eq!(resolve(&users(YESCRYPT), state.path()).enforced["alice"], YESCRYPT);
        record_override(state.path(), "alice", Some(YESCRYPT)).unwrap();
        let kept = resolve(&users(YESCRYPT), state.path());
        assert!(kept.enforced.is_empty() && kept.overridden.contains("alice"));
        assert!(override_lines(&kept)[0].contains("keeping alice's password set in the rescue entry"));
        let changed = resolve(&users(SHA512), state.path());
        assert_eq!(changed.enforced["alice"], SHA512);
        assert!(override_lines(&changed)[0].contains("changed since the rescue entry"));
        forget_overrides(state.path(), &changed);
        let after = resolve(&users(SHA512), state.path());
        assert!(after.superseded.is_empty() && after.overridden.is_empty());
        record_override(state.path(), "alice", None).unwrap();
        assert!(
            resolve(&users(SHA512), state.path()).superseded.contains("alice"),
            "a hash declared after the rescue replaces it"
        );
    }

    #[test]
    fn a_rescue_password_over_a_secret_hash_holds_until_the_secrets_value_changes() {
        let state = tempfile::tempdir().unwrap();
        let entry = |sha: &str, hmac: Option<&str>| -> crate::secrets::Entry {
            serde_json::from_value(serde_json::json!({
                "file": "pw.age", "sha256": sha, "kind": "crypt_hash", "owner": "root", "group": "root", "mode": 256, "hmac": hmac
            }))
            .unwrap()
        };
        let manifest = |e: crate::secrets::Entry| crate::secrets::Manifest {
            secrets: [("pw".to_string(), e)].into(),
            hashes: [("alice".to_string(), "pw".to_string())].into(),
            ..Default::default()
        };
        let resolved = |m: &crate::secrets::Manifest| {
            let mut passwords = Passwords::default();
            resolve_secrets(&mut passwords, Some(m), state.path());
            passwords
        };
        assert!(!overridden(state.path(), "alice"));
        assert_eq!(resolved(&manifest(entry("ct1", Some("h1")))).secret["alice"], "pw");
        record_secret_override(state.path(), "alice", &secret_mark(Some("h1"), "ct1")).unwrap();
        assert!(
            overridden(state.path(), "alice"),
            "a boot sees it and keeps the rescue password"
        );
        let reencrypted = resolved(&manifest(entry("ct2", Some("h1"))));
        assert!(
            reencrypted.overridden.contains("alice"),
            "re-encrypting the same value keeps it"
        );
        assert!(override_lines(&reencrypted)[0].contains("over the crypt_hash secret pw"));
        let undecrypted = resolved(&manifest(entry("ct1", None)));
        assert!(
            undecrypted.overridden.contains("alice"),
            "an unchanged ciphertext keeps it without a decrypt"
        );
        let changed = resolved(&manifest(entry("ct3", Some("h2"))));
        assert!(changed.superseded.contains("alice") && override_lines(&changed)[0].contains("secret pw changed"));
        forget_overrides(state.path(), &changed);
        assert!(!overridden(state.path(), "alice"));
        assert!(
            describe(&changed, state.path())[0].contains("alice's comes from secret pw"),
            "{:?}",
            describe(&changed, state.path())
        );
    }
}

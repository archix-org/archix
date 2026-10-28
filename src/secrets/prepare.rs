//! What a switch checks before it commits a deployment with secrets: each one decrypts here and a password hash parses, in locked memory, and nothing it would write holds a value.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use super::memory::{MAX_TOTAL, SecretBuf};
use super::scan::Needle;
use super::{Kind, Manifest, crypto};

/// How far a switch can check its secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Decrypt each one here, as on a booted system.
    Check,
    /// In a chroot, as from a live USB: nothing is decrypted, the boot does it and names what fails.
    Chroot,
    /// `--without-secrets`: those this machine can't open are left out of the deployment.
    Without,
}

/// The values a switch decrypted, held until its commit for the last scan.
#[derive(Default)]
pub struct Prepared {
    pub values: Vec<(String, Kind, SecretBuf)>,
    /// Users whose password comes from a crypt_hash secret that decrypted here and is one the rescue entry can check.
    pub logins: BTreeSet<String>,
    pub warnings: Vec<String>,
}

impl Prepared {
    pub fn needles(&self, kinds: &[Kind]) -> Vec<Needle<'_>> {
        self.values
            .iter()
            .filter(|(_, kind, _)| kinds.contains(kind))
            .map(|(name, _, value)| Needle { name, value })
            .collect()
    }

    pub fn value(&self, name: &str) -> Option<&SecretBuf> {
        self.values
            .iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, _, value)| value)
    }
}

/// The hash key, made now if the identity predates it.
fn hash_key(state: &Path) -> Result<SecretBuf> {
    if !state.join("secrets/hmac.key").exists() {
        let mut key = [0u8; 32];
        if unsafe { libc::getrandom(key.as_mut_ptr().cast(), key.len(), 0) } != key.len() as isize {
            bail!("getrandom: {}", std::io::Error::last_os_error());
        }
        let path = state.join("secrets/hmac.key");
        crate::durable::write_secret(&path, crypto::hex(&key).as_bytes())?;
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o400))?;
    }
    crypto::hmac_key(state)
}

/// Decrypts and checks every secret in `manifest`, whose ciphertexts `ciphertext` gives by sha256, filling in their HMACs (and, without secrets, which are absent).
pub fn prepare(
    state: &Path,
    manifest: &mut Manifest,
    ciphertext: &dyn Fn(&str) -> Option<Vec<u8>>,
    mode: Mode,
) -> Result<Prepared> {
    let mut prepared = Prepared::default();
    if manifest.secrets.is_empty() {
        return Ok(prepared);
    }
    match mode {
        Mode::Chroot => {
            prepared.warnings.push(
                "can't check secrets here: this runs in a chroot, so nothing is decrypted; the deployment's boot decrypts them and names any that fail".to_string(),
            );
            return Ok(prepared);
        }
        Mode::Without => {
            let identities = crypto::machine_identities(state).unwrap_or_default();
            let mut absent = Vec::new();
            for (name, entry) in manifest.secrets.iter_mut() {
                let opens = ciphertext(&entry.sha256).is_some_and(|ct| crypto::opener(&ct, &identities).is_ok());
                entry.absent = !opens;
                if !opens {
                    absent.push(name.clone());
                }
            }
            if !absent.is_empty() {
                prepared.warnings.push(format!(
                    "--without-secrets: {} can't be decrypted on this machine, so the new deployment leaves them out and what uses them fails until a switch after `archix secrets rekey`: {}",
                    if absent.len() == 1 { "this secret" } else { "these secrets" },
                    absent.join(", ")
                ));
            }
            return Ok(prepared);
        }
        Mode::Check => {}
    }
    let identities = crypto::machine_identities(state).map_err(|e| {
        anyhow::anyhow!("{e:#}; the config declares secrets, which this machine decrypts at boot. Run `sudo archix secrets init`, put the key it prints in secrets.machine_key and `archix secrets rekey` where an admin key is; or switch with --without-secrets; nothing was changed")
    })?;
    let key = hash_key(state)?;
    let machine = identities[0].recipient.clone();
    let mut total = 0;
    for (name, entry) in manifest.secrets.iter_mut() {
        entry.absent = false;
        let Some(ct) = ciphertext(&entry.sha256) else {
            bail!("secret {name}'s ciphertext is missing from the switch; nothing was changed");
        };
        let (mut value, _) = match crypto::decrypt(&ct, &identities, &format!("secret {name}")) {
            Ok(found) => found,
            Err(e) if e.to_string().contains("no identity here matches") => bail!(
                "secret {name} isn't encrypted to this machine ({machine}); run `archix secrets rekey` where an admin key is available, or switch with --without-secrets; nothing was changed"
            ),
            Err(e) => return Err(e.context("nothing was changed")),
        };
        total += value.len();
        if total > MAX_TOTAL {
            bail!(
                "the secrets hold more than {} MiB together, the most a boot writes to its ramfs; nothing was changed",
                MAX_TOTAL >> 20
            );
        }
        if entry.kind == Kind::CryptHash {
            value.trim_end_newlines();
            let text = std::str::from_utf8(value.as_slice()).unwrap_or("\u{0}");
            crate::system::credentials::check_hash(&format!("secret {name} (kind = \"crypt_hash\")"), text)
                .map_err(|e| e.context("nothing was changed"))?;
            if crate::system::password::checkable(text) {
                prepared.logins.extend(
                    manifest
                        .hashes
                        .iter()
                        .filter(|(_, secret)| *secret == name)
                        .map(|(user, _)| user.clone()),
                );
            }
        }
        entry.hmac = Some(crypto::hmac(key.as_slice(), value.as_slice()));
        prepared.values.push((name.clone(), entry.kind, value));
    }
    Ok(prepared)
}

/// What `want` would leave of the existing files at its links, found where `place` puts each path: a regular file is replaced only when it holds exactly the value its link brings, as `archix init`'s capture leaves, and never backed up; one that took the place of a link archix wrote, or holds anything else, refuses the switch. Returns what it says.
pub fn existing(
    want: &Manifest,
    previous: Option<&Manifest>,
    prepared: &Prepared,
    place: &dyn Fn(&str) -> Option<PathBuf>,
    deployment: Option<u32>,
) -> Result<Vec<String>> {
    let had = previous.map(Manifest::links).unwrap_or_default();
    let mut lines = Vec::new();
    for (path, target) in want.links() {
        let Some(on_disk) = place(&path) else { continue };
        let Ok(meta) = fs::symlink_metadata(&on_disk) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let what = secret_of(want, &path);
        if had.get(&path).is_some_and(|before| *before == target) {
            bail!(
                "{path} should be a link to {what} but is a file{}; it holds plain text on disk. Move it away (archix keeps no copy of it) and switch again; nothing was changed",
                deployment.map_or(String::new(), |n| format!(" in deployment {n}"))
            );
        }
        let wanted = wanted_value(want, &path, prepared)?;
        let Some(wanted) = wanted.filter(|_| !prepared.values.is_empty()) else {
            if !prepared.values.is_empty() {
                bail!(
                    "{path} exists, and {what} would replace it with a link; archix won't back up a file that may hold a secret, so move it away yourself and switch again; nothing was changed"
                );
            }
            bail!(
                "{path} exists, and {what} would replace it with a link; archix can't compare them here, so it won't drop a file that may hold a secret. Move it away yourself and switch again; nothing was changed"
            );
        };
        let same = fs::read(&on_disk).is_ok_and(|mut bytes| {
            let equal = bytes == wanted.as_slice();
            bytes.iter_mut().for_each(|b| unsafe { std::ptr::write_volatile(b, 0) });
            equal
        });
        if !same {
            bail!(
                "{path} exists and holds something other than {what}, which would replace it with a link; archix won't back up a file that may hold a secret, so move it away yourself and switch again; nothing was changed"
            );
        }
        lines.push(format!(
            "replacing  {path}, which holds {what}'s value, with a link to it; the old file isn't backed up"
        ));
    }
    Ok(lines)
}

fn secret_of(manifest: &Manifest, path: &str) -> String {
    if let Some((name, _)) = manifest
        .secrets
        .iter()
        .find(|(_, e)| e.consumers.iter().any(|c| c == path))
    {
        return format!("secret {name}");
    }
    "a template of secrets".to_string()
}

/// What the file at `path` holds once its link resolves: the secret's value or the rendered template; None when the values weren't decrypted.
fn wanted_value(manifest: &Manifest, path: &str, prepared: &Prepared) -> Result<Option<SecretBuf>> {
    if let Some((name, _)) = manifest
        .secrets
        .iter()
        .find(|(_, e)| e.consumers.iter().any(|c| c == path))
    {
        return prepared
            .value(name)
            .map(|v| SecretBuf::from_slice(v.as_slice()))
            .transpose();
    }
    Ok(None)
}

/// One line per secret a switch adds, changes, re-encrypts or drops, by name and consumer, never value; `hmacs` tells a changed value from a new encryption of the same one.
pub fn describe(from: Option<&Manifest>, to: Option<&Manifest>) -> Vec<String> {
    let empty = BTreeMap::new();
    let before = from.map_or(&empty, |m| &m.secrets);
    let after = to.map_or(&empty, |m| &m.secrets);
    let mut lines = Vec::new();
    let users = |name: &str, entry: &super::Entry| match entry.consumers.is_empty() {
        true => format!(" ({})", super::link_target(name)),
        false => format!(" ({})", entry.consumers.join(", ")),
    };
    for (name, entry) in after {
        let line = match before.get(name) {
            None => "new",
            Some(old) if old.sha256 == entry.sha256 && old.absent == entry.absent => continue,
            Some(old) if old.sha256 == entry.sha256 => "decrypted again",
            Some(old) => match (&old.hmac, &entry.hmac) {
                (Some(a), Some(b)) if a == b => "re-encrypted only",
                (Some(_), Some(_)) => "changed",
                _ => "changed or re-encrypted (only a switch that decrypts it here tells which)",
            },
        };
        let absent = if entry.absent {
            ", left out: it doesn't decrypt here"
        } else {
            ""
        };
        lines.push(format!("secret     {name}: {line}{}{absent}", users(name, entry)));
    }
    for name in before.keys().filter(|name| !after.contains_key(*name)) {
        lines.push(format!("secret     {name}: removed"));
    }
    lines
}

/// The HMACs of `manifest`'s values decrypted here, for `archix diff`, which writes nothing; empty without root or an identity.
pub fn hmacs(
    state: &Path,
    manifest: &Manifest,
    ciphertext: &dyn Fn(&str) -> Option<Vec<u8>>,
) -> BTreeMap<String, String> {
    let (Ok(identities), Ok(key)) = (crypto::machine_identities(state), crypto::hmac_key(state)) else {
        return BTreeMap::new();
    };
    if super::memory::harden(false).is_err() {
        return BTreeMap::new();
    }
    let mut out = BTreeMap::new();
    for (name, entry) in &manifest.secrets {
        let Some(ct) = ciphertext(&entry.sha256) else { continue };
        if let Ok((mut value, _)) = crypto::decrypt(&ct, &identities, name) {
            if entry.kind == Kind::CryptHash {
                value.trim_end_newlines();
            }
            out.insert(name.clone(), crypto::hmac(key.as_slice(), value.as_slice()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::Entry;

    fn entry(sha: &str, hmac: Option<&str>) -> Entry {
        Entry {
            file: "s.age".into(),
            sha256: sha.into(),
            kind: Kind::File,
            owner: "root".into(),
            group: "root".into(),
            mode: 0o400,
            path: None,
            consumers: vec!["/etc/x".into()],
            hmac: hmac.map(str::to_string),
            absent: false,
        }
    }

    #[test]
    fn diff_lines_name_secrets_and_tell_a_changed_value_from_a_new_encryption() {
        let manifest = |entries: Vec<(&str, Entry)>| Manifest {
            secrets: entries.into_iter().map(|(n, e)| (n.to_string(), e)).collect(),
            ..Manifest::default()
        };
        let old = manifest(vec![
            ("a", entry("1", Some("h1"))),
            ("b", entry("2", Some("h2"))),
            ("gone", entry("3", None)),
        ]);
        let new = manifest(vec![
            ("a", entry("9", Some("h1"))),
            ("b", entry("8", Some("hx"))),
            ("c", entry("7", None)),
        ]);
        let lines = describe(Some(&old), Some(&new));
        assert_eq!(
            lines,
            [
                "secret     a: re-encrypted only (/etc/x)",
                "secret     b: changed (/etc/x)",
                "secret     c: new (/etc/x)",
                "secret     gone: removed"
            ]
        );
    }

    #[test]
    fn a_secret_hash_counts_as_a_login_only_when_this_switch_decrypted_and_parsed_it() {
        let state = tempfile::tempdir().unwrap();
        let (machine, _) = crypto::init(state.path(), false).unwrap();
        let other = age::x25519::Identity::generate().to_public().to_string();
        let hash = crate::system::password::sha512_crypt(b"pw", "saltsalt");
        let mine = crypto::encrypt(std::slice::from_ref(&machine), format!("{hash}\n").as_bytes()).unwrap();
        let theirs = crypto::encrypt(&[other], b"not-for-this-machine").unwrap();
        let shas = [crate::fetch::sha256_bytes(&mine), crate::fetch::sha256_bytes(&theirs)];
        let ciphertext = |sha: &str| {
            [&mine, &theirs]
                .into_iter()
                .find(|ct| crate::fetch::sha256_bytes(ct) == sha)
                .cloned()
        };
        let manifest = || Manifest {
            secrets: [
                (
                    "pw".to_string(),
                    Entry {
                        kind: Kind::CryptHash,
                        ..entry(&shas[0], None)
                    },
                ),
                ("other".to_string(), entry(&shas[1], None)),
            ]
            .into(),
            hashes: [("alice".to_string(), "pw".to_string())].into(),
            ..Manifest::default()
        };
        let mut checked = manifest();
        let error = prepare(state.path(), &mut checked, &ciphertext, Mode::Check)
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains("secret other isn't encrypted to this machine") && error.contains("--without-secrets"),
            "{error}"
        );
        checked.secrets.remove("other");
        let prepared = prepare(state.path(), &mut checked, &ciphertext, Mode::Check).unwrap();
        assert!(prepared.logins.contains("alice") && checked.secrets["pw"].hmac.is_some());
        assert_eq!(
            prepared.value("pw").unwrap().as_slice(),
            hash.as_bytes(),
            "the hash's line break goes"
        );
        for mode in [Mode::Without, Mode::Chroot] {
            let mut left = manifest();
            let prepared = prepare(state.path(), &mut left, &ciphertext, mode).unwrap();
            assert!(
                prepared.logins.is_empty() && prepared.values.is_empty(),
                "{mode:?} decrypts nothing and counts no login"
            );
            assert_eq!(left.secrets["other"].absent, mode == Mode::Without, "{mode:?}");
            assert!(!left.secrets["pw"].absent && left.secrets["pw"].hmac.is_none());
        }
        let mut bad = manifest();
        bad.secrets.remove("other");
        let plain = crypto::encrypt(std::slice::from_ref(&machine), b"hunter2-not-a-hash").unwrap();
        bad.secrets.get_mut("pw").unwrap().sha256 = crate::fetch::sha256_bytes(&plain);
        let error = prepare(state.path(), &mut bad, &|_| Some(plain.clone()), Mode::Check)
            .err()
            .unwrap();
        let error = format!("{error:#}");
        assert!(
            error.contains("looks like a plaintext password") && !error.contains("hunter2"),
            "{error}"
        );
    }

    #[test]
    fn an_existing_file_at_a_link_goes_only_when_it_holds_the_value_and_a_link_turned_file_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = Manifest {
            secrets: [("a".to_string(), entry("1", None))].into(),
            ..Manifest::default()
        };
        let place = |path: &str| Some(dir.path().join(path.trim_start_matches('/')));
        fs::create_dir_all(dir.path().join("etc")).unwrap();
        let mut prepared = Prepared::default();
        prepared.values.push((
            "a".into(),
            Kind::File,
            SecretBuf::from_slice(b"value-of-a-123456").unwrap(),
        ));
        fs::write(dir.path().join("etc/x"), "value-of-a-123456").unwrap();
        let said = existing(&manifest, None, &prepared, &place, Some(3)).unwrap();
        assert!(
            said[0].contains("replacing  /etc/x") && !said[0].contains("value-of"),
            "{said:?}"
        );
        fs::write(dir.path().join("etc/x"), "something else").unwrap();
        let error = existing(&manifest, None, &prepared, &place, Some(3))
            .unwrap_err()
            .to_string();
        assert!(error.contains("holds something other than secret a"), "{error}");
        let error = existing(&manifest, Some(&manifest), &prepared, &place, Some(3))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("should be a link to secret a but is a file in deployment 3"),
            "{error}"
        );
        let error = existing(&manifest, None, &Prepared::default(), &place, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("can't compare them here"), "{error}");
        fs::remove_file(dir.path().join("etc/x")).unwrap();
        std::os::unix::fs::symlink("/run/archix/secrets/a", dir.path().join("etc/x")).unwrap();
        assert!(
            existing(&manifest, Some(&manifest), &Prepared::default(), &place, None)
                .unwrap()
                .is_empty()
        );
    }
}

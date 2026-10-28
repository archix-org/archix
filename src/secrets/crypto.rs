//! age encryption and decryption for secrets: the machine's identity and the retired ones, the user's own identities, recipients, and a keyed hash for telling values apart without keeping them.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use age::secrecy::ExposeSecret;
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use super::memory::{MAX_SECRET, SecretBuf};

/// What `secrets` keeps under archix's state directory.
pub fn dir(state: &Path) -> PathBuf {
    state.join("secrets")
}

pub fn identity_path(state: &Path) -> PathBuf {
    dir(state).join("identity")
}

pub fn retired_dir(state: &Path) -> PathBuf {
    dir(state).join("identity.retired")
}

fn hmac_key_path(state: &Path) -> PathBuf {
    dir(state).join("hmac.key")
}

/// One identity that can unwrap a file key, with the recipient it answers to.
pub struct Identity {
    pub recipient: String,
    /// Where it came from, for messages.
    pub source: String,
    inner: Box<dyn age::Identity + Send + Sync>,
}

impl Identity {
    pub fn as_age(&self) -> &dyn age::Identity {
        self.inner.as_ref()
    }
}

/// A fresh machine identity as age-keygen writes one, and its recipient.
fn generate() -> (String, String) {
    let identity = age::x25519::Identity::generate();
    let recipient = identity.to_public().to_string();
    let text = format!(
        "# archix machine identity; never copy it into the config\n# public key: {recipient}\n{}\n",
        identity.to_string().expose_secret()
    );
    (text, recipient)
}

/// Writes a private file through a temporary name, root's and 0400, in a 0700 directory.
fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().context("a file in a directory")?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .with_context(|| format!("creating {}", parent.display()))?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod {}", parent.display()))?;
    let tmp = path.with_extension("tmp");
    let _ = fs::remove_file(&tmp);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o400)
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    crate::durable::sync_dir(path)
}

/// Creates this machine's identity, and the key that hashes values for change detection, unless they exist; returns its recipient and whether it was made now.
pub fn init(state: &Path, replace: bool) -> Result<(String, bool)> {
    let path = identity_path(state);
    if !replace && let Ok(existing) = machine(state) {
        return Ok((existing.recipient, false));
    }
    if replace && path.exists() {
        retire(state)?;
    }
    let (text, recipient) = generate();
    write_private(&path, text.as_bytes())?;
    if !hmac_key_path(state).exists() {
        let mut key = [0u8; 32];
        random(&mut key)?;
        write_private(&hmac_key_path(state), hex(&key).as_bytes())?;
    }
    Ok((recipient, true))
}

/// Moves the current identity to the retired ones, kept while a kept deployment needs it; returns its recipient.
pub fn retire(state: &Path) -> Result<String> {
    let current = machine(state)?;
    let target = retired_dir(state).join(&current.recipient);
    let text = fs::read(identity_path(state)).context("reading the machine identity")?;
    write_private(&target, &text)?;
    fs::remove_file(identity_path(state)).context("removing the old machine identity")?;
    Ok(current.recipient)
}

fn random(bytes: &mut [u8]) -> Result<()> {
    let got = unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), 0) };
    if got != bytes.len() as isize {
        return Err(std::io::Error::last_os_error()).context("getrandom");
    }
    Ok(())
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The x25519 identity in an identity file, as age-keygen and `init` write them.
fn read_x25519(path: &Path) -> Result<age::x25519::Identity> {
    let mut text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let found = text
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("AGE-SECRET-KEY-"))
        .map(age::x25519::Identity::from_str);
    zeroize_string(&mut text);
    match found {
        Some(Ok(identity)) => Ok(identity),
        _ => bail!("{} holds no age identity", path.display()),
    }
}

fn zeroize_string(text: &mut String) {
    let bytes = unsafe { text.as_mut_vec() };
    for byte in bytes.iter_mut() {
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
}

fn from_x25519(identity: age::x25519::Identity, source: String) -> Identity {
    Identity {
        recipient: identity.to_public().to_string(),
        source,
        inner: Box::new(identity),
    }
}

/// This machine's identity.
pub fn machine(state: &Path) -> Result<Identity> {
    let path = identity_path(state);
    if !path.exists() {
        bail!(
            "this machine has no secrets identity at {}; `sudo archix secrets init` makes one",
            path.display()
        );
    }
    Ok(from_x25519(read_x25519(&path)?, path.display().to_string()))
}

/// The retired identities, newest last.
pub fn retired(state: &Path) -> Vec<Identity> {
    let mut found: Vec<(std::time::SystemTime, Identity)> = fs::read_dir(retired_dir(state))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| !entry.file_name().to_string_lossy().ends_with(".tmp"))
        .filter_map(|entry| {
            let identity = read_x25519(&entry.path()).ok()?;
            let when = entry.metadata().and_then(|m| m.modified()).ok()?;
            Some((when, from_x25519(identity, entry.path().display().to_string())))
        })
        .collect();
    found.sort_by_key(|(when, _)| *when);
    found.into_iter().map(|(_, identity)| identity).collect()
}

/// The machine identity, then the retired ones, for decrypting what this machine boots.
pub fn machine_identities(state: &Path) -> Result<Vec<Identity>> {
    let mut all = vec![machine(state)?];
    all.extend(retired(state));
    Ok(all)
}

/// Callbacks for plugins and passphrase-protected ssh keys, asking on the terminal.
#[derive(Clone)]
pub struct Terminal;

fn tty_line(prompt: &str, echo: bool) -> Option<String> {
    use std::os::fd::AsRawFd;
    let tty = fs::OpenOptions::new().read(true).write(true).open("/dev/tty").ok()?;
    let fd = tty.as_raw_fd();
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    let have = unsafe { libc::tcgetattr(fd, &mut saved) } == 0;
    if have && !echo {
        let mut quiet = saved;
        quiet.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &quiet) };
    }
    let _ = (&tty).write_all(prompt.as_bytes());
    let mut line = String::new();
    let read = BufReader::new(&tty).read_line(&mut line);
    if have && !echo {
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) };
        let _ = (&tty).write_all(b"\n");
    }
    read.ok()?;
    let trimmed = line.trim_end_matches(['\n', '\r']).to_string();
    zeroize_string(&mut line);
    Some(trimmed)
}

impl age::Callbacks for Terminal {
    fn display_message(&self, message: &str) {
        eprintln!("{message}");
    }

    fn confirm(&self, message: &str, yes: &str, no: Option<&str>) -> Option<bool> {
        let answer = tty_line(&format!("{message} [{yes}/{}] ", no.unwrap_or("no")), true)?;
        Some(answer.eq_ignore_ascii_case(yes) || answer.eq_ignore_ascii_case("y"))
    }

    fn request_public_string(&self, description: &str) -> Option<String> {
        tty_line(&format!("{description} "), true)
    }

    fn request_passphrase(&self, description: &str) -> Option<age::secrecy::SecretString> {
        tty_line(&format!("{description} "), false).map(age::secrecy::SecretString::from)
    }
}

/// The home of the user who ran sudo, whose own keys count too when the config needs root to write.
fn sudo_home() -> Option<PathBuf> {
    let name = std::ffi::CString::new(std::env::var("SUDO_USER").ok()?).ok()?;
    let entry = unsafe { libc::getpwnam(name.as_ptr()) };
    if entry.is_null() {
        return None;
    }
    let dir = unsafe { std::ffi::CStr::from_ptr((*entry).pw_dir) };
    Some(PathBuf::from(dir.to_string_lossy().into_owned()))
}

/// The caller's own identities: $ARCHIX_AGE_IDENTITY, ~/.config/archix/identity, then ~/.ssh's ed25519 and RSA keys, in $HOME and, under sudo, the invoking user's home; age identity files may name plugins such as age-plugin-yubikey.
pub fn user_identities() -> Vec<Identity> {
    let mut homes: Vec<PathBuf> = std::env::var_os("HOME").map(PathBuf::from).into_iter().collect();
    homes.extend(sudo_home());
    let mut files: Vec<PathBuf> = Vec::new();
    if let Some(path) = std::env::var_os("ARCHIX_AGE_IDENTITY") {
        files.push(PathBuf::from(path));
    }
    files.extend(homes.iter().map(|home| home.join(".config/archix/identity")));
    let mut found = Vec::new();
    for path in files.into_iter().filter(|path| path.is_file()) {
        let Ok(file) = age::IdentityFile::from_file(path.display().to_string()) else {
            eprintln!("warning: {} isn't an age identity file; skipping it", path.display());
            continue;
        };
        let x25519 = read_x25519(&path).ok().map(|identity| identity.to_public().to_string());
        if let Ok(identities) = file.with_callbacks(Terminal).into_identities() {
            for (i, inner) in identities.into_iter().enumerate() {
                found.push(Identity {
                    recipient: if i == 0 {
                        x25519.clone().unwrap_or_default()
                    } else {
                        String::new()
                    },
                    source: path.display().to_string(),
                    inner,
                });
            }
        }
    }
    for path in homes
        .iter()
        .flat_map(|home| ["id_ed25519", "id_rsa"].map(|name| home.join(".ssh").join(name)))
    {
        let Ok(file) = fs::File::open(&path) else { continue };
        let Ok(identity) = age::ssh::Identity::from_buffer(BufReader::new(file), Some(path.display().to_string()))
        else {
            continue;
        };
        if matches!(identity, age::ssh::Identity::Unsupported(_)) {
            continue;
        }
        let recipient = fs::read_to_string(path.with_extension("pub"))
            .map(|text| text.split_whitespace().take(2).collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        found.push(Identity {
            recipient,
            source: path.display().to_string(),
            inner: Box::new(identity.with_callbacks(Terminal)),
        });
    }
    found
}

/// A recipient line as `secrets.machine_key` and `secrets.admins` take it: age1… (x25519), age1<plugin>1… (a plugin's), or an ssh-ed25519 or ssh-rsa public key.
pub enum Recipient {
    Native(Box<dyn age::Recipient + Send>),
    Plugin(age::plugin::Recipient),
}

pub fn parse_recipient(text: &str) -> Result<Recipient> {
    let text = text.trim();
    if let Ok(recipient) = age::x25519::Recipient::from_str(text) {
        return Ok(Recipient::Native(Box::new(recipient)));
    }
    if let Ok(recipient) = age::plugin::Recipient::from_str(text) {
        return Ok(Recipient::Plugin(recipient));
    }
    if let Ok(recipient) = age::ssh::Recipient::from_str(text) {
        return Ok(Recipient::Native(Box::new(recipient)));
    }
    bail!(
        "{text:?} isn't an age recipient: give an age1… key (`archix secrets pubkey` prints this machine's), an age plugin's recipient, or an ssh-ed25519 public key line"
    )
}

/// Encrypts `plaintext` to every one of `recipients`, ASCII-armored so a config's git diffs stay text.
pub fn encrypt(recipients: &[String], plaintext: &[u8]) -> Result<Vec<u8>> {
    let mut native: Vec<Box<dyn age::Recipient + Send>> = Vec::new();
    let mut plugins: BTreeMap<String, Vec<age::plugin::Recipient>> = BTreeMap::new();
    for text in recipients {
        match parse_recipient(text)? {
            Recipient::Native(recipient) => native.push(recipient),
            Recipient::Plugin(recipient) => plugins
                .entry(recipient.plugin().to_string())
                .or_default()
                .push(recipient),
        }
    }
    for (name, list) in &plugins {
        let plugin = age::plugin::RecipientPluginV1::new(name, list, &[], Terminal)
            .map_err(|e| anyhow::anyhow!("finding age-plugin-{name}, which recipients of that kind need: {e}"))?;
        native.push(Box::new(plugin));
    }
    if native.is_empty() {
        bail!("there's nobody to encrypt to");
    }
    let encryptor = age::Encryptor::with_recipients(native.iter().map(|r| r.as_ref() as &dyn age::Recipient))
        .map_err(|e| anyhow::anyhow!("encrypting: {e}"))?;
    let mut out = Vec::new();
    let armored = age::armor::ArmoredWriter::wrap_output(&mut out, age::armor::Format::AsciiArmor)?;
    let mut writer = encryptor.wrap_output(armored)?;
    writer.write_all(plaintext)?;
    writer.finish()?.finish()?;
    Ok(out)
}

/// Why an age file didn't decrypt, without its bytes.
pub fn reason(error: &age::DecryptError) -> String {
    match error {
        age::DecryptError::NoMatchingKeys => "no identity here matches its recipients".to_string(),
        age::DecryptError::InvalidHeader => "its header isn't a valid age header".to_string(),
        age::DecryptError::InvalidMac => "its header MAC doesn't match (damaged or tampered with)".to_string(),
        age::DecryptError::UnknownFormat => "it isn't an age v1 file".to_string(),
        age::DecryptError::Io(e) => format!("reading it failed: {}", e.kind()),
        other => other.to_string(),
    }
}

/// Whether `ciphertext` looks like an age file, armored or binary.
pub fn is_age(ciphertext: &[u8]) -> bool {
    ciphertext.starts_with(b"age-encryption.org/v1\n") || ciphertext.starts_with(b"-----BEGIN AGE ENCRYPTED FILE-----")
}

fn decryptor(
    ciphertext: &[u8],
) -> Result<age::Decryptor<age::armor::ArmoredReader<std::io::BufReader<&[u8]>>>, age::DecryptError> {
    age::Decryptor::new_buffered(age::armor::ArmoredReader::new(ciphertext))
}

/// The first of `identities` that unwraps `ciphertext`'s file key, without reading its payload; Err says why none did.
pub fn opener<'a>(ciphertext: &[u8], identities: &'a [Identity]) -> std::result::Result<&'a Identity, String> {
    let mut last = age::DecryptError::NoMatchingKeys;
    for identity in identities {
        match decryptor(ciphertext).and_then(|d| d.decrypt(std::iter::once(identity.as_age()))) {
            Ok(_) => return Ok(identity),
            Err(age::DecryptError::NoMatchingKeys) => {}
            Err(e) => last = e,
        }
    }
    Err(reason(&last))
}

/// Decrypts `ciphertext` with the first of `identities` that unwraps it, into locked memory; `what` names the secret in errors, which never hold its bytes.
pub fn decrypt(ciphertext: &[u8], identities: &[Identity], what: &str) -> Result<(SecretBuf, String)> {
    let opener = opener(ciphertext, identities).map_err(|why| anyhow::anyhow!("{what} doesn't decrypt here: {why}"))?;
    let reader = decryptor(ciphertext)
        .and_then(|d| d.decrypt(std::iter::once(opener.as_age())))
        .map_err(|e| anyhow::anyhow!("{what} doesn't decrypt here: {}", reason(&e)))?;
    let plain = SecretBuf::read_all(reader, MAX_SECRET, what)?;
    Ok((plain, opener.recipient.clone()))
}

/// The key `hmac` uses, made with the identity.
pub fn hmac_key(state: &Path) -> Result<SecretBuf> {
    let mut text = fs::read_to_string(hmac_key_path(state)).context("reading the secrets' hash key")?;
    let key = SecretBuf::from_slice(text.trim().as_bytes());
    zeroize_string(&mut text);
    key
}

/// HMAC-SHA256 of `message` under `key`, hex: tells two values apart without keeping either, as a plain hash of a short password could be guessed.
pub fn hmac(key: &[u8], message: &[u8]) -> String {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.map(|b| b ^ byte);
    let inner = Sha256::new().chain_update(pad(0x36)).chain_update(message).finalize();
    let outer = Sha256::new().chain_update(pad(0x5c)).chain_update(inner).finalize();
    block.iter_mut().for_each(|b| unsafe { std::ptr::write_volatile(b, 0) });
    hex(&outer)
}

/// Writes `ciphertext` at `path` in the config through a temporary file and a rename, so a cut leaves the old file.
pub fn write_ciphertext(path: &Path, ciphertext: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("age.archix-new");
    let _ = fs::remove_file(&tmp);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    file.write_all(ciphertext)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", tmp.display()))?;
    crate::testhooks::crash_at("secrets-write");
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    crate::durable::sync_dir(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_encrypted_to_a_machine_and_an_admin_decrypts_with_either_and_errors_hold_no_bytes() {
        let state = tempfile::tempdir().unwrap();
        let (machine_key, made) = init(state.path(), false).unwrap();
        assert!(made && machine_key.starts_with("age1"));
        assert_eq!(init(state.path(), false).unwrap(), (machine_key.clone(), false));
        let mode = fs::metadata(identity_path(state.path())).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o400);
        let admin = age::x25519::Identity::generate();
        let secret = b"wg-private-key-0123456789abcdef";
        let ciphertext = encrypt(&[machine_key.clone(), admin.to_public().to_string()], secret).unwrap();
        assert!(is_age(&ciphertext) && !crate::secrets::memory::contains(&ciphertext, b"wg-private"));
        let identities = machine_identities(state.path()).unwrap();
        let (plain, by) = decrypt(&ciphertext, &identities, "wg0").unwrap();
        assert_eq!((plain.as_slice(), by.as_str()), (&secret[..], machine_key.as_str()));
        let admin_only = vec![from_x25519(admin, "admin".into())];
        assert_eq!(decrypt(&ciphertext, &admin_only, "wg0").unwrap().0.as_slice(), secret);

        let old = retire(state.path()).unwrap();
        assert_eq!(old, machine_key);
        init(state.path(), false).unwrap();
        let after = machine_identities(state.path()).unwrap();
        assert_ne!(after[0].recipient, machine_key);
        assert_eq!(
            decrypt(&ciphertext, &after, "wg0").unwrap().1,
            machine_key,
            "a retired identity still opens it"
        );
        let other = encrypt(&[after[0].recipient.clone()], secret).unwrap();
        let error = decrypt(&other, &admin_only, "wg0").err().unwrap().to_string();
        assert!(
            error.contains("wg0") && error.contains("no identity here matches"),
            "{error}"
        );
        let mut torn = ciphertext.clone();
        let middle = torn.len() / 2;
        torn[middle] ^= 1;
        let error = decrypt(&torn, &identities, "wg0")
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(!error.is_empty() && !error.contains("wg-private"), "{error}");
        assert_eq!(
            hmac(b"key", b"The quick brown fox jumps over the lazy dog"),
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
    }
}

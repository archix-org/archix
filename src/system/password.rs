//! Setting a user's password in a deployment's /etc from the rescue boot, where neither PAM nor chpasswd runs: SHA-512 crypt hashes, which libxcrypt and PAM accept.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha512};

const ALPHABET: &[u8; 64] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const ROUNDS: usize = 5000;

/// Ulrich Drepper's SHA-512 crypt of `password` with `salt` (at most 16 bytes used) and the default 5000 rounds, as `$6$salt$hash`.
pub fn sha512_crypt(password: &[u8], salt: &str) -> String {
    let salt = &salt.as_bytes()[..salt.len().min(16)];
    let digest = |parts: &[&[u8]]| -> [u8; 64] {
        let mut hasher = Sha512::new();
        for part in parts {
            hasher.update(part);
        }
        hasher.finalize().into()
    };
    let b = digest(&[password, salt, password]);
    let mut a = Sha512::new();
    a.update(password);
    a.update(salt);
    for chunk in password.chunks(64) {
        a.update(&b[..chunk.len()]);
    }
    let mut n = password.len();
    while n > 0 {
        if n & 1 == 1 {
            a.update(b);
        } else {
            a.update(password);
        }
        n >>= 1;
    }
    let a: [u8; 64] = a.finalize().into();
    let repeated = |unit: &[u8], times: usize, len: usize| -> Vec<u8> {
        let mut hasher = Sha512::new();
        for _ in 0..times {
            hasher.update(unit);
        }
        let block: [u8; 64] = hasher.finalize().into();
        block.iter().cycle().take(len).copied().collect()
    };
    let p = repeated(password, password.len(), password.len());
    let s = repeated(salt, 16 + a[0] as usize, salt.len());
    let mut c = a;
    for round in 0..ROUNDS {
        let mut hasher = Sha512::new();
        if round % 2 == 1 {
            hasher.update(&p);
        } else {
            hasher.update(c);
        }
        if round % 3 != 0 {
            hasher.update(&s);
        }
        if round % 7 != 0 {
            hasher.update(&p);
        }
        if round % 2 == 1 {
            hasher.update(c);
        } else {
            hasher.update(&p);
        }
        c = hasher.finalize().into();
    }
    let mut out = format!("$6${}$", String::from_utf8_lossy(salt));
    let mut push = |b2: u8, b1: u8, b0: u8, chars: usize| {
        let mut w = (u32::from(b2) << 16) | (u32::from(b1) << 8) | u32::from(b0);
        for _ in 0..chars {
            out.push(ALPHABET[(w & 0x3f) as usize] as char);
            w >>= 6;
        }
    };
    for i in 0..21 {
        let (x, y, z) = (i, i + 21, i + 42);
        let order = [(x, y, z), (y, z, x), (z, x, y)][i % 3];
        push(c[order.0], c[order.1], c[order.2], 4);
    }
    push(0, 0, c[63], 2);
    out
}

/// A random 16-character salt from crypt's alphabet.
pub fn salt() -> Result<String> {
    let mut bytes = [0u8; 16];
    if unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), 0) } != bytes.len() as isize {
        return Err(std::io::Error::last_os_error()).context("getrandom");
    }
    Ok(bytes.iter().map(|b| ALPHABET[(b & 0x3f) as usize] as char).collect())
}

/// Days since 1970, shadow's unit for the last password change.
fn today() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() / 86_400)
}

/// shadow text with `user`'s hash replaced, its last change set to `days`, and a line added when `user` had none; other lines stay byte for byte.
pub fn with_password(shadow: &str, user: &str, hash: &str, days: u64) -> String {
    let mut found = false;
    let mut lines: Vec<String> = shadow
        .lines()
        .map(|line| {
            let mut fields: Vec<&str> = line.split(':').collect();
            if fields.first() != Some(&user) || found {
                return line.to_string();
            }
            found = true;
            fields.resize(fields.len().max(9), "");
            let days = days.to_string();
            fields[1] = hash;
            fields[2] = &days;
            fields.join(":")
        })
        .collect();
    if !found {
        lines.push(format!("{user}:{hash}:{days}:0:99999:7:::"));
    }
    lines.join("\n") + "\n"
}

/// passwd text with `user`'s password field pointing at shadow ("x"), or None when it already does; fails when `user` isn't there.
fn passwd_pointing_at_shadow(passwd: &str, user: &str) -> Result<Option<String>> {
    let mut found = None;
    let lines: Vec<String> = passwd
        .lines()
        .map(|line| {
            let mut fields: Vec<&str> = line.split(':').collect();
            if fields.first() != Some(&user) || found.is_some() {
                return line.to_string();
            }
            found = Some(fields.get(1) == Some(&"x"));
            if fields.len() > 1 {
                fields[1] = "x";
            }
            fields.join(":")
        })
        .collect();
    match found {
        None => bail!("there's no user {user} in its /etc/passwd"),
        Some(true) => Ok(None),
        Some(false) => Ok(Some(lines.join("\n") + "\n")),
    }
}

/// Sets `user`'s password hash in the /etc at `etc`, each file through a flushed temporary file and a rename; shadow keeps its mode and stays root's.
pub fn set(etc: &Path, user: &str, hash: &str) -> Result<()> {
    let passwd_path = etc.join("passwd");
    let passwd = fs::read_to_string(&passwd_path).with_context(|| format!("reading {}", passwd_path.display()))?;
    let pointed = passwd_pointing_at_shadow(&passwd, user)?;
    let shadow_path = etc.join("shadow");
    let shadow = match fs::read_to_string(&shadow_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", shadow_path.display())),
    };
    let mode = fs::metadata(&shadow_path).map_or(0o600, |meta| {
        std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o777
    });
    crate::durable::write_secret(&shadow_path, with_password(&shadow, user, hash, today()).as_bytes())?;
    if mode != 0o600 {
        fs::set_permissions(&shadow_path, std::os::unix::fs::PermissionsExt::from_mode(mode))?;
    }
    if let Some(text) = pointed {
        crate::durable::write(&passwd_path, text.as_bytes())?;
        fs::set_permissions(&passwd_path, std::os::unix::fs::PermissionsExt::from_mode(0o644))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha512_crypt_matches_the_reference_vectors() {
        assert_eq!(
            sha512_crypt(b"Hello world!", "saltstring"),
            "$6$saltstring$svn8UoSVapNtMuq1ukKS4tPQd8iKwSMHWjl/O817G3uBnIFNjnQJuesI68u4OTLiBFdcbYEdFCoEOfaS35inz1"
        );
        assert_eq!(
            sha512_crypt(b"Hello world!", "saltstringsaltstring"),
            sha512_crypt(b"Hello world!", "saltstringsaltst"),
            "only 16 bytes of salt count"
        );
        let long = vec![b'a'; 200];
        assert!(sha512_crypt(&long, "x").starts_with("$6$x$"));
        assert_eq!(sha512_crypt(&long, "x").len(), "$6$x$".len() + 86);
    }

    #[test]
    fn a_salt_is_sixteen_characters_of_crypts_alphabet() {
        let salt = salt().unwrap();
        assert_eq!(salt.len(), 16);
        assert!(salt.bytes().all(|b| ALPHABET.contains(&b)), "{salt}");
        assert_ne!(salt, super::salt().unwrap());
    }

    #[test]
    fn only_the_users_hash_and_last_change_are_replaced() {
        let shadow = "root:!*:19000::::::\nbin:!*:19000::::::\nalice:$6$old$x:19500:0:99999:7:::\n";
        let new = with_password(shadow, "alice", "$6$new$y", 20000);
        assert_eq!(
            new,
            "root:!*:19000::::::\nbin:!*:19000::::::\nalice:$6$new$y:20000:0:99999:7:::\n"
        );
        let root = with_password(shadow, "root", "$6$r$z", 20000);
        assert!(root.starts_with("root:$6$r$z:20000::::::\nbin:"), "{root}");
        assert_eq!(
            with_password("garbage\n", "root", "$6$r$z", 20000),
            "garbage\nroot:$6$r$z:20000:0:99999:7:::\n",
            "a user missing from shadow gets a line"
        );
        assert_eq!(with_password("root:x\n", "root", "h", 1), "root:h:1::::::\n");
    }

    #[test]
    fn setting_a_password_writes_shadow_and_points_passwd_at_it() {
        let dir = tempfile::tempdir().unwrap();
        let etc = dir.path();
        fs::write(
            etc.join("passwd"),
            "root:x:0:0::/root:/bin/bash\nbob:*:1000:1000::/home/bob:/bin/bash\n",
        )
        .unwrap();
        fs::write(etc.join("shadow"), "root:!:19000::::::\n").unwrap();
        fs::set_permissions(etc.join("shadow"), std::os::unix::fs::PermissionsExt::from_mode(0o400)).unwrap();
        set(etc, "bob", "$6$s$h").unwrap();
        let shadow = fs::read_to_string(etc.join("shadow")).unwrap();
        assert!(shadow.starts_with("root:!:19000::::::\nbob:$6$s$h:"), "{shadow}");
        assert_eq!(
            fs::read_to_string(etc.join("passwd")).unwrap(),
            "root:x:0:0::/root:/bin/bash\nbob:x:1000:1000::/home/bob:/bin/bash\n"
        );
        let mode = std::os::unix::fs::PermissionsExt::mode(&fs::metadata(etc.join("shadow")).unwrap().permissions());
        assert_eq!(mode & 0o777, 0o400, "shadow keeps its mode");
        assert!(set(etc, "nobody-here", "$6$s$h").is_err());
        assert!(!etc.join("shadow.tmp").exists());
    }
}

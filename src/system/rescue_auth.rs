//! The rescue boot's login: the password of root or of a wheel or sudo user from the current deployment's /etc/shadow before its prompt, and on an encrypted root the LUKS passphrase first, never a TPM, FIDO2 or PKCS#11 token.

use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::password;

/// Groups whose members' passwords open the rescue, besides root's.
pub const GROUPS: &[&str] = &["wheel", "sudo"];
/// Wrong logins before the rescue gives up and the machine restarts.
pub const ATTEMPTS: u32 = 3;
/// What to do when the rescue can't be opened.
pub const GUIDE: &str = "https://archix.dev/docs/guides/recovery/";

/// An account whose password opens the rescue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub user: String,
    pub hash: String,
}

/// Days since 1970, shadow's unit.
fn today() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| (since.as_secs() / 86_400) as i64)
}

/// Root's and every wheel or sudo member's account in the /etc at `etc` that has a password the rescue can check: not locked, not empty, not expired, and yescrypt or SHA-crypt.
pub fn accounts(etc: &Path) -> Result<Vec<Account>> {
    let read = |name: &str| {
        let path = etc.join(name);
        fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
    };
    let (passwd, shadow) = (read("passwd")?, read("shadow")?);
    let group = read("group").unwrap_or_default();
    let mut gids = Vec::new();
    let mut members = Vec::new();
    for line in group.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() >= 4 && GROUPS.contains(&fields[0]) {
            gids.push(fields[2]);
            members.extend(fields[3].split(',').filter(|m| !m.is_empty()));
        }
    }
    let mut accounts = Vec::new();
    for line in passwd.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        let [user, field, uid, gid, ..] = fields[..] else {
            continue;
        };
        if uid != "0" && !gids.contains(&gid) && !members.contains(&user) {
            continue;
        }
        let entry = shadow
            .lines()
            .map(|line| line.split(':').collect::<Vec<_>>())
            .find(|fields| fields.first() == Some(&user));
        let hash = match (field, &entry) {
            ("x", Some(fields)) => fields.get(1).copied().unwrap_or(""),
            ("x", None) => continue,
            (hash, _) => hash,
        };
        let expired = entry
            .as_ref()
            .and_then(|fields| fields.get(7))
            .and_then(|days| days.parse::<i64>().ok())
            .is_some_and(|days| days <= today());
        if !expired && password::checkable(hash) && !accounts.iter().any(|a: &Account| a.user == user) {
            accounts.push(Account {
                user: user.to_string(),
                hash: hash.to_string(),
            });
        }
    }
    Ok(accounts)
}

/// Whether `user` is one of `accounts` and `password` its password.
pub fn check(accounts: &[Account], user: &str, password: &str) -> bool {
    !password.is_empty()
        && accounts
            .iter()
            .any(|account| account.user == user && password::verify(password.as_bytes(), &account.hash) == Some(true))
}

/// What the rescue says, and does, when no account can open it.
pub fn refusal(deployment: u32) -> String {
    format!(
        "the rescue entry opens only for root or a member of wheel with a password, and deployment {deployment}'s /etc/shadow has none: root is locked and no wheel user has a password (or it's a hash type other than yescrypt or SHA-crypt). Nothing was opened; boot a deployment from the menu and set one with passwd, or recover from a live USB: {GUIDE}"
    )
}

/// The delay after the `failed`th wrong login.
pub fn delay(failed: u32) -> Duration {
    Duration::from_secs(2 * u64::from(failed))
}

/// Asks for a user and password until one of `accounts` matches, at most ATTEMPTS times with a growing delay between; `ask(prompt, secret)` reads a line, None at the end of input. Returns the user.
pub fn login(
    accounts: &[Account],
    ask: &mut dyn FnMut(&str, bool) -> Result<Option<String>>,
    wait: &mut dyn FnMut(Duration),
    say: &mut dyn FnMut(&str),
) -> Result<String> {
    for failed in 1..=ATTEMPTS {
        let Some(user) = ask("rescue login (root or a wheel user): ", false)? else {
            bail!("no login; the rescue stays closed");
        };
        let Some(password) = ask("password: ", true)? else {
            bail!("no password; the rescue stays closed");
        };
        if check(accounts, user.trim(), &password) {
            return Ok(user.trim().to_string());
        }
        say("wrong user or password");
        if failed < ATTEMPTS {
            wait(delay(failed));
        }
    }
    bail!("{ATTEMPTS} wrong logins; the rescue stays closed")
}

/// LUKS devices the kernel command line names for the root (rd.luks.name, rd.luks.uuid, cryptdevice), each with the name it opens as.
pub fn luks_on_cmdline(cmdline: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for word in cmdline.split_whitespace() {
        if let Some(value) = word.strip_prefix("rd.luks.name=")
            && let Some((uuid, name)) = value.split_once('=')
        {
            found.push((format!("UUID={uuid}"), name.to_string()));
        } else if let Some(uuid) = word.strip_prefix("rd.luks.uuid=") {
            let uuid = uuid.trim_start_matches("luks-");
            found.push((format!("UUID={uuid}"), format!("luks-{uuid}")));
        } else if let Some(value) = word.strip_prefix("cryptdevice=") {
            let mut parts = value.splitn(3, ':');
            if let (Some(device), Some(name)) = (parts.next(), parts.next()) {
                found.push((device.to_string(), name.to_string()));
            }
        }
    }
    found
}

/// The cryptsetup command that opens `device` as `name` with its passphrase only: no token plugin is loaded, so a TPM, FIDO2 or PKCS#11 token never opens it.
pub fn open_argv(device: &str, name: &str) -> Vec<String> {
    [
        "open",
        "--type",
        "luks",
        "--disable-external-tokens",
        "--tries",
        "3",
        device,
        name,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const YESCRYPT: &str = "$y$j9T$kMqcsJASkGTKWEBuc18hd1$2I2nh.vdW7uIPYraHA5FxOpLQZ5yNdM39PxvIPOsMJ7";
    const SHA512: &str = "$6$rounds=10000$cELJX0Yyual9PBLd$ZJA7Nzq5g/Da4tYSy2XqdVdPNPXO7Px4qOy6qTEqYjVh9sv6/bevNoOxtws22wCFM9eXiZL8TAV3aesSQuc5n0";
    const SHA256: &str = "$5$3V1SUXG2YYvzsvkd$T5i9K8/ndLVjD9.1KoAtIzCOtxSmilsd8eznaBV40K2";
    const PASS: &str = "correct horse battery";

    fn etc(passwd: &str, shadow: &str, group: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("passwd"), passwd).unwrap();
        fs::write(dir.path().join("shadow"), shadow).unwrap();
        fs::write(dir.path().join("group"), group).unwrap();
        dir
    }

    const PASSWD: &str = "root:x:0:0::/root:/bin/bash\nbin:x:1:1::/:/usr/bin/nologin\nalice:x:1000:1000::/home/alice:/bin/bash\nbob:x:1001:1001::/home/bob:/bin/bash\ncarol:x:1002:10::/home/carol:/bin/bash\n";
    const GROUP: &str = "root:x:0:root\nwheel:x:10:alice\nbob:x:1001:\n";

    #[test]
    fn libxcrypt_hashes_verify_against_their_password_only() {
        for hash in [YESCRYPT, SHA512, SHA256] {
            assert_eq!(password::verify(PASS.as_bytes(), hash), Some(true), "{hash}");
            assert_eq!(password::verify(b"correct horse batterz", hash), Some(false), "{hash}");
            assert_eq!(password::verify(b"", hash), Some(false), "{hash}");
        }
        let ours = password::sha512_crypt(b"s3cret", "abcdefgh");
        assert_eq!(password::verify(b"s3cret", &ours), Some(true));
        for unchecked in ["!", "*", "", "!$y$j9T$x$y", "$1$salt$hash", "$2b$10$abc"] {
            assert_eq!(password::verify(PASS.as_bytes(), unchecked), None, "{unchecked}");
            assert!(!password::checkable(unchecked), "{unchecked}");
        }
    }

    #[test]
    fn root_and_wheel_members_with_a_password_open_the_rescue() {
        let shadow = format!(
            "root:{SHA512}:20000::::::\nbin:*:20000::::::\nalice:{YESCRYPT}:20000::::::\nbob:{YESCRYPT}:20000::::::\ncarol:{SHA256}:20000::::::\n"
        );
        let dir = etc(PASSWD, &shadow, GROUP);
        let users: Vec<String> = accounts(dir.path()).unwrap().into_iter().map(|a| a.user).collect();
        assert_eq!(
            users,
            ["root", "alice", "carol"],
            "bob isn't in wheel; carol's primary group is"
        );
        let found = accounts(dir.path()).unwrap();
        assert!(check(&found, "root", PASS), "root's password");
        assert!(check(&found, "alice", PASS), "a wheel member's password");
        assert!(check(&found, "carol", PASS), "a member by primary group");
        assert!(!check(&found, "bob", PASS), "not in wheel");
        assert!(!check(&found, "alice", "wrong"), "a wrong password");
        assert!(!check(&found, "alice", ""), "no password");
        assert!(!check(&found, "nobody", PASS));
    }

    #[test]
    fn a_locked_root_and_no_wheel_password_leave_nothing_that_opens_it() {
        let shadow =
            "root:*:14871::::::\nalice:!$y$j9T$x$y:20000::::::\nbob:$y$j9T$x$y:20000::::::\ncarol::20000::::::\n";
        let dir = etc(PASSWD, shadow, GROUP);
        assert_eq!(
            accounts(dir.path()).unwrap(),
            [],
            "locked, locked with a hash, not in wheel, empty"
        );
        assert!(refusal(4).contains("deployment 4") && refusal(4).contains(GUIDE));
        let expired = format!("root:{SHA512}:20000:0:99999:7::1:\n");
        assert_eq!(
            accounts(etc(PASSWD, &expired, GROUP).path()).unwrap(),
            [],
            "an expired account"
        );
        assert!(accounts(&dir.path().join("missing")).is_err());
    }

    /// Runs `login` on canned answers, returning its result, what it waited and how many answers it read.
    fn run(accounts: &[Account], answers: &[&str]) -> (Result<String>, Vec<Duration>, usize) {
        let mut answers = answers.iter();
        let mut read = 0;
        let mut waited = Vec::new();
        let result = login(
            accounts,
            &mut |_, _| {
                read += 1;
                Ok(answers.next().map(|s| s.to_string()))
            },
            &mut |d| waited.push(d),
            &mut |_| {},
        );
        (result, waited, read)
    }

    #[test]
    fn logins_are_limited_with_a_growing_delay_and_never_fall_through() {
        let found = vec![Account {
            user: "alice".into(),
            hash: YESCRYPT.into(),
        }];
        let (ok, waited, _) = run(&found, &["alice", PASS]);
        assert_eq!(ok.unwrap(), "alice");
        assert!(waited.is_empty());
        let (ok, waited, _) = run(&found, &["root", PASS, "alice", "wrong", "alice", PASS]);
        assert_eq!(ok.unwrap(), "alice", "the third try");
        assert_eq!(waited, [delay(1), delay(2)]);
        let (refused, waited, read) = run(&found, &["alice", "a", "alice", "b", "alice", "c", "alice", PASS]);
        assert!(refused.is_err(), "three wrong logins close it");
        assert_eq!(read, 6, "no fourth try");
        assert_eq!(waited, [Duration::from_secs(2), Duration::from_secs(4)]);
        let (refused, _, _) = run(&found, &["alice"]);
        assert!(refused.is_err(), "the end of input is no password");
        let (refused, _, _) = run(&found, &["alice", "", "alice", "", "alice", ""]);
        assert!(refused.is_err(), "an empty password");
        let (refused, _, read) = run(&[], &["root", "", "alice", PASS, "x", "y"]);
        assert!(refused.is_err() && read == 6, "no account: nothing opens it");
    }

    #[test]
    fn the_root_volumes_come_from_the_command_line_and_open_by_passphrase_only() {
        assert_eq!(
            luks_on_cmdline(
                "rd.luks.name=1234-ab=cryptroot root=/dev/mapper/cryptroot rd.luks.uuid=luks-99 cryptdevice=UUID=55:root:allow-discards"
            ),
            [
                ("UUID=1234-ab".to_string(), "cryptroot".to_string()),
                ("UUID=99".to_string(), "luks-99".to_string()),
                ("UUID=55".to_string(), "root".to_string()),
            ]
        );
        assert!(luks_on_cmdline("root=UUID=x rw").is_empty());
        let argv = open_argv("/dev/vda2", "root");
        assert!(argv.contains(&"--disable-external-tokens".to_string()), "{argv:?}");
        assert!(!argv.iter().any(|a| a.starts_with("--token")), "{argv:?}");
        assert_eq!(argv[argv.len() - 2..], ["/dev/vda2", "root"]);
    }
}

//! Lua→home-manager (SCOPE 7): home.<user>.home_manager rendered to a flake the user builds and activates.

pub mod exec;
pub mod generate;
pub mod nix;
pub mod runner;
pub mod update;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::options::Diagnostic;
use crate::value::Value;

pub use crate::lock::HmLock;
pub use exec::{Cmd, Exec, Output, System};
pub use generate::{Rendered, generate};
pub use runner::Runner;

/// The key the prelude's hm.* helpers tag their tables with.
pub const HM_TAG: &str = "__archix_hm";

/// SCOPE 7's refusal for a secret on its way into home-manager.
pub const SECRET_MESSAGE: &str = "in v1 secrets go through a system file with a restrictive mode or the built-in modules, never home-manager, because /nix/store is world-readable";

/// Where generate() writes, under the user's home.
pub const DIR: &str = ".config/archix/home-manager";

/// Nix needs these for `--flake`; Arch's nix package leaves them off.
pub const NIX_CONFIG: &str = "experimental-features = nix-command flakes";

/// A plan refusal (exit 2), as opposed to an error.
#[derive(Debug)]
pub struct Refusal(pub String);

impl Refusal {
    pub fn secret(path: &str) -> Refusal {
        Refusal(format!("{path}: {SECRET_MESSAGE}"))
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refusal {}

/// Whether `text` carries a value from archix.secret(): its placeholder, in any case.
pub fn is_tainted(text: &str) -> bool {
    text.to_ascii_lowercase().contains("{{archix-secret:")
}

fn get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Map(map) => map.get(key),
        _ => None,
    }
}

/// Each user's home_manager value, for users that declare one.
pub fn users(config: &Value) -> BTreeMap<String, &Value> {
    let Some(Value::Map(home)) = get(config, "home") else {
        return BTreeMap::new();
    };
    home.iter()
        .filter_map(|(user, spec)| match get(spec, "home_manager") {
            Some(Value::Null) | None => None,
            Some(value) => Some((user.clone(), value)),
        })
        .collect()
}

/// Whether home.<user>.linger is true.
pub fn linger(config: &Value, user: &str) -> bool {
    matches!(
        get(config, "home")
            .and_then(|h| get(h, user))
            .and_then(|u| get(u, "linger")),
        Some(Value::Bool(true))
    )
}

/// The option path of a user's home_manager value.
pub fn option_path(user: &str) -> String {
    match nix::is_identifier(user) {
        true => format!("home.{user}.home_manager"),
        false => format!("home.{user:?}.home_manager"),
    }
}

/// A user has the built-in home modules (home.user) or home_manager, never both.
pub fn check_users(config: &Value) -> Vec<Diagnostic> {
    let builtin = match get(config, "home").and_then(|h| get(h, "user")) {
        Some(Value::Str(user)) => user.as_str(),
        _ => return Vec::new(),
    };
    users(config)
        .keys()
        .filter(|user| user.as_str() == builtin)
        .map(|user| {
            Diagnostic::new(
                &option_path(user),
                format!(
                    "user {user} has both the built-in home modules (home.user = {user:?}) and home-manager; a user has one of them, so drop home.user or home.{user}.home_manager"
                ),
            )
        })
        .collect()
}

/// A passwd entry: what generate() and Runner need of a user.
#[derive(Debug, Clone, PartialEq)]
pub struct Account {
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
}

pub fn account(root: &Path, user: &str) -> Result<Account> {
    let path = root.join("etc/passwd");
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    for line in text.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() >= 6 && fields[0] == user {
            return Ok(Account {
                uid: fields[2]
                    .parse()
                    .with_context(|| format!("{user}'s uid in {}", path.display()))?,
                gid: fields[3]
                    .parse()
                    .with_context(|| format!("{user}'s gid in {}", path.display()))?,
                home: PathBuf::from(fields[5]),
            });
        }
    }
    bail!("home-manager for {user}: no such user in {}", path.display())
}

pub fn euid() -> u32 {
    unsafe { libc::geteuid() }
}

/// The generated flake dir of a user whose home is `home`.
pub fn flake_dir(home: &Path, user: &str) -> PathBuf {
    home.join(DIR).join(user)
}

/// `argv` run as `user`: through runuser unless we are that uid already.
pub fn as_user(user: &str, uid: u32, self_uid: u32, argv: Vec<String>) -> Vec<String> {
    if self_uid == uid {
        return argv;
    }
    let mut out: Vec<String> = ["runuser", "-u", user, "--"].map(String::from).to_vec();
    out.extend(argv);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(json: serde_json::Value) -> Value {
        fn conv(v: serde_json::Value) -> Value {
            match v {
                serde_json::Value::Null => Value::Null,
                serde_json::Value::Bool(b) => Value::Bool(b),
                serde_json::Value::Number(n) => Value::Int(n.as_i64().unwrap()),
                serde_json::Value::String(s) => Value::Str(s),
                serde_json::Value::Array(a) => Value::List(a.into_iter().map(conv).collect()),
                serde_json::Value::Object(o) => Value::Map(o.into_iter().map(|(k, v)| (k, conv(v))).collect()),
            }
        }
        conv(json)
    }

    #[test]
    fn a_user_with_both_home_kinds_is_an_error_naming_them() {
        let both = config(serde_json::json!({
            "home": { "user": "alice", "alice": { "home_manager": { "x": 1 }, "linger": false },
                      "bob": { "home_manager": { "x": 1 }, "linger": true } }
        }));
        let diags = check_users(&both);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].path.as_deref(), Some("home.alice.home_manager"));
        assert!(diags[0].message.contains("user alice"), "{}", diags[0].message);
        assert_eq!(users(&both).keys().collect::<Vec<_>>(), ["alice", "bob"]);
        assert!(linger(&both, "bob") && !linger(&both, "alice"));
        let null = config(serde_json::json!({ "home": { "user": "alice", "alice": { "home_manager": null } } }));
        assert!(check_users(&null).is_empty() && users(&null).is_empty());
    }

    #[test]
    fn taint_survives_case_changes() {
        assert!(is_tainted(&crate::secrets::placeholder("k")));
        assert!(is_tainted(&crate::secrets::placeholder("k").to_uppercase()));
        assert!(!is_tainted("/run/archix/secrets/k"));
    }

    #[test]
    fn accounts_come_from_passwd_under_the_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("etc")).unwrap();
        std::fs::write(
            dir.path().join("etc/passwd"),
            "root:x:0:0::/root:/bin/bash\nal:x:1000:1000::/home/al:/bin/zsh\n",
        )
        .unwrap();
        assert_eq!(
            account(dir.path(), "al").unwrap(),
            Account {
                uid: 1000,
                gid: 1000,
                home: "/home/al".into()
            }
        );
        assert!(account(dir.path(), "nobody").is_err());
    }

    #[test]
    fn commands_go_through_runuser_only_for_another_uid() {
        let argv = vec!["true".to_string()];
        assert_eq!(as_user("al", 1000, 1000, argv.clone()), ["true"]);
        assert_eq!(as_user("al", 1000, 0, argv), ["runuser", "-u", "al", "--", "true"]);
    }
}

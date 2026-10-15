use std::path::Path;

use anyhow::{Result, bail};

use crate::archive::{ARCH, Archive};

/// The official repositories, the ones the Arch Linux Archive keeps dated snapshots of.
pub const OFFICIAL: &[&str] = &[
    "core",
    "extra",
    "multilib",
    "core-testing",
    "extra-testing",
    "multilib-testing",
];

/// A repository packages resolve from, in pacman.conf order.
#[derive(Debug, Clone, PartialEq)]
pub struct Repo {
    pub name: String,
    pub custom: Option<Custom>,
}

/// A third-party repository: live servers only, no dated archive.
#[derive(Debug, Clone, PartialEq)]
pub struct Custom {
    pub servers: Vec<String>,
    pub keys: Vec<String>,
    pub keyserver: String,
    pub sig_level: String,
    pub before_official: bool,
}

impl Repo {
    pub fn official(name: &str) -> Repo {
        Repo {
            name: name.to_string(),
            custom: None,
        }
    }

    /// Package URLs on this custom repo's servers, in the order pacman would try them.
    pub fn urls(&self, file: &str) -> Vec<String> {
        match &self.custom {
            Some(custom) => custom
                .servers
                .iter()
                .map(|server| format!("{}/{file}", expand(server, &self.name)))
                .collect(),
            None => Vec::new(),
        }
    }
}

pub fn is_official(name: &str) -> bool {
    OFFICIAL.contains(&name)
}

/// A server URL with pacman's `$repo` and `$arch` filled in.
pub fn expand(server: &str, repo: &str) -> String {
    server
        .replace("$repo", repo)
        .replace("$arch", ARCH)
        .trim_end_matches('/')
        .to_string()
}

/// Custom repos marked `before_official` first, then the official ones, then the remaining custom ones.
pub fn ordered(official: &[String], custom: Vec<(String, Custom)>) -> Vec<Repo> {
    let (before, after): (Vec<_>, Vec<_>) = custom.into_iter().partition(|(_, c)| c.before_official);
    let wrap = |(name, custom): (String, Custom)| Repo {
        name,
        custom: Some(custom),
    };
    before
        .into_iter()
        .map(wrap)
        .chain(official.iter().map(|name| Repo::official(name)))
        .chain(after.into_iter().map(wrap))
        .collect()
}

/// Downloads a custom repo's current database from the first server that has it.
pub fn fetch_db(archive: &Archive, repo: &Repo, dest: &Path) -> Result<()> {
    for url in repo.urls(&format!("{}.db", repo.name)) {
        match archive.fetch(&url, dest) {
            Ok(true) => return Ok(()),
            Ok(false) => continue,
            Err(e) => eprintln!("warning: {e:#}"),
        }
    }
    bail!("none of {}'s servers has {}.db", repo.name, repo.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn custom(before: bool) -> Custom {
        Custom {
            servers: vec!["https://mirror.example/$arch/$repo/".into()],
            keys: vec![],
            keyserver: String::new(),
            sig_level: String::new(),
            before_official: before,
        }
    }

    #[test]
    fn orders_like_pacman_conf() {
        let repos = ordered(
            &["core".into(), "extra".into()],
            vec![("late".into(), custom(false)), ("cachyos".into(), custom(true))],
        );
        let names: Vec<&str> = repos.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["cachyos", "core", "extra", "late"]);
        assert_eq!(
            repos[0].urls("a.pkg.tar.zst"),
            ["https://mirror.example/x86_64/cachyos/a.pkg.tar.zst"]
        );
        assert!(
            repos[1].urls("x").is_empty(),
            "official packages come from the archive, not repo servers"
        );
    }
}

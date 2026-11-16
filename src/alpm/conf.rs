use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::date::Date;

pub const SYSHOOKDIR: &str = "/usr/share/libalpm/hooks/";

const PINNABLE: &[&str] = &[
    "core",
    "extra",
    "multilib",
    "core-testing",
    "extra-testing",
    "multilib-testing",
];

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Repo {
    pub name: String,
    pub servers: Vec<String>,
    pub sig_level: Vec<String>,
    pub usage: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PacmanConf {
    pub options: Vec<(String, Option<String>)>,
    pub repos: Vec<Repo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PinMode {
    Archive,
    Live,
}

impl PacmanConf {
    pub fn parse(text: &str) -> Result<PacmanConf> {
        let mut conf = PacmanConf::default();
        let mut section: Option<String> = None;
        for (n, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                if name != "options" {
                    conf.repos.push(Repo {
                        name: name.to_string(),
                        ..Repo::default()
                    });
                }
                section = Some(name.to_string());
                continue;
            }
            let (key, value) = match line.split_once('=') {
                Some((k, v)) => (k.trim().to_string(), Some(v.trim().to_string())),
                None => (line.to_string(), None),
            };
            if key == "Include" {
                bail!("pacman-conf output line {}: Include should already be expanded", n + 1);
            }
            match section.as_deref() {
                Some("options") => conf.options.push((key, value)),
                Some(_) => {
                    let repo = conf.repos.last_mut().expect("a repo section was opened");
                    let value = value.unwrap_or_default();
                    match key.as_str() {
                        "Server" => repo.servers.push(value),
                        "SigLevel" => repo.sig_level.push(value),
                        "Usage" => repo.usage.push(value),
                        _ => {}
                    }
                }
                None => bail!("pacman-conf output line {}: {line:?} before any section", n + 1),
            }
        }
        Ok(conf)
    }

    /// Runs `pacman-conf` (read-only) on `config` or the system's pacman.conf.
    pub fn read(config: Option<&Path>) -> Result<PacmanConf> {
        let mut cmd = Command::new("pacman-conf");
        if let Some(config) = config {
            cmd.arg("-c").arg(config);
        }
        let out = cmd.output().context("running pacman-conf")?;
        if !out.status.success() {
            bail!("pacman-conf failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        PacmanConf::parse(&String::from_utf8_lossy(&out.stdout))
    }

    pub fn values(&self, key: &str) -> Vec<&str> {
        self.options
            .iter()
            .filter(|(k, _)| k == key)
            .filter_map(|(_, v)| v.as_deref())
            .collect()
    }

    pub fn value(&self, key: &str) -> Option<&str> {
        self.values(key).into_iter().last()
    }

    pub fn flag(&self, key: &str) -> bool {
        self.options.iter().any(|(k, _)| k == key)
    }

    pub fn root_dir(&self) -> PathBuf {
        PathBuf::from(self.value("RootDir").unwrap_or("/"))
    }

    pub fn db_path(&self) -> PathBuf {
        PathBuf::from(self.value("DBPath").unwrap_or("/var/lib/pacman/"))
    }

    /// The hook dirs in pacman's order: the system dir under RootDir first, then each configured one, so later ones override.
    pub fn hook_dirs(&self) -> Vec<String> {
        let root = self.root_dir();
        let mut dirs = vec![format!(
            "{}/",
            root.join(SYSHOOKDIR.trim_start_matches('/'))
                .display()
                .to_string()
                .trim_end_matches('/')
        )];
        let configured = self.values("HookDir");
        if configured.is_empty() {
            dirs.push("/etc/pacman.d/hooks/".to_string());
        }
        dirs.extend(configured.into_iter().map(str::to_string));
        dirs
    }

    pub fn pin_mode(&self) -> PinMode {
        match !self.repos.is_empty() && self.repos.iter().all(|r| PINNABLE.contains(&r.name.as_str())) {
            true => PinMode::Archive,
            false => PinMode::Live,
        }
    }

    /// This config with every repo's servers replaced by the Arch Linux Archive at `date`, expanded as pacman-conf expands $repo and $arch.
    pub fn pinned(&self, archive: &str, date: Date) -> PacmanConf {
        let mut conf = self.clone();
        let base = archive.trim_end_matches('/');
        let arch = crate::archive::ARCH;
        for repo in &mut conf.repos {
            repo.servers = vec![format!(
                "{base}/repos/{:04}/{:02}/{:02}/{}/os/{arch}",
                date.year, date.month, date.day, repo.name
            )];
        }
        conf
    }

    /// A complete pacman.conf: [options] verbatim, then every repo in order with its servers; never an Include.
    pub fn render(&self) -> String {
        let mut out = String::from("[options]\n");
        for (key, value) in &self.options {
            match value {
                Some(value) => writeln!(out, "{key} = {value}").unwrap(),
                None => writeln!(out, "{key}").unwrap(),
            }
        }
        for repo in &self.repos {
            writeln!(out, "\n[{}]", repo.name).unwrap();
            for usage in &repo.usage {
                writeln!(out, "Usage = {usage}").unwrap();
            }
            for level in &repo.sig_level {
                writeln!(out, "SigLevel = {level}").unwrap();
            }
            for server in &repo.servers {
                writeln!(out, "Server = {server}").unwrap();
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const CACHYOS: &str = "[options]
RootDir = /
DBPath = /var/lib/pacman/
CacheDir = /var/cache/pacman/pkg/
HookDir = /etc/pacman.d/hooks/
GPGDir = /etc/pacman.d/gnupg/
LogFile = /var/log/pacman.log
DownloadUser = alpm
HoldPkg = pacman
HoldPkg = glibc
Architecture = x86_64
Architecture = x86_64_v4
Color
ParallelDownloads = 10
SigLevel = PackageRequired
SigLevel = DatabaseOptional
LocalFileSigLevel = PackageOptional
[cachyos-znver4]
Usage = All
Server = https://mirror.example/cachyos/repo/x86_64_v4/cachyos-znver4
[core]
Usage = All
Server = https://mirror.example/core/os/x86_64
[extra]
Usage = All
Server = https://mirror.example/extra/os/x86_64
";

    #[test]
    fn parses_options_and_repos_in_order() {
        let conf = PacmanConf::parse(CACHYOS).unwrap();
        assert_eq!(conf.values("Architecture"), ["x86_64", "x86_64_v4"]);
        assert!(conf.flag("Color"));
        assert_eq!(conf.value("ParallelDownloads"), Some("10"));
        let names: Vec<_> = conf.repos.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["cachyos-znver4", "core", "extra"]);
        assert_eq!(conf.repos[0].servers.len(), 1);
    }

    #[test]
    fn hook_dirs_put_the_system_dir_first() {
        let conf = PacmanConf::parse(CACHYOS).unwrap();
        assert_eq!(conf.hook_dirs(), ["/usr/share/libalpm/hooks/", "/etc/pacman.d/hooks/"]);
        let extra = PacmanConf::parse("[options]\nRootDir = /mnt\nHookDir = /a/\nHookDir = /b/\n").unwrap();
        assert_eq!(extra.hook_dirs(), ["/mnt/usr/share/libalpm/hooks/", "/a/", "/b/"]);
    }

    #[test]
    fn pin_mode_is_live_once_any_repo_is_unpinnable() {
        let conf = PacmanConf::parse(CACHYOS).unwrap();
        assert_eq!(conf.pin_mode(), PinMode::Live);
        let stock = PacmanConf::parse("[options]\n[core]\nServer = x\n[extra]\nServer = y\n[multilib]\n").unwrap();
        assert_eq!(stock.pin_mode(), PinMode::Archive);
    }

    #[test]
    fn render_has_no_include_and_round_trips() {
        let conf = PacmanConf::parse(CACHYOS).unwrap();
        let text = conf.render();
        assert!(!text.contains("Include"));
        assert_eq!(PacmanConf::parse(&text).unwrap(), conf);
    }

    #[test]
    fn pinned_replaces_every_server_with_the_archive() {
        let conf = PacmanConf::parse(CACHYOS).unwrap();
        let date: Date = "2026-11-01".parse().unwrap();
        let pinned = conf.pinned("https://archive.archlinux.org/", date);
        assert_eq!(
            pinned.repos[1].servers,
            ["https://archive.archlinux.org/repos/2026/11/01/core/os/x86_64"]
        );
    }

    #[test]
    fn rejects_unexpanded_include() {
        assert!(PacmanConf::parse("[core]\nInclude = /etc/pacman.d/mirrorlist\n").is_err());
    }

    #[test]
    fn reads_a_synthetic_config_through_pacman_conf() {
        if Command::new("pacman-conf").arg("--version").output().is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mirrors = dir.path().join("mirrorlist");
        std::fs::write(&mirrors, "Server = https://m.example/$repo/os/$arch\n").unwrap();
        let conf = dir.path().join("pacman.conf");
        std::fs::write(
            &conf,
            format!(
                "[options]\nArchitecture = x86_64\nIgnorePkg = foo\n[core]\nInclude = {}\n",
                mirrors.display()
            ),
        )
        .unwrap();
        let parsed = PacmanConf::read(Some(&conf)).unwrap();
        assert_eq!(parsed.values("IgnorePkg"), ["foo"]);
        assert_eq!(parsed.repos[0].servers, ["https://m.example/core/os/x86_64"]);
    }
}

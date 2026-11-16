use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use super::conf::PacmanConf;

#[derive(Debug, Clone, Default)]
pub struct Pkg {
    pub name: String,
    pub version: String,
    pub depends: Vec<String>,
    pub provides: Vec<String>,
    pub conflicts: Vec<String>,
    pub replaces: Vec<String>,
    pub files: Vec<String>,
    pub explicit: bool,
}

pub fn pkg(name: &str, version: &str) -> Pkg {
    Pkg {
        name: name.into(),
        version: version.into(),
        explicit: true,
        ..Pkg::default()
    }
}

impl Pkg {
    pub fn deps(mut self, deps: &[&str]) -> Pkg {
        self.depends = deps.iter().map(|d| d.to_string()).collect();
        self
    }
    pub fn dep(mut self) -> Pkg {
        self.explicit = false;
        self
    }
    pub fn files(mut self, files: &[&str]) -> Pkg {
        self.files = files.iter().map(|f| f.to_string()).collect();
        self
    }
    pub fn conflicts(mut self, names: &[&str]) -> Pkg {
        self.conflicts = names.iter().map(|d| d.to_string()).collect();
        self
    }
    pub fn provides(mut self, names: &[&str]) -> Pkg {
        self.provides = names.iter().map(|d| d.to_string()).collect();
        self
    }
    pub fn replaces(mut self, names: &[&str]) -> Pkg {
        self.replaces = names.iter().map(|d| d.to_string()).collect();
        self
    }

    fn desc(&self, local: bool) -> String {
        let mut out = String::new();
        let mut field = |key: &str, values: &[String]| {
            if !values.is_empty() {
                writeln!(out, "%{key}%\n{}\n", values.join("\n")).unwrap();
            }
        };
        if !local {
            field("FILENAME", &[format!("{}-{}-x86_64.pkg.tar.zst", self.name, self.version)]);
        }
        field("NAME", std::slice::from_ref(&self.name));
        field("VERSION", std::slice::from_ref(&self.version));
        field("ARCH", &["x86_64".into()]);
        field("BUILDDATE", &["1".into()]);
        if local {
            field("INSTALLDATE", &["1".into()]);
            if !self.explicit {
                field("REASON", &["1".into()]);
            }
        } else {
            field("CSIZE", &["1".into()]);
        }
        field("SIZE", &["1".into()]);
        field("DEPENDS", &self.depends);
        field("PROVIDES", &self.provides);
        field("CONFLICTS", &self.conflicts);
        field("REPLACES", &self.replaces);
        out
    }

    fn file_list(&self) -> String {
        let mut dirs = std::collections::BTreeSet::new();
        for file in &self.files {
            let mut at = String::new();
            for part in file.split('/').collect::<Vec<_>>().split_last().map(|(_, d)| d).unwrap_or(&[]) {
                at.push_str(part);
                at.push('/');
                dirs.insert(at.clone());
            }
        }
        let mut all: Vec<String> = dirs.into_iter().collect();
        all.extend(self.files.iter().cloned());
        all.sort();
        format!("%FILES%\n{}\n\n", all.join("\n"))
    }
}

/// A synthetic root: root/ with etc and a local db, dbpath/ the plan's DBPath with sync dbs.
pub struct TestRoot {
    pub dir: tempfile::TempDir,
}

impl Default for TestRoot {
    fn default() -> Self {
        Self::new()
    }
}

impl TestRoot {
    pub fn new() -> TestRoot {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("root/var/lib/pacman/local")).unwrap();
        fs::write(dir.path().join("root/var/lib/pacman/local/ALPM_DB_VERSION"), "9\n").unwrap();
        fs::create_dir_all(dir.path().join("plan/sync")).unwrap();
        TestRoot { dir }
    }

    pub fn root(&self) -> PathBuf {
        self.dir.path().join("root")
    }

    pub fn real_dbpath(&self) -> PathBuf {
        self.root().join("var/lib/pacman")
    }

    pub fn plan_dbpath(&self) -> PathBuf {
        super::handle::plan_dbpath(&self.dir.path().join("plan"), &self.real_dbpath()).unwrap()
    }

    pub fn install(&self, pkgs: &[Pkg]) {
        for p in pkgs {
            let dir = self.real_dbpath().join(format!("local/{}-{}", p.name, p.version));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("desc"), p.desc(true)).unwrap();
            fs::write(dir.join("files"), p.file_list()).unwrap();
        }
    }

    pub fn repo(&self, name: &str, pkgs: &[Pkg]) {
        let sync = self.dir.path().join("plan/sync");
        fs::create_dir_all(&sync).unwrap();
        write_tar(&sync.join(format!("{name}.db")), pkgs, |p| vec![("desc", p.desc(false))]);
        write_tar(&sync.join(format!("{name}.files")), pkgs, |p| {
            vec![("desc", p.desc(false)), ("files", p.file_list())]
        });
    }

    pub fn conf(&self, repos: &[&str]) -> PacmanConf {
        let mut text = format!(
            "[options]\nRootDir = {}/\nDBPath = {}/\nArchitecture = x86_64\nSigLevel = Never\nHookDir = {}/etc/pacman.d/hooks/\n",
            self.root().display(),
            self.real_dbpath().display(),
            self.root().display()
        );
        for repo in repos {
            text.push_str(&format!("[{repo}]\nServer = file:///nonexistent/{repo}\n"));
        }
        PacmanConf::parse(&text).unwrap()
    }
}

fn write_tar(path: &Path, pkgs: &[Pkg], entries: impl Fn(&Pkg) -> Vec<(&'static str, String)>) {
    let mut builder = tar::Builder::new(fs::File::create(path).unwrap());
    for p in pkgs {
        for (name, body) in entries(p) {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, format!("{}-{}/{name}", p.name, p.version), body.as_bytes())
                .unwrap();
        }
    }
    builder.finish().unwrap();
}

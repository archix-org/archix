use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

use anyhow::{Context, Result};

use super::{ActionKind, Event, Journal, Meta, list, now, switches_dir};

/// Writes one switch's journal: every record fsynced before the call returns.
pub struct JournalWriter {
    journal: Journal,
    meta: Meta,
    lines: File,
}

impl JournalWriter {
    /// Makes the next numbered directory under `root`'s switches dir, with plan.json, config.tar of `config_tree` and a running meta.json.
    pub fn create(root: &Path, plan_json: &str, config_tree: &Path) -> Result<JournalWriter> {
        let base = switches_dir(root);
        fs::create_dir_all(&base).with_context(|| format!("creating {}", base.display()))?;
        let mut n = list(root)?.last().copied().unwrap_or(0) + 1;
        let dir = loop {
            let dir = base.join(n.to_string());
            match fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => break dir,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
                Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
            }
        };
        crate::durable::sync_dir(&dir)?;
        let journal = Journal { dir, n };
        write_tar(&journal.config_tar(), config_tree)?;
        crate::durable::write(&journal.plan_json(), plan_json.as_bytes())?;
        let meta = Meta::new(n, now());
        write_meta(&journal, &meta)?;
        let path = journal.journal_jsonl();
        let lines = OpenOptions::new()
            .append(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        crate::durable::sync_renamed(&path)?;
        Ok(JournalWriter { journal, meta, lines })
    }

    pub fn journal(&self) -> &Journal {
        &self.journal
    }

    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    /// Changes meta.json and rewrites it atomically.
    pub fn update_meta(&mut self, change: impl FnOnce(&mut Meta)) -> Result<()> {
        let mut meta = self.meta.clone();
        change(&mut meta);
        write_meta(&self.journal, &meta)?;
        self.meta = meta;
        Ok(())
    }

    /// Appends one event and fsyncs it.
    pub fn record(&mut self, event: &Event) -> Result<()> {
        let mut line = serde_json::to_vec(event)?;
        line.push(b'\n');
        let path = self.journal.journal_jsonl();
        self.lines
            .write_all(&line)
            .and_then(|()| self.lines.sync_data())
            .with_context(|| format!("writing {}", path.display()))
    }

    pub fn step_start(&mut self, step: &str) -> Result<()> {
        self.record(&Event::StepStart {
            t: now(),
            step: step.to_string(),
        })
    }

    pub fn step_end(&mut self, step: &str, ok: bool, detail: &str) -> Result<()> {
        self.record(&Event::StepEnd {
            t: now(),
            step: step.to_string(),
            ok,
            detail: detail.to_string(),
        })
    }

    pub fn action(&mut self, step: &str, kind: ActionKind, target: &str) -> Result<()> {
        self.record(&Event::Action {
            t: now(),
            step: step.to_string(),
            kind,
            target: target.to_string(),
        })
    }
}

fn write_meta(journal: &Journal, meta: &Meta) -> Result<()> {
    let mut text = serde_json::to_vec_pretty(meta)?;
    text.push(b'\n');
    crate::durable::write(&journal.meta_json(), &text)
}

fn write_tar(path: &Path, tree: &Path) -> Result<()> {
    let tmp = path.with_extension("tar.tmp");
    let result = (|| {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        let mut builder = tar::Builder::new(file);
        builder.follow_symlinks(false);
        builder
            .append_dir_all(".", tree)
            .with_context(|| format!("copying {} into {}", tree.display(), tmp.display()))?;
        let file = builder.into_inner()?;
        file.sync_all()?;
        crate::durable::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
        crate::durable::sync_dir(path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::super::{Outcome, read_lines};
    use super::*;

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("init.lua"), "return {}\n").unwrap();
        fs::create_dir(dir.path().join("lua")).unwrap();
        fs::write(dir.path().join("lua/a.lua"), "return 1\n").unwrap();
        std::os::unix::fs::symlink("init.lua", dir.path().join("link")).unwrap();
        dir
    }

    #[test]
    fn numbers_follow_the_largest_and_are_never_reused() {
        let root = tempfile::tempdir().unwrap();
        let t = tree();
        let a = JournalWriter::create(root.path(), "{}", t.path()).unwrap();
        assert_eq!(a.journal().n, 1);
        fs::create_dir_all(switches_dir(root.path()).join("7")).unwrap();
        let b = JournalWriter::create(root.path(), "{}", t.path()).unwrap();
        assert_eq!(b.journal().n, 8);
        fs::remove_dir_all(&a.journal().dir).unwrap();
        let c = JournalWriter::create(root.path(), "{}", t.path()).unwrap();
        assert_eq!(c.journal().n, 9, "a gap below the largest is never filled");
    }

    #[test]
    fn writes_plan_config_tar_meta_and_one_line_per_event() {
        let root = tempfile::tempdir().unwrap();
        let t = tree();
        let mut w = JournalWriter::create(root.path(), "{\"schema_version\":1}\n", t.path()).unwrap();
        let j = w.journal().clone();
        assert_eq!(fs::read_to_string(j.plan_json()).unwrap(), "{\"schema_version\":1}\n");
        let mut names: Vec<String> = tar::Archive::new(File::open(j.config_tar()).unwrap())
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert!(names.iter().any(|n| n.ends_with("init.lua")), "{names:?}");
        assert!(names.iter().any(|n| n.ends_with("lua/a.lua")), "{names:?}");
        assert!(names.iter().any(|n| n.ends_with("link")), "{names:?}");
        w.step_start("lock").unwrap();
        w.action("install", ActionKind::Install, "ripgrep").unwrap();
        w.step_end("lock", true, "").unwrap();
        let events = read_lines(&j).unwrap();
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[1], Event::Action { kind: ActionKind::Install, target, .. } if target == "ripgrep"));
        let meta: Meta = serde_json::from_str(&fs::read_to_string(j.meta_json()).unwrap()).unwrap();
        assert_eq!(meta.outcome, Outcome::Running);
        assert_eq!(meta.n, j.n);
    }

    #[test]
    fn meta_is_replaced_whole_and_a_stale_temporary_changes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let t = tree();
        let mut w = JournalWriter::create(root.path(), "{}", t.path()).unwrap();
        let j = w.journal().clone();
        fs::write(j.dir.join("meta.json.tmp"), "torn").unwrap();
        w.update_meta(|m| {
            m.snapper_pre = Some(41);
            m.alpm_window.start = Some("t".into());
        })
        .unwrap();
        let meta: Meta = serde_json::from_str(&fs::read_to_string(j.meta_json()).unwrap()).unwrap();
        assert_eq!(meta.snapper_pre, Some(41));
        assert_eq!(meta.alpm_window.start.as_deref(), Some("t"));
        assert!(
            !j.dir.join("meta.json.tmp").exists(),
            "the temporary was renamed over meta.json"
        );
        let leftovers: Vec<_> = fs::read_dir(&j.dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn a_failed_meta_write_keeps_the_previous_meta_in_memory_and_on_disk() {
        let root = tempfile::tempdir().unwrap();
        let t = tree();
        let mut w = JournalWriter::create(root.path(), "{}", t.path()).unwrap();
        let j = w.journal().clone();
        fs::create_dir(j.dir.join("meta.json.tmp")).unwrap();
        assert!(w.update_meta(|m| m.snapper_pre = Some(5)).is_err());
        assert_eq!(w.meta().snapper_pre, None);
        let meta: Meta = serde_json::from_str(&fs::read_to_string(j.meta_json()).unwrap()).unwrap();
        assert_eq!(meta.snapper_pre, None);
    }

    #[test]
    fn concurrent_writers_never_share_a_number() {
        let root = tempfile::tempdir().unwrap();
        let t = tree();
        let threads: Vec<_> = (0..6)
            .map(|_| {
                let (root, tree) = (root.path().to_path_buf(), t.path().to_path_buf());
                std::thread::spawn(move || JournalWriter::create(&root, "{}", &tree).unwrap().journal().n)
            })
            .collect();
        let mut ns: Vec<u32> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        ns.sort_unstable();
        assert_eq!(ns, [1, 2, 3, 4, 5, 6]);
    }
}

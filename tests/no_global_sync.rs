// A global sync waits on every disk on the machine; under load it once ran past the host's hung-task timeout and panicked it.
use std::fs;
use std::path::{Path, PathBuf};

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn bare_sync(line: &str) -> bool {
    let code = line.split('#').next().unwrap_or("");
    code.split([';', '&', '|', '{', '}', '(', ')'])
        .map(str::trim)
        .any(|command| command == "sync" || (command.starts_with("sync ") && !command.starts_with("sync -f")))
}

#[test]
fn nothing_on_the_host_calls_a_global_sync() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut all = Vec::new();
    files(&root.join("src"), &mut all);
    files(&root.join("tests"), &mut all);
    let rust_sync = concat!("libc::", "sync()");
    let mut found = Vec::new();
    for path in all {
        let Ok(text) = fs::read_to_string(&path) else { continue };
        let name = path.file_name().unwrap().to_string_lossy();
        // These run inside a test VM, where a sync only flushes the VM's own disks.
        let in_vm = name == "vmtest.sh";
        for (number, line) in text.lines().enumerate() {
            let rust = path.extension().is_some_and(|ext| ext == "rs") && line.contains(rust_sync);
            let shell = !in_vm && path.extension().is_none_or(|ext| ext == "sh") && bare_sync(line);
            if rust || shell {
                found.push(format!(
                    "{}:{}: {}",
                    path.strip_prefix(root).unwrap().display(),
                    number + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        found.is_empty(),
        "global syncs; use durable::sync_filesystems or `sync -f PATH`:\n{}",
        found.join("\n")
    );
}

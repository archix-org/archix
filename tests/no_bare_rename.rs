// A rename is only as durable as what was flushed before and after it: a cut can leave the new name in place and the file empty, as the write replay found with outside-changes.json.
use std::fs;
use std::path::{Path, PathBuf};

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn every_rename_in_src_goes_through_durable() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut all = Vec::new();
    files(&root.join("src"), &mut all);
    let bare = [concat!("fs::", "rename("), concat!("libc::", "rename(")];
    let mut found = Vec::new();
    for path in all {
        if path == root.join("src/durable.rs") {
            continue;
        }
        let text = fs::read_to_string(&path).unwrap();
        for (number, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            if bare.iter().any(|call| code.contains(call)) {
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
        "bare renames; use durable::rename (or durable::write for a whole file), which flushes what's renamed and both directories:\n{}",
        found.join("\n")
    );
}

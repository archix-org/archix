use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const TREE: &str = "/var/lib/dkms";

/// One `dkms status` line: module, version, kernel, arch and state.
#[derive(Debug, PartialEq)]
struct Entry {
    module: String,
    version: String,
    kernel: String,
    arch: String,
    state: String,
}

fn parse(status: &str) -> Vec<Entry> {
    status
        .lines()
        .filter_map(|line| {
            let (what, state) = line.rsplit_once(": ")?;
            let mut parts = what.split(", ");
            let (module, version) = parts.next()?.split_once('/')?;
            let kernel = parts.next()?;
            let arch = parts.next()?;
            Some(Entry {
                module: module.to_string(),
                version: version.to_string(),
                kernel: kernel.to_string(),
                arch: arch.to_string(),
                state: state.split_whitespace().next()?.to_string(),
            })
        })
        .collect()
}

fn stem(name: &str) -> Option<&str> {
    name.find(".ko").map(|end| &name[..end])
}

/// Files in the kernel's `updates/dkms` with the same module names as what DKMS built for `entry`.
fn stale_copies(tree: &Path, modules: &Path, entry: &Entry) -> Vec<PathBuf> {
    let built = tree
        .join(&entry.module)
        .join(&entry.version)
        .join(&entry.kernel)
        .join(&entry.arch)
        .join("module");
    let Ok(built) = fs::read_dir(built) else {
        return Vec::new();
    };
    let names: Vec<String> = built
        .flatten()
        .filter_map(|f| stem(&f.file_name().to_string_lossy()).map(str::to_string))
        .collect();
    let Ok(installed) = fs::read_dir(modules.join(&entry.kernel).join("updates/dkms")) else {
        return Vec::new();
    };
    let mut copies: Vec<PathBuf> = installed
        .flatten()
        .filter(|f| stem(&f.file_name().to_string_lossy()).is_some_and(|s| names.iter().any(|n| n == s)))
        .map(|f| f.path())
        .collect();
    copies.sort();
    copies
}

fn aside(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".archix-stale");
    PathBuf::from(name)
}

/// After a rollback puts a kernel's kept modules back, DKMS refuses to install its fresh build over them and lists it as only "built", so it would never remove them; `--force` is no answer, as DKMS then saves the copies as originals and restores them on removal. This sets the copies aside so a plain install owns the files again; it returns what it couldn't fix.
pub fn reconcile(modules: &Path) -> Vec<String> {
    let Ok(output) = Command::new("dkms").arg("status").output() else {
        return Vec::new();
    };
    let mut failures = Vec::new();
    for entry in parse(&String::from_utf8_lossy(&output.stdout)) {
        if entry.state != "built" || !modules.join(&entry.kernel).join("modules.dep").exists() {
            continue;
        }
        let target = format!("{}/{}", entry.module, entry.version);
        let copies = stale_copies(Path::new(TREE), modules, &entry);
        let moved: Vec<&PathBuf> = copies.iter().filter(|c| fs::rename(c, aside(c)).is_ok()).collect();
        let installed = Command::new("dkms")
            .args(["install", &target, "-k", &entry.kernel])
            .output()
            .is_ok_and(|o| o.status.success());
        for copy in moved {
            let _ = if installed {
                fs::remove_file(aside(copy))
            } else {
                fs::rename(aside(copy), copy)
            };
        }
        if !installed {
            failures.push(format!("dkms install {target} -k {}", entry.kernel));
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_dkms_status() {
        let status = "nvidia/610.43.02, 7.0.10-arch1-1, x86_64: built\nv4l2loopback/0.15.4, 7.2.9-arch1-1, x86_64: installed (WARNING! Diff between built and installed module!)\n";
        let entries = parse(status);
        assert_eq!(
            entries[0],
            Entry {
                module: "nvidia".into(),
                version: "610.43.02".into(),
                kernel: "7.0.10-arch1-1".into(),
                arch: "x86_64".into(),
                state: "built".into()
            }
        );
        assert_eq!(entries[1].state, "installed");
        assert_eq!(entries[1].module, "v4l2loopback");
    }

    #[test]
    fn finds_only_copies_of_what_dkms_built() {
        let root = tempfile::tempdir().unwrap();
        let tree = root.path().join("dkms");
        let modules = root.path().join("modules");
        let built = tree.join("nvidia/610.43.02/7.0.10-arch1-1/x86_64/module");
        let updates = modules.join("7.0.10-arch1-1/updates/dkms");
        fs::create_dir_all(&built).unwrap();
        fs::create_dir_all(&updates).unwrap();
        for name in ["nvidia.ko.zst", "nvidia-drm.ko.zst"] {
            fs::write(built.join(name), "").unwrap();
        }
        for name in [
            "nvidia.ko.zst",
            "nvidia-drm.ko",
            "v4l2loopback.ko.zst",
            "nvidia-drm-extra.ko.zst",
        ] {
            fs::write(updates.join(name), "").unwrap();
        }
        let entry = parse("nvidia/610.43.02, 7.0.10-arch1-1, x86_64: built").remove(0);
        assert_eq!(
            stale_copies(&tree, &modules, &entry),
            vec![updates.join("nvidia-drm.ko"), updates.join("nvidia.ko.zst")]
        );
    }
}

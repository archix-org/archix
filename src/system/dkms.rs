use std::path::Path;
use std::process::Command;

/// One `dkms status` line: module, version, kernel and state.
#[derive(Debug, PartialEq)]
struct Entry {
    module: String,
    version: String,
    kernel: String,
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
            Some(Entry {
                module: module.to_string(),
                version: version.to_string(),
                kernel: kernel.to_string(),
                state: state.split_whitespace().next()?.to_string(),
            })
        })
        .collect()
}

/// After a rollback puts a kernel's kept modules back, DKMS finds its own module already on disk at the same version and refuses to install its fresh build ("already installed ... override by specifying --force"). It then lists the module as only "built", and wouldn't remove those files when the package goes. This installs such builds over the copies, so DKMS owns them again; it returns what it couldn't fix.
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
        let installed = Command::new("dkms")
            .args(["install", "--force", &target, "-k", &entry.kernel])
            .output()
            .is_ok_and(|o| o.status.success());
        if !installed {
            failures.push(format!("dkms install --force {target} -k {}", entry.kernel));
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
                state: "built".into()
            }
        );
        assert_eq!(entries[1].state, "installed");
        assert_eq!(entries[1].module, "v4l2loopback");
    }
}

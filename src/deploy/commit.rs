use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::{Staged, Store};
use crate::durable;
use crate::testhooks::mark;

/// Puts a committed deployment's boot entry on the ESP; every file it writes must be complete before its final name appears.
pub trait EspStep {
    fn install(&self, number: u32, deployment: &Path) -> Result<()>;
}

impl<F: Fn(u32, &Path) -> Result<()>> EspStep for F {
    fn install(&self, number: u32, deployment: &Path) -> Result<()> {
        self(number, deployment)
    }
}

/// The prototype's ESP step: the deployment's UKI, a systemd-boot entry naming it and loader.conf's default, each through a fsynced temporary file and a rename.
pub struct SystemdBoot {
    pub esp: PathBuf,
    /// The UKI inside the deployment, such as boot/archix.efi.
    pub uki: PathBuf,
}

impl EspStep for SystemdBoot {
    fn install(&self, number: u32, deployment: &Path) -> Result<()> {
        mark("deploy-esp-uki");
        durable::copy(
            &deployment.join(&self.uki),
            &self.esp.join(format!("EFI/archix/{number}.efi")),
        )?;
        mark("deploy-esp-entry");
        let entry = format!("title archix deployment {number}\nefi /EFI/archix/{number}.efi\n");
        durable::write(
            &self.esp.join(format!("loader/entries/archix-{number}.conf")),
            entry.as_bytes(),
        )?;
        mark("deploy-esp-loader");
        let conf = self.esp.join("loader/loader.conf");
        let old = match fs::read_to_string(&conf) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", conf.display())),
        };
        durable::write(&conf, with_default(&old, &format!("archix-{number}.conf")).as_bytes())
    }
}

/// loader.conf with `default` naming `entry`, everything else kept.
pub fn with_default(conf: &str, entry: &str) -> String {
    let mut lines: Vec<String> = conf
        .lines()
        .filter(|line| line.split_whitespace().next() != Some("default"))
        .map(str::to_string)
        .collect();
    lines.push(format!("default {entry}"));
    lines.join("\n") + "\n"
}

impl Staged {
    /// Commits in the order that keeps every surviving pointer naming a complete deployment: flush, rename into place, ESP, then `current`.
    pub fn commit(self, store: &Store, esp: &Path, step: &dyn EspStep) -> Result<PathBuf> {
        self.check()?;
        mark("deploy-commit-sync");
        durable::sync_filesystems(&[store.dir(), esp]);
        mark("deploy-commit-rename");
        let path = store.path(self.number);
        rename_noreplace(&self.dir, &path)?;
        durable::sync_dir(&path)?;
        durable::sync_filesystems(&[store.dir()]);
        mark("deploy-commit-esp");
        step.install(self.number, &path)
            .with_context(|| format!("writing deployment {}'s boot entry", self.number))?;
        durable::sync_filesystems(&[esp]);
        mark("deploy-commit-current");
        store.point_current(self.number)?;
        durable::sync_filesystems(&[store.dir()]);
        mark("deploy-done");
        Ok(path)
    }
}

fn rename_noreplace(from: &Path, to: &Path) -> Result<()> {
    let c = |path: &Path| CString::new(path.as_os_str().as_bytes()).context("path has a NUL byte");
    let (c_from, c_to) = (c(from)?, c(to)?);
    let done = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            c_from.as_ptr(),
            libc::AT_FDCWD,
            c_to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if done != 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EEXIST) {
            bail!("{} already exists", to.display());
        }
        return Err(err).with_context(|| format!("renaming {} to {}", from.display(), to.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_entry_is_replaced_and_the_rest_kept() {
        assert_eq!(
            with_default("timeout 2\ndefault archix-1.conf\nconsole-mode keep\n", "archix-2.conf"),
            "timeout 2\nconsole-mode keep\ndefault archix-2.conf\n"
        );
        assert_eq!(with_default("", "archix-5.conf"), "default archix-5.conf\n");
        assert_eq!(with_default("defaults x\n", "a.conf"), "defaults x\ndefault a.conf\n");
    }
}

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;

use anyhow::{Context, Result};

use super::Store;

/// The deployment a boot entry file names: archix-<N>.conf, with or without systemd-boot's +tries suffix.
pub fn entry_number(entry: &str) -> Option<u32> {
    let rest = entry.strip_prefix("archix-")?.strip_suffix(".conf")?;
    let digits = rest.split('+').next()?;
    if digits.is_empty() || digits.starts_with('0') || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The deployment loader.conf's last `default` line names.
pub fn boot_default(conf: &str) -> Option<u32> {
    conf.lines()
        .filter_map(|line| line.strip_prefix("default"))
        .rfind(|rest| rest.starts_with([' ', '\t']))
        .and_then(|rest| entry_number(rest.trim()))
}

/// What `deployments.keep` leaves: never the booted, current or boot-default deployment, and then the newest others until `keep` are left.
#[derive(Debug, Clone, Default)]
pub struct Keep {
    pub keep: usize,
    pub booted: Option<u32>,
    pub current: Option<u32>,
    pub boot_default: Option<u32>,
}

impl Keep {
    /// The pins as they stand: the deployment systemd-boot started, `current`, and loader.conf's default on `esp`.
    pub fn read(keep: usize, store: &Store, esp: &Path) -> Result<Keep> {
        let conf = esp.join("loader/loader.conf");
        let conf = match fs::read_to_string(&conf) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", conf.display())),
        };
        Ok(Keep {
            keep,
            booted: crate::testhooks::selected_entry().as_deref().and_then(entry_number),
            current: store.current()?,
            boot_default: boot_default(&conf),
        })
    }

    /// The committed deployments to delete, oldest first.
    pub fn to_delete(&self, committed: &[u32]) -> Vec<u32> {
        let committed: BTreeSet<u32> = committed.iter().copied().collect();
        let mut kept: BTreeSet<u32> = [self.booted, self.current, self.boot_default]
            .into_iter()
            .flatten()
            .filter(|n| committed.contains(n))
            .collect();
        for &n in committed.iter().rev() {
            if kept.len() >= self.keep.max(1) {
                break;
            }
            kept.insert(n);
        }
        committed.difference(&kept).copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keep(keep: usize, booted: u32, current: u32, boot_default: u32) -> Keep {
        Keep {
            keep,
            booted: Some(booted),
            current: Some(current),
            boot_default: Some(boot_default),
        }
    }

    #[test]
    fn keeps_the_newest_after_the_pinned_ones() {
        assert_eq!(keep(2, 8, 8, 8).to_delete(&[5, 6, 7, 8]), vec![5, 6]);
        assert_eq!(keep(3, 8, 8, 8).to_delete(&[5, 6, 7, 8]), vec![5]);
    }

    #[test]
    fn keep_one_drops_the_previous_deployment_only_once_the_new_one_booted() {
        assert_eq!(keep(1, 7, 8, 8).to_delete(&[6, 7, 8]), vec![6]);
        assert_eq!(keep(1, 8, 8, 8).to_delete(&[6, 7, 8]), vec![6, 7]);
    }

    #[test]
    fn never_deletes_the_booted_current_or_default_deployment_whatever_keep_says() {
        let pinned = keep(1, 3, 5, 2);
        let gone = pinned.to_delete(&[1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(gone, vec![1, 4, 6, 7]);
        assert_eq!(keep(0, 4, 4, 4).to_delete(&[3, 4]), vec![3]);
    }

    #[test]
    fn after_a_rollback_the_current_deployment_and_the_newest_are_kept() {
        assert_eq!(keep(2, 3, 3, 3).to_delete(&[2, 3, 6, 7]), vec![2, 6]);
    }

    #[test]
    fn entries_and_the_loader_default_name_deployments() {
        assert_eq!(entry_number("archix-12.conf"), Some(12));
        assert_eq!(entry_number("archix-12+2-1.conf"), Some(12));
        assert_eq!(entry_number("archix-012.conf"), None);
        assert_eq!(entry_number("arch.conf"), None);
        assert_eq!(boot_default("timeout 2\ndefault archix-3.conf\n"), Some(3));
        assert_eq!(boot_default("default archix-3.conf\ndefault\tarchix-4.conf\n"), Some(4));
        assert_eq!(boot_default("defaults archix-3.conf\n"), None);
        assert_eq!(boot_default("default arch.conf\n"), None);
    }

    #[test]
    fn pins_naming_deleted_deployments_take_no_place() {
        let stale = Keep {
            keep: 2,
            booted: Some(1),
            current: Some(9),
            boot_default: None,
        };
        assert_eq!(stale.to_delete(&[7, 8, 9]), vec![7]);
    }
}

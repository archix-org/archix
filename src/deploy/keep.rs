use std::collections::BTreeSet;

/// What `deployments.keep` leaves: never the booted, current or boot-default deployment, and then the newest others until `keep` are left.
#[derive(Debug, Clone, Default)]
pub struct Keep {
    pub keep: usize,
    pub booted: Option<u32>,
    pub current: Option<u32>,
    pub boot_default: Option<u32>,
}

impl Keep {
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

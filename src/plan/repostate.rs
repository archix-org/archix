use std::cmp::Ordering;
use std::collections::BTreeMap;

use anyhow::{Result, bail};

use crate::date::Date;

/// Installed packages a repo state has older versions of; packages it doesn't have don't count.
pub fn newer_than(installed: &BTreeMap<String, String>, sync: &BTreeMap<String, String>) -> Vec<String> {
    installed
        .iter()
        .filter(|(name, have)| {
            sync.get(*name)
                .is_some_and(|repo| alpm::vercmp(have.as_str(), repo.as_str()) == Ordering::Greater)
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// SCOPE 2.1's "never older than installed": the earliest date from `start` whose dbs aren't older than anything installed, with its versions.
pub fn earliest_not_older(
    start: Date,
    max_days: u32,
    installed: &BTreeMap<String, String>,
    mut versions_at: impl FnMut(Date) -> Result<BTreeMap<String, String>>,
) -> Result<(Date, BTreeMap<String, String>)> {
    let mut date = start;
    let mut last_newer = Vec::new();
    for _ in 0..=max_days {
        let versions = versions_at(date)?;
        last_newer = newer_than(installed, &versions);
        if last_newer.is_empty() {
            return Ok((date, versions));
        }
        date = date.next();
    }
    bail!(
        "no archive date within {max_days} days of {start} is as new as what's installed (newer: {})",
        last_newer.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[test]
    fn moves_forward_to_the_first_date_not_older_than_installed() {
        let start: Date = "2026-11-01".parse().unwrap();
        let installed = map(&[("glibc", "2.41-2"), ("foreign", "9-9")]);
        let mut asked = Vec::new();
        let (date, versions) = earliest_not_older(start, 14, &installed, |d| {
            asked.push(d);
            Ok(match d.day {
                1 | 2 => map(&[("glibc", "2.41-1")]),
                _ => map(&[("glibc", "2.41-2")]),
            })
        })
        .unwrap();
        assert_eq!(date.to_string(), "2026-11-03");
        assert_eq!(versions["glibc"], "2.41-2");
        assert_eq!(asked.len(), 3);
    }

    #[test]
    fn keeps_the_date_when_nothing_installed_is_newer() {
        let start: Date = "2026-11-01".parse().unwrap();
        let (date, _) = earliest_not_older(start, 3, &map(&[("a", "1-1")]), |_| Ok(map(&[("a", "2-1")]))).unwrap();
        assert_eq!(date, start);
    }

    #[test]
    fn refuses_when_no_date_catches_up() {
        let start: Date = "2026-11-01".parse().unwrap();
        let err = earliest_not_older(start, 2, &map(&[("a", "3-1")]), |_| Ok(map(&[("a", "2-1")]))).unwrap_err();
        assert!(err.to_string().contains("newer: a"), "{err}");
    }
}

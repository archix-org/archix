use std::collections::BTreeMap;
use std::sync::LazyLock;

use crate::pacman::vercmp;
use crate::plan::Change;

/// Shipped rules: package name to how many leading version parts make its major version.
static MAJORS: LazyLock<BTreeMap<String, usize>> = LazyLock::new(|| {
    let rules: BTreeMap<String, serde_json::Value> =
        serde_json::from_str(include_str!("unsafe_downgrades.json")).expect("unsafe_downgrades.json is valid");
    rules
        .into_iter()
        .filter_map(|(name, parts)| Some((name, usize::try_from(parts.as_u64()?).ok()?)))
        .collect()
});

/// The first `parts` dot-separated parts of a version's pkgver, epoch included.
fn major(version: &str, parts: usize) -> String {
    let (epoch, rest) = version.split_once(':').map_or(("", version), |(e, r)| (e, r));
    let pkgver = rest.rsplit_once('-').map_or(rest, |(v, _)| v);
    let head: Vec<&str> = pkgver.split('.').take(parts).collect();
    if epoch.is_empty() {
        head.join(".")
    } else {
        format!("{epoch}:{}", head.join("."))
    }
}

/// Why downgrading `name` from `from` to `to` isn't safe: a shipped major-version rule, or a version from the config's `unsafe_downgrades`.
pub fn crossing(name: &str, from: &str, to: &str, configured: &BTreeMap<String, Vec<String>>) -> Option<String> {
    if vercmp(to, from) != std::cmp::Ordering::Less {
        return None;
    }
    if let Some(boundary) = configured
        .get(name)
        .into_iter()
        .flatten()
        .find(|v| vercmp(from, v) != std::cmp::Ordering::Less && vercmp(to, v) == std::cmp::Ordering::Less)
    {
        return Some(format!("crosses {boundary}, which unsafe_downgrades marks"));
    }
    let parts = *MAJORS.get(name)?;
    let (old, new) = (major(from, parts), major(to, parts));
    (old != new)
        .then(|| format!("goes back from major version {old} to {new}, and {name} migrates its data between them"))
}

/// Both generations' configured boundaries, per package.
pub fn merged(a: &BTreeMap<String, Vec<String>>, b: &BTreeMap<String, Vec<String>>) -> BTreeMap<String, Vec<String>> {
    let mut all = a.clone();
    for (name, versions) in b {
        let list = all.entry(name.clone()).or_default();
        list.extend(
            versions
                .iter()
                .filter(|v| !list.contains(v))
                .cloned()
                .collect::<Vec<_>>(),
        );
    }
    all
}

/// One line per unsafe downgrade among `changes`.
pub fn unsafe_downgrades(changes: &[Change], configured: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    changes
        .iter()
        .filter_map(|change| match change {
            Change::Downgrade { name, from, to } => {
                crossing(name, from, to, configured).map(|why| format!("{name} {from} -> {to} {why}"))
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_rules_compare_major_versions() {
        let none = BTreeMap::new();
        assert!(crossing("postgresql", "17.2-1", "16.6-1", &none).is_some());
        assert!(crossing("postgresql", "17.2-1", "17.1-3", &none).is_none());
        assert!(
            crossing("mariadb", "11.4.3-1", "11.2.1-1", &none).is_some(),
            "mariadb's major is two parts"
        );
        assert!(crossing("mariadb", "11.4.3-1", "11.4.2-1", &none).is_none());
        assert!(
            crossing("postgresql", "16.6-1", "17.2-1", &none).is_none(),
            "upgrades are fine"
        );
        assert!(crossing("tree", "2.2-1", "1.8-1", &none).is_none());
    }

    #[test]
    fn configured_boundaries_apply_to_any_package() {
        let configured = BTreeMap::from([("myapp".to_string(), vec!["2.0".to_string()])]);
        assert!(crossing("myapp", "2.1-1", "1.9-1", &configured).is_some());
        assert!(crossing("myapp", "2.1-1", "2.0.5-1", &configured).is_none());
        assert!(crossing("myapp", "1.9-1", "1.8-1", &configured).is_none());
    }

    #[test]
    fn both_generations_boundaries_count() {
        let a = BTreeMap::from([("app".to_string(), vec!["2.0".to_string()])]);
        let b = BTreeMap::from([("app".to_string(), vec!["3.0".to_string(), "2.0".to_string()])]);
        assert_eq!(merged(&a, &b)["app"], ["2.0", "3.0"]);
    }

    #[test]
    fn majors_keep_the_epoch() {
        assert_eq!(major("1:17.2-1", 1), "1:17");
        assert_eq!(major("11.4.3-1", 2), "11.4");
    }
}

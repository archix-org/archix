use std::collections::BTreeMap;

use anyhow::{Result, bail};

use super::git::AurGit;
use crate::lock::{AUR_REPO, HostLock};

/// Each AUR base in a lock with the commit it's locked at; the config's own recipes need no review.
pub fn commits(lock: Option<&HostLock>) -> BTreeMap<String, String> {
    lock.into_iter()
        .flat_map(|lock| lock.packages.values())
        .filter(|pkg| pkg.repo == AUR_REPO)
        .filter_map(|pkg| Some((pkg.base.clone()?, pkg.commit.clone()?)))
        .collect()
}

/// Shows every AUR build recipe entering the lock (a diff for changed bases, the PKGBUILD for new ones) and asks to trust it.
pub fn review(
    git: &AurGit,
    old: Option<&HostLock>,
    new: &HostLock,
    ask: bool,
    confirm: &dyn Fn(&str) -> Result<bool>,
) -> Result<()> {
    let before = commits(old);
    for (base, commit) in commits(Some(new)) {
        let short = &commit[..commit.len().min(12)];
        let shown = match before.get(&base) {
            Some(previous) if *previous == commit => continue,
            Some(previous) => {
                println!(
                    "==> {base}: build recipe changed ({} -> {short})",
                    &previous[..previous.len().min(12)]
                );
                git.diff(&base, previous, &commit)?
            }
            None => {
                println!("==> {base}: new AUR package at {short}");
                git.show(&base, &commit, "PKGBUILD")?
            }
        };
        if !ask {
            continue;
        }
        println!("{shown}");
        if !confirm(&format!("trust {base} at {short} and lock it?"))? {
            bail!("not locking {base}; nothing was changed");
        }
    }
    Ok(())
}

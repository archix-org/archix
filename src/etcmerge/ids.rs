use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::accounts::{FILES, Texts, ids};

/// Every UID and GID any deployment's or the running system's account files ever handed out on this machine, with the first name seen, so no later account gets a number files on /home or /var may still carry.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Registry {
    pub uids: BTreeMap<u32, String>,
    pub gids: BTreeMap<u32, String>,
}

/// What placeholder accounts are named, so they're never mistaken for real ones and always removed.
pub const HOLD_PREFIX: &str = "archix-hold-";

fn path(state: &Path) -> PathBuf {
    state.join("id-registry.json")
}

impl Registry {
    pub fn load(state: &Path) -> Registry {
        fs::read_to_string(path(state))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn save(&self, state: &Path) -> Result<()> {
        crate::durable::write(&path(state), serde_json::to_string_pretty(self)?.as_bytes())
    }
}

/// Adds every number `texts` hands out that the registry hasn't seen.
pub fn record(state: &Path, texts: &Texts) -> Result<()> {
    fs::create_dir_all(state).with_context(|| format!("creating {}", state.display()))?;
    let lock = fs::File::create(state.join("id-registry.lock")).context("opening the ID registry's lock")?;
    // A switch and the /etc merge can both record at once; without it one's numbers would be lost.
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error()).context("locking the ID registry");
    }
    let mut registry = Registry::load(state);
    let [uids, gids] = ids(texts);
    let mut changed = false;
    for (map, list) in [(&mut registry.uids, uids), (&mut registry.gids, gids)] {
        for (id, name) in list {
            if !name.starts_with(HOLD_PREFIX) && !map.contains_key(&id) {
                map.insert(id, name);
                changed = true;
            }
        }
    }
    if changed {
        registry.save(state)?;
    }
    Ok(())
}

pub fn read_texts(etc: &Path) -> Texts {
    FILES.map(|file| fs::read_to_string(etc.join(file)).ok())
}

fn rewrite(etc: &Path, file: &str, text: &str) -> Result<()> {
    let path = etc.join(file);
    super::tree::write_like(&path, text.as_bytes(), &path).with_context(|| format!("writing {}", path.display()))
}

/// Accounts archix declares, which may take a registered number: their own back, or one the config names explicitly.
#[derive(Debug, Default)]
pub struct Declared {
    pub names: BTreeSet<String>,
    pub uids: BTreeSet<u32>,
    pub gids: BTreeSet<u32>,
}

/// Before a staging transaction: placeholder entries in `etc`'s passwd and group for every registered number it doesn't use, so sysusers and useradd there pick others.
pub fn hold(state: &Path, etc: &Path, keep: &Declared) -> Result<usize> {
    let texts = read_texts(etc);
    record(state, &texts)?;
    let registry = Registry::load(state);
    let [uids, gids] = ids(&texts);
    let used_uids: BTreeSet<u32> = uids.iter().map(|(id, _)| *id).collect();
    let used_gids: BTreeSet<u32> = gids.iter().map(|(id, _)| *id).collect();
    let mut held = 0;
    if let Some(passwd) = &texts[0] {
        let mut text = passwd.clone();
        for (id, name) in registry
            .uids
            .iter()
            .filter(|(id, name)| !used_uids.contains(id) && !keep.names.contains(*name) && !keep.uids.contains(id))
        {
            text.push_str(&format!(
                "{HOLD_PREFIX}u{id}:x:{id}:65534:archix holds UID {id}, once {name}'s:/:/usr/bin/nologin\n"
            ));
            held += 1;
        }
        if text != *passwd {
            rewrite(etc, "passwd", &text)?;
        }
    }
    if let Some(group) = &texts[1] {
        let mut text = group.clone();
        for (id, _) in registry
            .gids
            .iter()
            .filter(|(id, name)| !used_gids.contains(id) && !keep.names.contains(*name) && !keep.gids.contains(id))
        {
            text.push_str(&format!("{HOLD_PREFIX}g{id}:x:{id}:\n"));
            held += 1;
        }
        if text != *group {
            rewrite(etc, "group", &text)?;
        }
    }
    Ok(held)
}

/// After the transaction and archix's own users: takes the placeholders out of all four files and records what the deployment hands out.
pub fn release(state: &Path, etc: &Path) -> Result<()> {
    let texts = read_texts(etc);
    for (file, text) in FILES.iter().zip(&texts) {
        let Some(text) = text else { continue };
        let kept: String = text
            .lines()
            .filter(|line| !line.starts_with(HOLD_PREFIX))
            .map(|line| format!("{line}\n"))
            .collect();
        if kept != *text {
            rewrite(etc, file, &kept)?;
        }
    }
    record(state, &read_texts(etc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_number_once_handed_out_is_held_in_staging_and_released_after() {
        let dir = tempfile::tempdir().unwrap();
        let (state, etc) = (dir.path().join("state"), dir.path().join("etc"));
        fs::create_dir_all(&etc).unwrap();
        fs::create_dir_all(&state).unwrap();
        let old = texts(
            "root:x:0:0::/root:/bin/bash\ngone:x:1005:1005::/home/gone:/bin/bash\nmine:x:1006:1006::/:/bin/sh\n",
            "root:x:0:\ngone:x:1005:\n",
        );
        record(&state, &old).unwrap();
        fs::write(etc.join("passwd"), "root:x:0:0::/root:/bin/bash\n").unwrap();
        fs::write(etc.join("group"), "root:x:0:\n").unwrap();
        fs::write(etc.join("shadow"), "root:!:1::::::\n").unwrap();
        let keep = Declared {
            names: BTreeSet::from(["mine".to_string()]),
            ..Declared::default()
        };
        let held = hold(&state, &etc, &keep).unwrap();
        assert_eq!(held, 2);
        let passwd = fs::read_to_string(etc.join("passwd")).unwrap();
        assert!(passwd.contains("archix-hold-u1005:x:1005:"), "{passwd}");
        assert!(
            !passwd.contains("1006"),
            "a declared account takes its own number back: {passwd}"
        );
        assert!(
            fs::read_to_string(etc.join("group"))
                .unwrap()
                .contains("archix-hold-g1005:x:1005:")
        );
        fs::write(etc.join("shadow"), "root:!:1::::::\narchix-hold-u1005:!*:1::::::\n").unwrap();
        fs::write(
            etc.join("passwd"),
            format!("{passwd}pkg:x:960:960::/:/usr/bin/nologin\n"),
        )
        .unwrap();
        release(&state, &etc).unwrap();
        assert_eq!(
            fs::read_to_string(etc.join("passwd")).unwrap(),
            "root:x:0:0::/root:/bin/bash\npkg:x:960:960::/:/usr/bin/nologin\n"
        );
        assert_eq!(fs::read_to_string(etc.join("shadow")).unwrap(), "root:!:1::::::\n");
        let registry = Registry::load(&state);
        assert_eq!(registry.uids.get(&960).map(String::as_str), Some("pkg"));
        assert_eq!(registry.uids.get(&1005).map(String::as_str), Some("gone"));
    }

    #[test]
    fn records_made_at_once_all_land() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().to_path_buf();
        let threads: Vec<_> = (0..16u32)
            .map(|i| {
                let state = state.clone();
                std::thread::spawn(move || {
                    let id = 2000 + i;
                    record(
                        &state,
                        &texts(&format!("u{i}:x:{id}:{id}::/:/bin/sh\n"), &format!("u{i}:x:{id}:\n")),
                    )
                    .unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let registry = Registry::load(&state);
        assert_eq!(registry.uids.len(), 16, "{:?}", registry.uids);
        assert_eq!(registry.gids.len(), 16, "{:?}", registry.gids);
    }

    fn texts(passwd: &str, group: &str) -> Texts {
        [Some(passwd.to_string()), Some(group.to_string()), None, None]
    }
}

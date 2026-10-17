use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use anyhow::Result;

/// Built packages whose DT_NEEDED soname's owner changes and stops shipping it; `new_files(name)` gives a changing package's files after the switch, None if removed.
pub fn broken_builds(
    root: &Path,
    local_db: &Path,
    built: &[String],
    changing: &BTreeSet<String>,
    mut new_files: impl FnMut(&str) -> Result<Option<Vec<String>>>,
) -> Result<Vec<String>> {
    let owners = crate::conflicts::owners(local_db);
    let library_owner: HashMap<&str, &str> = owners
        .iter()
        .flat_map(|(owner, files)| {
            files
                .iter()
                .filter(|file| file.starts_with("usr/lib/") && file.contains(".so"))
                .map(move |file| (file.as_str(), owner.as_str()))
        })
        .collect();
    let mut shipped: BTreeMap<String, Option<BTreeSet<String>>> = BTreeMap::new();
    let mut broken = Vec::new();
    for package in built {
        let Some(files) = owners.get(package) else { continue };
        let mut lost = None;
        'files: for file in files
            .iter()
            .filter(|file| file.starts_with("usr/") || file.starts_with("opt/"))
        {
            for soname in crate::elf::needed(&root.join(file)).unwrap_or_default() {
                let library = format!("usr/lib/{soname}");
                let Some(owner) = library_owner.get(library.as_str()).copied() else {
                    continue;
                };
                if owner == package || !changing.contains(owner) {
                    continue;
                }
                if !shipped.contains_key(owner) {
                    let after = new_files(owner)?.map(|files| files.into_iter().collect());
                    shipped.insert(owner.to_string(), after);
                }
                if !shipped[owner].as_ref().is_some_and(|files| files.contains(&library)) {
                    lost = Some(format!("{package} (links {soname}, which {owner} stops shipping)"));
                    break 'files;
                }
            }
        }
        if let Some(reason) = lost {
            eprintln!(":: rebuilding {reason}");
            broken.push(package.clone());
        }
    }
    Ok(broken)
}

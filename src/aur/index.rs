use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use flate2::read::GzDecoder;
use serde::Deserialize;

use super::srcinfo::dep_name;
use crate::archive::Archive;

const DUMP: &str = "packages-meta-ext-v1.json.gz";
const MAX_AGE: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Entry {
    pub name: String,
    pub package_base: String,
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub provides: Vec<String>,
    #[serde(default)]
    pub num_votes: u32,
    #[serde(default)]
    pub popularity: f64,
    #[serde(default)]
    pub out_of_date: Option<u64>,
}

/// Every AUR package from the AUR's metadata dump, so names resolve without asking the AUR each time.
pub struct Index {
    entries: HashMap<String, Entry>,
    providers: HashMap<String, Vec<String>>,
}

impl Index {
    /// Loads the cached dump, downloading a fresh one if it's missing, older than an hour, or `refresh` is set.
    pub fn load(archive: &Archive, aur_url: &str, cache: &Path, refresh: bool) -> Result<Index> {
        let path = cache.join(DUMP);
        let stale = fs::metadata(&path).and_then(|m| m.modified()).map_or(true, |modified| {
            SystemTime::now().duration_since(modified).unwrap_or_default() > MAX_AGE
        });
        if refresh || stale {
            fs::create_dir_all(cache)?;
            let url = format!("{}/{DUMP}", aur_url.trim_end_matches('/'));
            let fetched = match archive.fetch(&url, &path) {
                Ok(true) => Ok(()),
                Ok(false) => Err(anyhow::anyhow!("{url} does not exist")),
                Err(e) => Err(e),
            };
            if let Err(e) = fetched {
                if !path.is_file() {
                    return Err(e).context("downloading the AUR package list");
                }
                eprintln!("warning: could not refresh the AUR package list, using the cached one: {e:#}");
            }
        }
        Index::read(&path)
    }

    pub fn read(path: &Path) -> Result<Index> {
        let mut text = String::new();
        GzDecoder::new(File::open(path).with_context(|| format!("reading {}", path.display()))?)
            .read_to_string(&mut text)
            .with_context(|| format!("decompressing {}", path.display()))?;
        let list: Vec<Entry> = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Ok(Index::new(list))
    }

    pub fn new(list: Vec<Entry>) -> Index {
        let mut providers: HashMap<String, Vec<String>> = HashMap::new();
        for entry in &list {
            for provided in &entry.provides {
                providers
                    .entry(dep_name(provided).to_string())
                    .or_default()
                    .push(entry.name.clone());
            }
        }
        let entries = list.into_iter().map(|entry| (entry.name.clone(), entry)).collect();
        Index { entries, providers }
    }

    pub fn get(&self, name: &str) -> Option<&Entry> {
        self.entries.get(name)
    }

    /// The package satisfying a dependency: one with that exact name, else the most popular provider.
    pub fn satisfier(&self, dep: &str) -> Option<&Entry> {
        let name = dep_name(dep);
        self.entries.get(name).or_else(|| {
            self.providers
                .get(name)?
                .iter()
                .filter_map(|provider| self.entries.get(provider))
                .max_by(|a, b| a.popularity.total_cmp(&b.popularity))
        })
    }

    /// Packages whose name or description contains every term, most popular first.
    pub fn search(&self, terms: &[String]) -> Vec<&Entry> {
        let terms: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();
        let mut hits: Vec<&Entry> = self
            .entries
            .values()
            .filter(|entry| {
                let haystack = format!("{} {}", entry.name, entry.description.as_deref().unwrap_or("")).to_lowercase();
                terms.iter().all(|term| haystack.contains(term))
            })
            .collect();
        hits.sort_by(|a, b| b.popularity.total_cmp(&a.popularity).then_with(|| a.name.cmp(&b.name)));
        hits
    }
}

pub fn dump_path(cache: &Path) -> PathBuf {
    cache.join(DUMP)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, provides: &[&str], popularity: f64) -> Entry {
        Entry {
            name: name.into(),
            package_base: name.into(),
            version: "1-1".into(),
            description: Some(format!("the {name} package")),
            provides: provides.iter().map(|p| p.to_string()).collect(),
            num_votes: 0,
            popularity,
            out_of_date: None,
        }
    }

    #[test]
    fn finds_by_name_or_most_popular_provider() {
        let index = Index::new(vec![
            entry("jdk-a", &["java-runtime=21"], 1.0),
            entry("jdk-b", &["java-runtime=21"], 5.0),
            entry("hello", &[], 0.0),
        ]);
        assert_eq!(index.satisfier("hello>=1").unwrap().name, "hello");
        assert_eq!(index.satisfier("java-runtime").unwrap().name, "jdk-b");
        assert!(index.satisfier("nothing").is_none());
        assert_eq!(
            index
                .search(&["JDK".into()])
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["jdk-b", "jdk-a"]
        );
    }

    #[test]
    fn parses_the_dump_format() {
        let json = r#"[{"ID":1,"Name":"yay-bin","PackageBaseID":2,"PackageBase":"yay-bin","Version":"13.0.1-1",
            "Description":"AUR helper","NumVotes":5,"Popularity":1.5,"OutOfDate":null,"Provides":["yay"],"Depends":["pacman"]}]"#;
        let list: Vec<Entry> = serde_json::from_str(json).unwrap();
        let index = Index::new(list);
        assert_eq!(index.satisfier("yay").unwrap().package_base, "yay-bin");
    }
}

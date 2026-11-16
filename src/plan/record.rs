use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::system::SystemState;

pub const DEFAULT_PATH: &str = "/var/lib/archix/last-applied.json";

/// What the last switch applied: the drift baseline for the next plan.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LastApplied {
    pub schema_version: u32,
    #[serde(default)]
    pub declared: BTreeSet<String>,
    /// Packages installed when the switch finished.
    #[serde(default)]
    pub installed: BTreeSet<String>,
    #[serde(default)]
    pub system: SystemState,
    /// HMACs of secret values under the machine's key, by secret name.
    #[serde(default)]
    pub secret_hmacs: BTreeMap<String, String>,
}

impl LastApplied {
    /// The record at `path`; None when no switch has written one yet.
    pub fn load(path: &Path) -> Result<Option<LastApplied>> {
        match fs::read_to_string(path) {
            Ok(text) => Ok(Some(
                serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }
}

pub fn default_path() -> PathBuf {
    PathBuf::from(DEFAULT_PATH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_record_is_none_and_a_written_one_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("last-applied.json");
        assert_eq!(LastApplied::load(&path).unwrap(), None);
        let record = LastApplied {
            schema_version: 1,
            declared: ["vim".to_string()].into(),
            ..LastApplied::default()
        };
        fs::write(&path, serde_json::to_string(&record).unwrap()).unwrap();
        assert_eq!(LastApplied::load(&path).unwrap(), Some(record));
    }
}

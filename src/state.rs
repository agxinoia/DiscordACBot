//! Persisted "last seen" digests.
//!
//! Without this the bot would re-announce every tracked module on restart, so
//! the file is written atomically (temp file + rename) to survive a crash
//! mid-write.

use crate::eac::{ModuleEntry, Snapshot};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Seen {
    pub digest: String,
    pub size: u64,
    /// Unix seconds.
    pub seen_at: u64,
    /// Retained so the next change can be described, not just detected.
    /// Defaulted so state files written before diffing still load.
    #[serde(default)]
    pub modules: Vec<ModuleEntry>,
    #[serde(default)]
    pub pe_timestamp: Option<u32>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    targets: BTreeMap<String, Seen>,
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Stable key for one product/deployment/platform triple.
pub fn target_key(product_id: &str, deployment_id: &str, platform: &str) -> String {
    format!("{product_id}/{deployment_id}/{platform}")
}

pub struct Store {
    path: PathBuf,
    state: State,
}

impl Store {
    /// Load the state file, treating a missing file as empty. A *corrupt*
    /// file is an error rather than a silent reset: quietly starting from
    /// scratch would re-announce everything.
    pub fn load(path: &Path) -> Result<Self> {
        let state = match std::fs::read_to_string(path) {
            Ok(raw) if raw.trim().is_empty() => State::default(),
            Ok(raw) => serde_json::from_str(&raw)
                .with_context(|| format!("parsing state file {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => {
                return Err(e).with_context(|| format!("reading state file {}", path.display()));
            }
        };
        Ok(Self {
            path: path.to_path_buf(),
            state,
        })
    }

    pub fn get(&self, key: &str) -> Option<&Seen> {
        self.state.targets.get(key)
    }

    pub fn record(&mut self, key: &str, snapshot: &Snapshot) {
        self.state.targets.insert(
            key.to_string(),
            Seen {
                digest: snapshot.digest().to_string(),
                size: snapshot.size(),
                seen_at: now_unix(),
                modules: snapshot.modules.clone(),
                pe_timestamp: snapshot.pe.as_ref().map(|p| p.timestamp),
            },
        );
    }

    pub fn entries(&self) -> impl Iterator<Item = (&String, &Seen)> {
        self.state.targets.iter()
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(&self.state).context("serialising state")?;

        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "eac-tracker-test-{}-{}.json",
            name,
            std::process::id()
        ));
        p
    }

    #[test]
    fn missing_file_loads_as_empty() {
        let path = temp_path("missing");
        let _ = std::fs::remove_file(&path);
        let store = Store::load(&path).unwrap();
        assert!(store.get("anything").is_none());
    }

    #[test]
    fn round_trips_through_disk() {
        let path = temp_path("roundtrip");
        let _ = std::fs::remove_file(&path);

        let mut store = Store::load(&path).unwrap();
        let key = target_key("prod", "deploy", "win64");
        let body = br#"{"modules":[{"name":"driver.sys","size":10,"hash":"aa"}]}"#;
        store.record(&key, &crate::eac::Snapshot::for_test(body));
        store.save().unwrap();

        let reloaded = Store::load(&path).unwrap();
        let seen = reloaded.get(&key).expect("entry persisted");
        assert_eq!(seen.digest, crate::analysis::hashes(body).sha256);
        assert_eq!(seen.size, body.len() as u64);
        assert_eq!(seen.modules.len(), 1, "modules persist for later diffing");
        assert_eq!(seen.modules[0].name, "driver.sys");

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn corrupt_file_is_an_error_not_a_silent_reset() {
        let path = temp_path("corrupt");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(Store::load(&path).is_err());
        std::fs::remove_file(&path).unwrap();
    }
}

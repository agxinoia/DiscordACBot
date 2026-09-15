//! Content-addressed payload archive.
//!
//! Change detection alone is lossy: once a new payload lands, the bytes it
//! replaced are gone, and no amount of later analysis can recover them. This
//! keeps every distinct payload under its own SHA-256 plus an append-only
//! JSONL index, so history can be diffed after the fact.
//!
//! It is also load-bearing for TLSH distance: tlsh2 cannot parse a hash string
//! back into a comparable value, so measuring how far a build moved requires
//! the previous bytes to still be on disk.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::analysis::{Hashes, PeInfo};
use crate::eac::ModuleEntry;

/// One line of `index.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    /// Unix seconds.
    pub seen_at: u64,
    pub game: String,
    pub product_id: String,
    pub deployment_id: String,
    pub platform: String,
    pub url: String,
    pub size: u64,
    #[serde(flatten)]
    pub hashes: Hashes,
    pub tlsh: Option<String>,
    pub format: String,
    pub entropy: f64,
    /// Full response headers; `Last-Modified` is the closest thing the CDN
    /// gives to a publish time.
    pub headers: BTreeMap<String, String>,
    pub modules: Vec<ModuleEntry>,
    pub pe: Option<PeInfo>,
    /// SHA-256 this payload replaced, if any.
    pub previous_sha256: Option<String>,
}

pub struct Archive {
    root: PathBuf,
}

impl Archive {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Blobs are sharded by the first byte of the digest so no single
    /// directory accumulates thousands of entries.
    fn blob_path(&self, sha256: &str) -> PathBuf {
        let shard = sha256.get(..2).unwrap_or("00");
        self.root
            .join("blobs")
            .join(shard)
            .join(format!("{sha256}.bin"))
    }

    pub fn has(&self, sha256: &str) -> bool {
        self.blob_path(sha256).is_file()
    }

    /// Store a payload under its digest. Content-addressed, so re-storing an
    /// identical payload is a no-op rather than a duplicate.
    pub fn store(&self, sha256: &str, bytes: &[u8]) -> Result<PathBuf> {
        let path = self.blob_path(sha256);
        if path.is_file() {
            return Ok(path);
        }
        let parent = path.parent().expect("blob path always has a parent");
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;

        // Write-then-rename so a crash cannot leave a short blob under a
        // digest that claims to describe the whole payload.
        let tmp = path.with_extension("bin.tmp");
        std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("sealing {}", path.display()))?;
        Ok(path)
    }

    pub fn load(&self, sha256: &str) -> Result<Option<Vec<u8>>> {
        let path = self.blob_path(sha256);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn index_path(&self) -> PathBuf {
        self.root.join("index.jsonl")
    }

    /// Append one record. JSONL rather than a single JSON document so the
    /// index stays appendable and streamable as it grows.
    pub fn append(&self, record: &Record) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("creating {}", self.root.display()))?;
        let path = self.index_path();
        let mut line = serde_json::to_string(record).context("serialising index record")?;
        line.push('\n');

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        file.write_all(line.as_bytes())
            .with_context(|| format!("appending to {}", path.display()))?;
        Ok(())
    }

    /// Read the index back. Malformed lines are skipped rather than failing
    /// the whole read, so one bad append cannot make the history unreadable.
    pub fn records(&self) -> Result<Vec<Record>> {
        let path = self.index_path();
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(raw
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis;

    fn temp_root(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("eac-archive-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn record(sha256: &str) -> Record {
        Record {
            seen_at: 1_735_689_600,
            game: "ARC Raiders".into(),
            product_id: "p".into(),
            deployment_id: "d".into(),
            platform: "win64".into(),
            url: "https://example.invalid/p/d/win64".into(),
            size: 3,
            hashes: Hashes {
                md5: "m".into(),
                sha1: "s".into(),
                sha256: sha256.into(),
            },
            tlsh: None,
            format: "unknown".into(),
            entropy: 1.5,
            headers: BTreeMap::from([("etag".into(), "\"abc\"".into())]),
            modules: Vec::new(),
            pe: None,
            previous_sha256: None,
        }
    }

    #[test]
    fn stores_and_loads_blobs_by_digest() {
        let root = temp_root("blobs");
        let archive = Archive::new(&root);
        let body = b"module payload";
        let sha = analysis::hashes(body).sha256;

        assert!(!archive.has(&sha));
        let path = archive.store(&sha, body).unwrap();
        assert!(archive.has(&sha));
        assert_eq!(archive.load(&sha).unwrap().as_deref(), Some(&body[..]));

        // Sharded by the first two hex characters.
        assert!(path.to_string_lossy().contains(&sha[..2]));
        // Re-storing is a no-op, not a duplicate or an error.
        assert_eq!(archive.store(&sha, body).unwrap(), path);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn missing_blobs_read_as_none() {
        let root = temp_root("missing");
        let archive = Archive::new(&root);
        assert!(archive.load(&"ab".repeat(32)).unwrap().is_none());
    }

    #[test]
    fn index_appends_and_round_trips() {
        let root = temp_root("index");
        let archive = Archive::new(&root);
        assert!(
            archive.records().unwrap().is_empty(),
            "absent index is empty"
        );

        archive.append(&record("aaa")).unwrap();
        archive.append(&record("bbb")).unwrap();

        let records = archive.records().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].hashes.sha256, "aaa");
        assert_eq!(records[1].hashes.sha256, "bbb");
        assert_eq!(records[0].headers.get("etag").unwrap(), "\"abc\"");
        assert_eq!(records[0].game, "ARC Raiders");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_corrupt_index_line_does_not_hide_the_rest() {
        let root = temp_root("corrupt");
        let archive = Archive::new(&root);
        archive.append(&record("aaa")).unwrap();

        // Simulate a torn append followed by a good one.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(archive.index_path())
            .unwrap();
        file.write_all(b"{ truncated\n").unwrap();
        drop(file);
        archive.append(&record("ccc")).unwrap();

        let records = archive.records().unwrap();
        assert_eq!(records.len(), 2, "the bad line is skipped, not fatal");
        assert_eq!(records[1].hashes.sha256, "ccc");

        std::fs::remove_dir_all(&root).unwrap();
    }
}

//! What actually changed between two payloads.
//!
//! A new hash tells you to look; this tells you whether it is worth your
//! afternoon. Formatting lives in [`crate::embed`] — this module only computes.

use crate::analysis;
use crate::eac::{ModuleEntry, Snapshot};
use crate::state::Seen;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleChange {
    pub name: String,
    pub old_size: Option<u64>,
    pub new_size: Option<u64>,
}

impl ModuleChange {
    /// Byte growth, when both sides reported a size.
    pub fn size_delta(&self) -> Option<i64> {
        Some(self.new_size? as i64 - self.old_size? as i64)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diff {
    pub size_delta: i64,
    /// TLSH distance: 0 identical, under ~30 a small patch, over ~200
    /// effectively unrelated. `None` when the previous payload is not
    /// archived, or either side is too small to hash.
    pub tlsh_distance: Option<i32>,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<ModuleChange>,
    /// Old and new COFF TimeDateStamp, when both are known and differ.
    pub pe_timestamp: Option<(u32, u32)>,
}

impl Diff {
    /// True when nothing beyond the raw bytes could be characterised. The
    /// embed falls back to just the hashes in that case.
    pub fn is_uninformative(&self) -> bool {
        self.size_delta == 0
            && self.tlsh_distance.is_none()
            && self.added.is_empty()
            && self.removed.is_empty()
            && self.changed.is_empty()
            && self.pe_timestamp.is_none()
    }
}

fn by_name(modules: &[ModuleEntry]) -> BTreeMap<&str, &ModuleEntry> {
    modules.iter().map(|m| (m.name.as_str(), m)).collect()
}

/// Compare a new snapshot against what was last seen.
///
/// `previous_body` comes from the archive. Without it the TLSH distance is
/// unavailable, because tlsh2 cannot rebuild a comparable hash from a string.
pub fn compute(previous: &Seen, snapshot: &Snapshot, previous_body: Option<&[u8]>) -> Diff {
    let old = by_name(&previous.modules);
    let new = by_name(&snapshot.modules);

    let added = new
        .keys()
        .filter(|name| !old.contains_key(*name))
        .map(|name| (*name).to_string())
        .collect();
    let removed = old
        .keys()
        .filter(|name| !new.contains_key(*name))
        .map(|name| (*name).to_string())
        .collect();

    let changed = new
        .iter()
        .filter_map(|(name, entry)| {
            let before = old.get(name)?;
            // A module counts as changed if either its size or its hash moved.
            let moved = before.size != entry.size
                || (before.hash.is_some() && entry.hash.is_some() && before.hash != entry.hash);
            moved.then(|| ModuleChange {
                name: (*name).to_string(),
                old_size: before.size,
                new_size: entry.size,
            })
        })
        .collect();

    let pe_timestamp = match (
        previous.pe_timestamp,
        snapshot.pe.as_ref().map(|p| p.timestamp),
    ) {
        (Some(before), Some(after)) if before != after => Some((before, after)),
        _ => None,
    };

    Diff {
        size_delta: snapshot.size() as i64 - previous.size as i64,
        tlsh_distance: previous_body.and_then(|body| analysis::tlsh_distance(body, &snapshot.body)),
        added,
        removed,
        changed,
        pe_timestamp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::Hashes;
    use std::collections::BTreeMap;

    fn module(name: &str, size: u64, hash: &str) -> ModuleEntry {
        ModuleEntry {
            name: name.into(),
            arch: None,
            size: Some(size),
            hash: Some(hash.into()),
        }
    }

    fn snapshot(body: &[u8], modules: Vec<ModuleEntry>) -> Snapshot {
        Snapshot {
            url: String::new(),
            hashes: Hashes {
                md5: String::new(),
                sha1: String::new(),
                sha256: String::new(),
            },
            headers: BTreeMap::new(),
            etag: None,
            last_modified: None,
            modules,
            format: "unknown".into(),
            entropy: 0.0,
            tlsh: None,
            pe: None,
            body: body.to_vec(),
        }
    }

    fn seen(size: u64, modules: Vec<ModuleEntry>) -> Seen {
        Seen {
            digest: "old".into(),
            size,
            seen_at: 0,
            modules,
            pe_timestamp: None,
        }
    }

    #[test]
    fn reports_added_removed_and_changed_modules() {
        let before = seen(
            100,
            vec![
                module("driver.sys", 1000, "aaaa"),
                module("client.dll", 2000, "bbbb"),
                module("old.dll", 300, "cccc"),
            ],
        );
        let after = snapshot(
            b"body",
            vec![
                module("driver.sys", 1500, "dddd"), // grew
                module("client.dll", 2000, "bbbb"), // untouched
                module("new.sys", 50, "eeee"),      // added
            ],
        );

        let diff = compute(&before, &after, None);
        assert_eq!(diff.added, vec!["new.sys"]);
        assert_eq!(diff.removed, vec!["old.dll"]);
        assert_eq!(diff.changed.len(), 1);
        assert_eq!(diff.changed[0].name, "driver.sys");
        assert_eq!(diff.changed[0].size_delta(), Some(500));
    }

    #[test]
    fn a_hash_change_alone_counts_as_changed() {
        let before = seen(10, vec![module("driver.sys", 1000, "aaaa")]);
        let after = snapshot(b"body", vec![module("driver.sys", 1000, "zzzz")]);

        let diff = compute(&before, &after, None);
        assert_eq!(diff.changed.len(), 1, "same size, different hash");
        assert_eq!(diff.changed[0].size_delta(), Some(0));
    }

    #[test]
    fn identical_modules_produce_no_module_churn() {
        let modules = vec![module("driver.sys", 1000, "aaaa")];
        let before = seen(4, modules.clone());
        let after = snapshot(b"body", modules);

        let diff = compute(&before, &after, None);
        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
        assert!(diff.changed.is_empty());
    }

    #[test]
    fn size_delta_is_signed() {
        let grew = compute(&seen(100, vec![]), &snapshot(&[0u8; 250], vec![]), None);
        assert_eq!(grew.size_delta, 150);

        let shrank = compute(&seen(400, vec![]), &snapshot(&[0u8; 250], vec![]), None);
        assert_eq!(shrank.size_delta, -150);
    }

    #[test]
    fn tlsh_distance_needs_the_archived_payload() {
        let base: Vec<u8> = (0..4096u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        let mut patched = base.clone();
        for b in patched.iter_mut().take(32) {
            *b ^= 0xFF;
        }
        let before = seen(base.len() as u64, vec![]);
        let after = snapshot(&patched, vec![]);

        // Without the previous bytes there is nothing to measure against.
        assert!(compute(&before, &after, None).tlsh_distance.is_none());

        let with_archive = compute(&before, &after, Some(&base));
        let distance = with_archive
            .tlsh_distance
            .expect("archive enables distance");
        assert!(distance > 0, "a patched payload is not identical");
        assert!(distance < 200, "a small patch stays close, got {distance}");
    }

    #[test]
    fn detects_a_rebuild_via_the_coff_timestamp() {
        let mut before = seen(4, vec![]);
        before.pe_timestamp = Some(1000);

        let mut after = snapshot(b"body", vec![]);
        assert!(
            compute(&before, &after, None).pe_timestamp.is_none(),
            "no new timestamp means nothing to compare"
        );

        after.pe = Some(crate::analysis::PeInfo {
            machine: "x64".into(),
            machine_raw: 0x8664,
            timestamp: 2000,
            is_dll: false,
            is_64: true,
            entry: 0,
            image_base: 0,
            subsystem: None,
            pdb_path: None,
            sections: Vec::new(),
            libraries: Vec::new(),
            export_count: 0,
            version: None,
            signature: None,
        });
        assert_eq!(
            compute(&before, &after, None).pe_timestamp,
            Some((1000, 2000))
        );
    }

    #[test]
    fn an_unchanged_payload_is_uninformative() {
        let before = seen(4, vec![]);
        let after = snapshot(b"body", vec![]);
        assert!(compute(&before, &after, None).is_uninformative());
    }
}

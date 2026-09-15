//! Easy Anti-Cheat module CDN client.
//!
//! EAC modules are distributed from an Epic Games endpoint addressed by the
//! game's product id, deployment id and platform:
//!
//! ```text
//! https://modules-cdn.eac-prod.on.epicgames.com/modules/{product_id}/{deployment_id}/{platform}
//! ```
//!
//! ## On parsing
//!
//! The payload format is not publicly documented and Epic has changed it
//! before, so nothing here *depends* on it. Change detection is driven purely
//! by the SHA-256 of the raw response body, which is correct for any format.
//! [`parse_modules`] is a best-effort enrichment pass on top: it tries JSON
//! first and falls back to scanning the blob for embedded module filenames.
//! When both come up empty the update still reports correctly, just without
//! the per-module breakdown.

use anyhow::{Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::time::Duration;

pub const CDN_BASE: &str = "https://modules-cdn.eac-prod.on.epicgames.com/modules";

/// Build the module URL for a target. A trailing slash on `base` is tolerated
/// so a hand-written config override cannot produce a `//` path.
pub fn module_url_with_base(
    base: &str,
    product_id: &str,
    deployment_id: &str,
    platform: &str,
) -> String {
    let base = base.trim_end_matches('/');
    format!("{base}/{product_id}/{deployment_id}/{platform}")
}

/// One module named inside a CDN response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleEntry {
    pub name: String,
    pub arch: Option<String>,
    pub size: Option<u64>,
    pub hash: Option<String>,
}

/// A single fetch of one target.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub url: String,
    pub body: Vec<u8>,
    /// Full SHA-256 of `body`, lowercase hex. The change-detection signal.
    pub digest: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub modules: Vec<ModuleEntry>,
}

impl Snapshot {
    pub fn size(&self) -> u64 {
        self.body.len() as u64
    }

    /// First 16 hex chars of the digest — what the embed displays.
    pub fn short_digest(&self) -> &str {
        short_hash(&self.digest)
    }
}

/// Truncate a hex hash for display, matching the compact form used in embeds.
pub fn short_hash(hash: &str) -> &str {
    let end = hash.char_indices().nth(16).map_or(hash.len(), |(i, _)| i);
    &hash[..end]
}

pub struct Client {
    http: reqwest::Client,
    base: String,
}

impl Client {
    /// Build a client against a non-default base, for mirrors and tests.
    pub fn with_base(base: &str, user_agent: &str, timeout: Duration) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(user_agent)
            .timeout(timeout)
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_string(),
        })
    }

    /// Fetch one target and digest it.
    pub async fn fetch(
        &self,
        product_id: &str,
        deployment_id: &str,
        platform: &str,
    ) -> Result<Snapshot> {
        let url = module_url_with_base(&self.base, product_id, deployment_id, platform);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("requesting {url}"))?;

        let status = resp.status();
        let header = |name: reqwest::header::HeaderName| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let etag = header(reqwest::header::ETAG);
        let last_modified = header(reqwest::header::LAST_MODIFIED);

        let body = resp
            .bytes()
            .await
            .with_context(|| format!("reading body of {url}"))?
            .to_vec();

        if !status.is_success() {
            anyhow::bail!("{url} returned HTTP {status}");
        }

        let digest = hex::encode(Sha256::digest(&body));
        let modules = parse_modules(&body);

        Ok(Snapshot {
            url,
            body,
            digest,
            etag,
            last_modified,
            modules,
        })
    }
}

/// Best-effort extraction of module entries from a CDN response body.
pub fn parse_modules(body: &[u8]) -> Vec<ModuleEntry> {
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        let mut found = Vec::new();
        collect_json_modules(&value, None, &mut found);
        if !found.is_empty() {
            dedupe(&mut found);
            return found;
        }
    }
    scan_for_filenames(body)
}

const NAME_KEYS: &[&str] = &["name", "file", "filename", "module", "path", "moduleName"];
const SIZE_KEYS: &[&str] = &[
    "size",
    "length",
    "bytes",
    "fileSize",
    "sizeBytes",
    "contentLength",
];
const HASH_KEYS: &[&str] = &["hash", "sha256", "sha1", "checksum", "digest", "md5"];
const ARCH_KEYS: &[&str] = &["arch", "architecture", "cpu", "platform", "target"];

/// Walk arbitrary JSON collecting anything that looks like a module record.
///
/// `key_hint` carries the object's own key from the parent, so a payload shaped
/// as `{"driver.sys": {"size": 123}}` is recognised as well as the more usual
/// `[{"name": "driver.sys", "size": 123}]`.
fn collect_json_modules(value: &Value, key_hint: Option<&str>, out: &mut Vec<ModuleEntry>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_json_modules(item, None, out);
            }
        }
        Value::Object(map) => {
            let name = NAME_KEYS
                .iter()
                .find_map(|k| map.get(*k).and_then(Value::as_str))
                .map(str::to_owned)
                .or_else(|| key_hint.filter(|h| looks_like_module(h)).map(str::to_owned));

            let size = SIZE_KEYS.iter().find_map(|k| map.get(*k).and_then(as_u64));
            let hash = HASH_KEYS
                .iter()
                .find_map(|k| map.get(*k).and_then(Value::as_str))
                .map(str::to_owned);

            // A record needs a name plus at least one corroborating field,
            // otherwise every nested object with a "name" gets swept up.
            if let Some(name) = name
                && (size.is_some() || hash.is_some())
            {
                out.push(ModuleEntry {
                    arch: ARCH_KEYS
                        .iter()
                        .find_map(|k| map.get(*k).and_then(Value::as_str))
                        .map(str::to_owned),
                    name,
                    size,
                    hash,
                });
            }

            for (k, v) in map {
                collect_json_modules(v, Some(k.as_str()), out);
            }
        }
        _ => {}
    }
}

/// JSON numbers are sometimes serialised as strings; accept both.
fn as_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
}

const MODULE_EXTENSIONS: &[&str] = &[".sys", ".dll", ".exe", ".so", ".dylib", ".bin"];

fn looks_like_module(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    MODULE_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
}

/// Fallback for non-JSON payloads: pull printable ASCII runs that look like
/// module filenames out of the blob.
fn scan_for_filenames(body: &[u8]) -> Vec<ModuleEntry> {
    let mut out = Vec::new();
    let mut run = String::new();

    let flush = |run: &mut String, out: &mut Vec<ModuleEntry>| {
        for token in run.split(['\\', '/']) {
            if token.len() >= 5 && looks_like_module(token) {
                out.push(ModuleEntry {
                    name: token.to_owned(),
                    arch: None,
                    size: None,
                    hash: None,
                });
            }
        }
        run.clear();
    };

    for &byte in body {
        // Printable ASCII, plus the path separators we split on above.
        if byte.is_ascii_graphic() {
            run.push(byte as char);
            if run.len() > 512 {
                flush(&mut run, &mut out);
            }
        } else {
            flush(&mut run, &mut out);
        }
    }
    flush(&mut run, &mut out);

    dedupe(&mut out);
    out
}

/// Drop repeats by name, keeping the first (richest) occurrence.
fn dedupe(entries: &mut Vec<ModuleEntry>) {
    let mut seen = std::collections::HashSet::new();
    entries.retain(|e| seen.insert(e.name.to_ascii_lowercase()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_the_documented_url() {
        assert_eq!(
            module_url_with_base(
                CDN_BASE,
                "9e8b37541e614575b4de303d2c2e44cf",
                "35e06571d8ab4de4b98519b624125459",
                "win64"
            ),
            "https://modules-cdn.eac-prod.on.epicgames.com/modules/\
             9e8b37541e614575b4de303d2c2e44cf/35e06571d8ab4de4b98519b624125459/win64"
        );
    }

    #[test]
    fn tolerates_a_trailing_slash_on_the_base() {
        assert_eq!(
            module_url_with_base("https://mirror.example/modules/", "p", "d", "win64"),
            "https://mirror.example/modules/p/d/win64"
        );
    }

    #[test]
    fn short_hash_truncates_to_sixteen() {
        assert_eq!(short_hash("d6b8cbf936b39c5200000000"), "d6b8cbf936b39c52");
        assert_eq!(short_hash("abc"), "abc");
    }

    #[test]
    fn parses_an_array_of_records() {
        let body = br#"{"modules":[
            {"name":"driver.sys","arch":"arm64","size":17301504,"hash":"bc1f4446a7008207"},
            {"name":"client.dll","arch":"x64","size":20971520,"hash":"272d0e577de143a0"}
        ]}"#;
        let modules = parse_modules(body);
        assert_eq!(modules.len(), 2);
        assert_eq!(modules[0].name, "driver.sys");
        assert_eq!(modules[0].arch.as_deref(), Some("arm64"));
        assert_eq!(modules[0].size, Some(17301504));
        assert_eq!(modules[1].hash.as_deref(), Some("272d0e577de143a0"));
    }

    #[test]
    fn parses_a_filename_keyed_map() {
        let body = br#"{"usermode.exe":{"size":"1048576","sha256":"b289b022c438cbf6"}}"#;
        let modules = parse_modules(body);
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].name, "usermode.exe");
        assert_eq!(modules[0].size, Some(1048576));
    }

    #[test]
    fn ignores_objects_that_only_have_a_name() {
        let body = br#"{"deployment":{"name":"prod"},"unrelated":{"name":"whatever"}}"#;
        assert!(parse_modules(body).is_empty());
    }

    #[test]
    fn falls_back_to_scanning_binary_blobs() {
        let mut body = vec![0x00, 0xFF, 0x13];
        body.extend_from_slice(b"C:\\eac\\driver.sys");
        body.push(0x00);
        body.extend_from_slice(b"client.dll");
        body.extend_from_slice(&[0x00, 0x01]);
        body.extend_from_slice(b"client.dll"); // duplicate
        let modules = parse_modules(&body);
        let names: Vec<_> = modules.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["driver.sys", "client.dll"]);
    }

    #[test]
    fn scanning_skips_short_and_non_module_tokens() {
        let modules = parse_modules(b"a.so readme.txt notes");
        assert!(modules.is_empty(), "got {modules:?}");
    }
}

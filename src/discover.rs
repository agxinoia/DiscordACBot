//! Find EAC product and deployment ids in installed games.
//!
//! Both ids live in the game's own EasyAntiCheat configuration. Rather than
//! depending on a particular filename — Epic has moved and renamed these — this
//! walks a directory and pulls the values out of any small text file that
//! mentions them, which works for the JSON and INI shapes alike.
//!
//! Runs anywhere the files are readable: a native Linux install, a Proton
//! prefix, or a Windows drive mounted on the server.

use serde::Serialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Config files are small; anything larger is not what we are looking for and
/// is skipped rather than read into memory.
const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// Guard against a pathological tree (or a symlink loop we failed to spot).
const MAX_FILES_SCANNED: usize = 500_000;

const EXTENSIONS: &[&str] = &["json", "ini", "cfg", "txt"];

const PRODUCT_KEYS: &[&str] = &["productid", "product_id"];
const DEPLOYMENT_KEYS: &[&str] = &["deploymentid", "deployment_id"];
const SANDBOX_KEYS: &[&str] = &["sandboxid", "sandbox_id"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    /// Best guess at the game's name, taken from the install path.
    pub game: String,
    pub product_id: String,
    pub deployment_id: String,
    pub sandbox_id: Option<String>,
    pub source: PathBuf,
    /// Platforms that answered a probe. Empty unless probing was requested.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub platforms: Vec<String>,
}

impl Finding {
    /// The command to paste into Discord.
    pub fn command(&self) -> String {
        let platforms = if self.platforms.is_empty() {
            crate::eac::DEFAULT_PLATFORM.to_string()
        } else {
            self.platforms.join(", ")
        };
        format!(
            "/eac add game:{} product_id:{} deployment_id:{} platforms:{}",
            self.game, self.product_id, self.deployment_id, platforms
        )
    }
}

/// Where Steam keeps games on Linux, plus the usual Flatpak location.
pub fn default_roots() -> Vec<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    [
        ".steam/steam/steamapps/common",
        ".steam/root/steamapps/common",
        ".local/share/Steam/steamapps/common",
        ".var/app/com.valvesoftware.Steam/.local/share/Steam/steamapps/common",
    ]
    .iter()
    .map(|suffix| Path::new(&home).join(suffix))
    .filter(|p| p.is_dir())
    .collect()
}

/// Walk `root` to `max_depth`, returning every distinct id pair found.
pub fn scan(root: &Path, max_depth: usize) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut scanned = 0usize;
    walk(root, 0, max_depth, &mut scanned, &mut findings, &mut seen);
    findings
}

fn walk(
    dir: &Path,
    depth: usize,
    max_depth: usize,
    scanned: &mut usize,
    findings: &mut Vec<Finding>,
    seen: &mut BTreeSet<(String, String)>,
) {
    if depth > max_depth || *scanned >= MAX_FILES_SCANNED {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        // An unreadable directory is normal on a mounted drive; skip it.
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        // symlink_metadata so a symlink loop cannot trap the walk.
        let Ok(meta) = entry.path().symlink_metadata() else {
            continue;
        };
        if meta.is_symlink() {
            continue;
        }
        if meta.is_dir() {
            walk(&path, depth + 1, max_depth, scanned, findings, seen);
            continue;
        }
        if !meta.is_file() || meta.len() > MAX_FILE_BYTES || meta.len() == 0 {
            continue;
        }
        let matches_extension = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| EXTENSIONS.iter().any(|want| e.eq_ignore_ascii_case(want)));
        if !matches_extension {
            continue;
        }

        *scanned += 1;
        if let Some(finding) = inspect(&path)
            && seen.insert((finding.product_id.clone(), finding.deployment_id.clone()))
        {
            findings.push(finding);
        }
    }
}

/// Pull an id pair out of one file, if it has both.
pub fn inspect(path: &Path) -> Option<Finding> {
    let text = std::fs::read_to_string(path).ok()?;
    let product_id = find_value(&text, PRODUCT_KEYS)?;
    let deployment_id = find_value(&text, DEPLOYMENT_KEYS)?;

    Some(Finding {
        game: infer_game_name(path),
        product_id,
        deployment_id,
        sandbox_id: find_value(&text, SANDBOX_KEYS),
        source: path.to_path_buf(),
        platforms: Vec::new(),
    })
}

fn find_value(text: &str, keys: &[&str]) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    for key in keys {
        let mut from = 0;
        while let Some(offset) = lower[from..].find(key) {
            let after = from + offset + key.len();
            if let Some(value) = read_value(&text[after..]) {
                return Some(value);
            }
            from = after;
        }
    }
    None
}

/// Read the value following a key, skipping the punctuation that separates
/// them in JSON (`": "`) and INI (`=`) alike.
fn read_value(rest: &str) -> Option<String> {
    // Bound the skip so an empty value cannot swallow the next key.
    const SKIP_WINDOW: usize = 24;

    let mut start = None;
    for (i, c) in rest.char_indices() {
        if i > SKIP_WINDOW {
            return None;
        }
        if c.is_ascii_alphanumeric() {
            start = Some(i);
            break;
        }
        if !matches!(c, '"' | '\'' | ':' | '=' | ' ' | '\t') {
            return None;
        }
    }
    let start = start?;
    let end = rest[start..]
        .find(|c: char| !c.is_ascii_alphanumeric())
        .map_or(rest.len(), |e| start + e);

    let value = &rest[start..end];
    (8..=64).contains(&value.len()).then(|| value.to_string())
}

/// Guess the game's name from where the file sits.
///
/// Steam libraries put it directly under `steamapps/common`; otherwise the
/// nearest ancestor that is not an EasyAntiCheat folder is the best signal.
fn infer_game_name(path: &Path) -> String {
    let components: Vec<String> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();

    if let Some(i) = components
        .iter()
        .position(|c| c.eq_ignore_ascii_case("common"))
        && let Some(name) = components.get(i + 1)
    {
        return name.clone();
    }

    components
        .iter()
        .rev()
        .skip(1) // the file itself
        .find(|c| {
            let lower = c.to_ascii_lowercase();
            !lower.starts_with("easyanticheat") && !lower.is_empty() && *c != "/"
        })
        .cloned()
        .unwrap_or_else(|| "Unknown Game".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("eac-discover-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    const SETTINGS_JSON: &str = r#"{
        "productid": "9e8b37541e614575b4de303d2c2e44cf",
        "sandboxid": "fe0e4f8b1f2e4a1b9c0d8e7f6a5b4c3d",
        "deploymentid": "35e06571d8ab4de4b98519b624125459",
        "clientid": "xyz"
    }"#;

    #[test]
    fn reads_ids_from_a_json_settings_file() {
        let root = temp_dir("json");
        let file = root.join("steamapps/common/ARC Raiders/EasyAntiCheat_EOS/Settings.json");
        write(&file, SETTINGS_JSON);

        let found = scan(&root, 8);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].product_id, "9e8b37541e614575b4de303d2c2e44cf");
        assert_eq!(found[0].deployment_id, "35e06571d8ab4de4b98519b624125459");
        assert_eq!(
            found[0].sandbox_id.as_deref(),
            Some("fe0e4f8b1f2e4a1b9c0d8e7f6a5b4c3d")
        );
        assert_eq!(found[0].game, "ARC Raiders", "name comes from the path");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn reads_ids_from_an_ini_style_file() {
        let root = temp_dir("ini");
        let file = root.join("Games/SomeGame/EasyAntiCheat/settings.ini");
        write(
            &file,
            "[EOS]\nProductId=9e8b37541e614575b4de303d2c2e44cf\nDeploymentId = 35e06571d8ab4de4b98519b624125459\n",
        );

        let found = scan(&root, 8);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].product_id, "9e8b37541e614575b4de303d2c2e44cf");
        assert_eq!(found[0].game, "SomeGame", "EasyAntiCheat dir is skipped");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn handles_snake_case_keys() {
        let root = temp_dir("snake");
        write(
            &root.join("g/eac.json"),
            r#"{"product_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","deployment_id":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}"#,
        );
        let found = scan(&root, 8);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].product_id, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_same_pair_in_several_files_is_reported_once() {
        let root = temp_dir("dedupe");
        write(&root.join("a/Settings.json"), SETTINGS_JSON);
        write(&root.join("b/Settings.json"), SETTINGS_JSON);
        write(&root.join("c/backup.txt"), SETTINGS_JSON);

        assert_eq!(scan(&root, 8).len(), 1);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn files_without_both_ids_are_ignored() {
        let root = temp_dir("partial");
        // Only a product id — not enough to build a URL.
        write(
            &root.join("a/Settings.json"),
            r#"{"productid":"9e8b37541e614575b4de303d2c2e44cf"}"#,
        );
        write(&root.join("b/other.json"), r#"{"name":"nothing to see"}"#);

        assert!(scan(&root, 8).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_empty_value_does_not_capture_the_next_key() {
        let root = temp_dir("empty");
        write(
            &root.join("a/Settings.json"),
            r#"{"productid":"","deploymentid":"35e06571d8ab4de4b98519b624125459"}"#,
        );
        assert!(
            scan(&root, 8).is_empty(),
            "a blank product id must not borrow the deployment id"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn irrelevant_extensions_and_large_files_are_skipped() {
        let root = temp_dir("skip");
        write(&root.join("a/Settings.dat"), SETTINGS_JSON);
        write(&root.join("b/Settings.json"), &"x".repeat(2 * 1024 * 1024));

        assert!(scan(&root, 8).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn depth_is_bounded() {
        let root = temp_dir("depth");
        write(&root.join("a/b/c/d/Settings.json"), SETTINGS_JSON);

        assert!(scan(&root, 2).is_empty(), "too deep to reach");
        assert_eq!(scan(&root, 8).len(), 1);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn renders_a_pasteable_command() {
        let mut finding = Finding {
            game: "ARC Raiders".into(),
            product_id: "p".into(),
            deployment_id: "d".into(),
            sandbox_id: None,
            source: PathBuf::new(),
            platforms: Vec::new(),
        };
        // Every deployment measured so far publishes win64, so that is the
        // default when probing has not narrowed it down.
        assert!(
            finding.command().ends_with("platforms:win64"),
            "got {}",
            finding.command()
        );

        finding.platforms = vec!["win64".into(), "winarm_x64_x64".into()];
        assert!(
            finding
                .command()
                .ends_with("platforms:win64, winarm_x64_x64")
        );
    }

    #[test]
    fn a_missing_root_yields_nothing_rather_than_failing() {
        assert!(scan(Path::new("/definitely/not/here"), 4).is_empty());
    }
}

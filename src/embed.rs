//! Embed rendering and attachment chunking.

use crate::analysis::{PeInfo, SignatureInfo, VersionInfo};
use crate::config::Game;
use crate::diff::Diff;
use crate::eac::{ModuleEntry, Snapshot, short_hash};
use serenity::builder::{CreateEmbed, CreateEmbedFooter};
use serenity::model::Timestamp;

/// Teal accent bar on the left of the embed.
const ACCENT: u32 = 0x00B5C4;
const MUTED: u32 = 0x4F545C;

/// Discord's hard limits.
const MAX_FIELDS: usize = 25;
const MAX_FIELD_VALUE: usize = 1024;
const MAX_FIELD_NAME: usize = 256;

/// One rendered embed field.
type Field = (String, String, bool);

/// Format a byte count the way the embed shows it (1024-based, 1 decimal).
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = UNITS[0];
    for next in &UNITS[1..] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    format!("{value:.1} {unit}")
}

/// Signed byte delta, e.g. `+412.0 KB`.
pub fn signed_bytes(delta: i64) -> String {
    match delta {
        0 => "no change".to_string(),
        d if d > 0 => format!("+{}", human_bytes(d as u64)),
        d => format!("-{}", human_bytes(d.unsigned_abs())),
    }
}

/// Plain-language reading of a TLSH distance, so the number means something
/// without having to remember the scale.
pub fn tlsh_verdict(distance: i32) -> &'static str {
    match distance {
        0 => "identical",
        d if d < 30 => "small patch",
        d if d < 100 => "substantial change",
        d if d < 200 => "major rewrite",
        _ => "effectively unrelated",
    }
}

/// Number of attachment parts `total` bytes will be split into.
pub fn part_count(total: u64, max_part: u64) -> u64 {
    if total == 0 || max_part == 0 {
        return 0;
    }
    total.div_ceil(max_part)
}

/// Split a body into upload-sized chunks.
pub fn split_parts(body: &[u8], max_part: u64) -> Vec<&[u8]> {
    if body.is_empty() || max_part == 0 {
        return Vec::new();
    }
    body.chunks(max_part as usize).collect()
}

/// Human-readable summary of how the raw response will be attached.
pub fn raw_response_summary(total: u64, max_part: u64) -> String {
    if total == 0 {
        return "Empty response.".to_string();
    }
    let parts = part_count(total, max_part);
    if parts <= 1 {
        return format!("{} attached in full.", human_bytes(total));
    }
    format!(
        "{} split into {} part(s), each under {}.",
        human_bytes(total),
        parts,
        human_bytes(max_part)
    )
}

fn module_field(module: &ModuleEntry) -> Field {
    let name = match &module.arch {
        Some(arch) if !arch.is_empty() => format!("{} ({})", module.name, arch),
        _ => module.name.clone(),
    };
    let mut value = match module.size {
        Some(size) => human_bytes(size),
        None => "size unknown".to_string(),
    };
    if let Some(hash) = &module.hash {
        value.push_str(&format!("\n`{}`", short_hash(hash)));
    }
    (name, value, true)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let end = s
        .char_indices()
        .take_while(|(i, c)| i + c.len_utf8() <= max.saturating_sub(1))
        .last()
        .map_or(0, |(i, c)| i + c.len_utf8());
    format!("{}…", &s[..end])
}

/// Join lines, dropping any that would overflow a field value.
fn join_capped(lines: Vec<String>) -> String {
    let mut out = String::new();
    for line in lines {
        if out.len() + line.len() + 1 > MAX_FIELD_VALUE {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&line);
    }
    out
}

/// What moved since the previous payload.
fn diff_field(diff: &Diff) -> Option<Field> {
    if diff.is_uninformative() {
        return None;
    }
    let mut lines = vec![format!("Size: {}", signed_bytes(diff.size_delta))];

    if let Some(distance) = diff.tlsh_distance {
        lines.push(format!(
            "TLSH distance: {distance} ({})",
            tlsh_verdict(distance)
        ));
    }
    if !diff.added.is_empty() || !diff.removed.is_empty() || !diff.changed.is_empty() {
        lines.push(format!(
            "Modules: {} added, {} removed, {} changed",
            diff.added.len(),
            diff.removed.len(),
            diff.changed.len()
        ));
    }
    if let Some((before, after)) = diff.pe_timestamp {
        lines.push(format!("Rebuilt: <t:{before}:f> → <t:{after}:f>"));
    }

    // Name the individual modules, newest information first.
    for name in diff.added.iter().take(5) {
        lines.push(format!("`+ {name}`"));
    }
    for name in diff.removed.iter().take(5) {
        lines.push(format!("`- {name}`"));
    }
    for change in diff.changed.iter().take(8) {
        match change.size_delta() {
            Some(d) => lines.push(format!("`~ {} ({})`", change.name, signed_bytes(d))),
            None => lines.push(format!("`~ {}`", change.name)),
        }
    }

    Some(("Changes".to_string(), join_capped(lines), false))
}

fn version_field(version: &VersionInfo) -> Option<Field> {
    let mut lines = Vec::new();
    if let Some(v) = &version.file_version {
        lines.push(format!("File: `{v}`"));
    }
    if let Some(v) = &version.product_version {
        lines.push(format!("Product: `{v}`"));
    }
    for (key, value) in &version.strings {
        // The two numeric versions are already shown above.
        if key != "FileVersion" && key != "ProductVersion" {
            lines.push(format!("{key}: {value}"));
        }
    }
    (!lines.is_empty()).then(|| ("Version".to_string(), join_capped(lines), false))
}

fn signature_field(signature: &SignatureInfo) -> Field {
    let mut lines = vec![format!(
        "{} entr{}, {}",
        signature.entry_count,
        if signature.entry_count == 1 {
            "y"
        } else {
            "ies"
        },
        human_bytes(signature.total_bytes as u64)
    )];
    for cert in signature.certificates.iter().take(4) {
        lines.push(format!(
            "`{}`\nvalid <t:{}:D> → <t:{}:D>",
            truncate(&cert.subject, 180),
            cert.not_before,
            cert.not_after
        ));
    }
    ("Signature".to_string(), join_capped(lines), false)
}

/// PE header facts, richest first. A COFF timestamp and a PDB path are the
/// two fields most likely to correlate a build against other artifacts.
fn pe_fields(pe: &PeInfo) -> Vec<Field> {
    let mut fields = Vec::new();

    let mut build = vec![format!(
        "`{}` {} · built <t:{}:f>",
        pe.machine,
        if pe.is_dll { "DLL" } else { "EXE" },
        pe.timestamp
    )];
    if let Some(subsystem) = pe.subsystem {
        build.push(format!("Subsystem: {subsystem}"));
    }
    if pe.export_count > 0 {
        build.push(format!("Exports: {}", pe.export_count));
    }
    fields.push(("Build".to_string(), join_capped(build), false));

    if let Some(pdb) = &pe.pdb_path {
        fields.push((
            "PDB Path".to_string(),
            format!("`{}`", truncate(pdb, 900)),
            false,
        ));
    }
    if let Some(version) = &pe.version {
        fields.extend(version_field(version));
    }
    if let Some(signature) = &pe.signature {
        fields.push(signature_field(signature));
    }
    if !pe.sections.is_empty() {
        let lines: Vec<String> = pe
            .sections
            .iter()
            .take(10)
            .map(|s| {
                format!(
                    "`{:<8} {:>9}  entropy {:.2}`",
                    s.name,
                    human_bytes(s.raw_size as u64),
                    s.entropy
                )
            })
            .collect();
        fields.push(("Sections".to_string(), join_capped(lines), false));
    }
    if !pe.libraries.is_empty() {
        let libs: Vec<String> = pe
            .libraries
            .iter()
            .take(20)
            .map(|l| format!("`{l}`"))
            .collect();
        fields.push((
            "Imports".to_string(),
            truncate(&libs.join(", "), MAX_FIELD_VALUE),
            false,
        ));
    }
    fields
}

/// Every field for a payload, in priority order. Callers truncate to the
/// embed's field budget.
fn payload_fields(
    game: &Game,
    platform: &str,
    snapshot: &Snapshot,
    diff: Option<&Diff>,
    max_part: u64,
    attach_raw: bool,
) -> Vec<Field> {
    let mut fields = vec![
        ("Game".to_string(), game.name.clone(), true),
        ("Platform".to_string(), format!("`{platform}`"), true),
        ("Download".to_string(), human_bytes(snapshot.size()), true),
        (
            "SHA-256".to_string(),
            format!("`{}`", snapshot.hashes.sha256),
            false,
        ),
        (
            "MD5 / SHA-1".to_string(),
            format!("`{}`\n`{}`", snapshot.hashes.md5, snapshot.hashes.sha1),
            false,
        ),
    ];

    let mut payload = vec![format!(
        "{} · entropy {:.2}",
        snapshot.format, snapshot.entropy
    )];
    if let Some(tlsh) = &snapshot.tlsh {
        payload.push(format!("`{tlsh}`"));
    }
    fields.push(("Payload".to_string(), join_capped(payload), false));

    if attach_raw {
        fields.push((
            "Raw Response".to_string(),
            raw_response_summary(snapshot.size(), max_part),
            false,
        ));
    }
    if let Some(field) = diff.and_then(diff_field) {
        fields.push(field);
    }
    if let Some(pe) = &snapshot.pe {
        fields.extend(pe_fields(pe));
    }
    fields.extend(snapshot.modules.iter().map(module_field));
    fields
}

/// Apply fields to an embed, respecting Discord's caps.
fn apply(mut embed: CreateEmbed, fields: Vec<Field>) -> CreateEmbed {
    let total = fields.len();
    let budget = if total > MAX_FIELDS {
        MAX_FIELDS - 1 // leave room for the overflow note
    } else {
        MAX_FIELDS
    };

    for (name, value, inline) in fields.into_iter().take(budget) {
        // A field with an empty value is rejected outright by Discord.
        if value.trim().is_empty() {
            continue;
        }
        embed = embed.field(
            truncate(&name, MAX_FIELD_NAME),
            truncate(&value, MAX_FIELD_VALUE),
            inline,
        );
    }
    if total > budget {
        embed = embed.field("…", format!("and {} more field(s)", total - budget), false);
    }
    embed
}

/// Surface whatever cache validators the CDN returned; they are useful for
/// corroborating an update but are not what the change was detected from.
fn cache_footer(snapshot: &Snapshot) -> String {
    let mut bits = Vec::new();
    if let Some(lm) = &snapshot.last_modified {
        bits.push(format!("Last-Modified: {lm}"));
    }
    if let Some(etag) = &snapshot.etag {
        bits.push(format!("ETag: {etag}"));
    }
    if bits.is_empty() {
        "Detected by hashing the raw CDN response".to_string()
    } else {
        bits.join(" \u{b7} ")
    }
}

/// The embed posted when a target's digest changes.
#[allow(clippy::too_many_arguments)]
pub fn build_update_embed(
    game: &Game,
    platform: &str,
    snapshot: &Snapshot,
    previous: Option<&str>,
    diff: Option<&Diff>,
    max_part: u64,
    attach_raw: bool,
    thumbnail: Option<&str>,
) -> CreateEmbed {
    let mut embed = CreateEmbed::new()
        .title(format!("EAC Update Detected for *{}*", game.name))
        .url(&snapshot.url)
        .colour(ACCENT)
        .timestamp(Timestamp::now());

    if let Some(url) = thumbnail.filter(|u| !u.is_empty()) {
        embed = embed.thumbnail(url);
    }

    let mut fields = payload_fields(game, platform, snapshot, diff, max_part, attach_raw);
    if let Some(previous) = previous {
        fields.push((
            "Previous Hash".to_string(),
            format!("`{}`", short_hash(previous)),
            false,
        ));
    }

    apply(embed, fields).footer(CreateEmbedFooter::new(truncate(
        &cache_footer(snapshot),
        2048,
    )))
}

/// The embed returned by an on-demand `/eac check`, which reports the current
/// state whether or not anything changed.
pub fn build_check_embed(
    game: &Game,
    platform: &str,
    snapshot: &Snapshot,
    changed: bool,
    diff: Option<&Diff>,
    max_part: u64,
) -> CreateEmbed {
    let title = if changed {
        format!("EAC Update Detected for *{}*", game.name)
    } else {
        format!("No change for *{}*", game.name)
    };
    let embed = CreateEmbed::new()
        .title(title)
        .url(&snapshot.url)
        .colour(if changed { ACCENT } else { MUTED })
        .timestamp(Timestamp::now());

    apply(
        embed,
        payload_fields(game, platform, snapshot, diff, max_part, false),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::ModuleChange;

    #[test]
    fn formats_byte_counts() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(22 * 1024 * 1024), "22.0 MB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn formats_signed_deltas() {
        assert_eq!(signed_bytes(0), "no change");
        assert_eq!(signed_bytes(1536), "+1.5 KB");
        assert_eq!(signed_bytes(-1536), "-1.5 KB");
        // i64::MIN must not panic on negation.
        assert!(signed_bytes(i64::MIN).starts_with('-'));
    }

    #[test]
    fn reads_tlsh_distances_in_plain_language() {
        assert_eq!(tlsh_verdict(0), "identical");
        assert_eq!(tlsh_verdict(27), "small patch");
        assert_eq!(tlsh_verdict(150), "major rewrite");
        assert_eq!(tlsh_verdict(400), "effectively unrelated");
    }

    #[test]
    fn counts_upload_parts() {
        assert_eq!(part_count(0, 9_500_000), 0);
        assert_eq!(part_count(100, 9_500_000), 1);
        assert_eq!(part_count(9_500_000, 9_500_000), 1);
        assert_eq!(part_count(9_500_001, 9_500_000), 2);
        assert_eq!(part_count(23_068_672, 9_500_000), 3);
    }

    #[test]
    fn splits_body_into_parts_that_rejoin() {
        let body: Vec<u8> = (0..=255u8).cycle().take(25_000).collect();
        let parts = split_parts(&body, 10_000);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[2].len(), 5_000);
        assert_eq!(parts.concat(), body);
    }

    #[test]
    fn summarises_the_raw_response() {
        assert_eq!(
            raw_response_summary(23_068_672, 9_500_000),
            "22.0 MB split into 3 part(s), each under 9.1 MB."
        );
        assert_eq!(
            raw_response_summary(1024, 9_500_000),
            "1.0 KB attached in full."
        );
        assert_eq!(raw_response_summary(0, 9_500_000), "Empty response.");
    }

    #[test]
    fn renders_module_fields() {
        let with_arch = ModuleEntry {
            name: "driver.sys".into(),
            arch: Some("arm64".into()),
            size: Some(17_301_504),
            hash: Some("bc1f4446a7008207deadbeef".into()),
        };
        let (name, value, inline) = module_field(&with_arch);
        assert_eq!(name, "driver.sys (arm64)");
        assert_eq!(value, "16.5 MB\n`bc1f4446a7008207`");
        assert!(inline);

        let bare = ModuleEntry {
            name: "client.dll".into(),
            arch: None,
            size: None,
            hash: None,
        };
        let (name, value, _) = module_field(&bare);
        assert_eq!(name, "client.dll");
        assert_eq!(value, "size unknown");
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(truncate("abcdef", 4), "abc…");
        let s = "ééééé";
        let out = truncate(s, 5);
        assert!(out.ends_with('…'));
        assert!(s.starts_with(out.trim_end_matches('…')));
    }

    #[test]
    fn join_capped_never_exceeds_a_field_value() {
        let lines: Vec<String> = (0..500).map(|i| format!("line number {i}")).collect();
        let joined = join_capped(lines);
        assert!(joined.len() <= MAX_FIELD_VALUE, "got {}", joined.len());
        assert!(joined.starts_with("line number 0"));
    }

    #[test]
    fn diff_field_describes_what_moved() {
        let diff = Diff {
            size_delta: 421_888,
            tlsh_distance: Some(27),
            added: vec!["new.sys".into()],
            removed: vec!["old.dll".into()],
            changed: vec![ModuleChange {
                name: "driver.sys".into(),
                old_size: Some(1000),
                new_size: Some(1500),
            }],
            pe_timestamp: Some((1000, 2000)),
        };
        let (name, value, _) = diff_field(&diff).expect("an informative diff renders");
        assert_eq!(name, "Changes");
        assert!(value.contains("+412.0 KB"), "got {value}");
        assert!(
            value.contains("TLSH distance: 27 (small patch)"),
            "got {value}"
        );
        assert!(
            value.contains("1 added, 1 removed, 1 changed"),
            "got {value}"
        );
        assert!(value.contains("+ new.sys"), "got {value}");
        assert!(value.contains("- old.dll"), "got {value}");
        assert!(value.contains("~ driver.sys (+500 B)"), "got {value}");
        assert!(
            value.contains("<t:1000:f>"),
            "rebuild times render, got {value}"
        );
    }

    #[test]
    fn an_uninformative_diff_renders_no_field() {
        assert!(diff_field(&Diff::default()).is_none());
    }

    #[test]
    fn payload_fields_stay_within_discord_limits() {
        // A payload with far more modules than the embed can hold.
        let modules: Vec<String> = (0..80)
            .map(|i| format!(r#"{{"name":"mod{i}.dll","size":{i},"hash":"aa{i:02}"}}"#))
            .collect();
        let body = format!(r#"{{"modules":[{}]}}"#, modules.join(","));
        let snapshot = Snapshot::for_test(body.as_bytes());
        assert!(snapshot.modules.len() > MAX_FIELDS);

        let game = Game {
            name: "ARC Raiders".into(),
            product_id: "p".into(),
            deployment_id: "d".into(),
            platforms: vec!["win64".into()],
        };
        let fields = payload_fields(&game, "win64", &snapshot, None, 9_500_000, true);

        for (name, value, _) in &fields {
            assert!(name.len() <= MAX_FIELD_NAME, "field name too long: {name}");
            assert!(
                value.len() <= MAX_FIELD_VALUE,
                "field value too long ({}): {name}",
                value.len()
            );
            assert!(!value.trim().is_empty(), "empty value for {name}");
        }
        // Everything is offered; `apply` is what trims to the budget.
        assert!(fields.len() > MAX_FIELDS);
    }

    #[test]
    fn full_hashes_are_shown_not_truncated() {
        let snapshot = Snapshot::for_test(b"payload");
        let game = Game {
            name: "g".into(),
            product_id: "p".into(),
            deployment_id: "d".into(),
            platforms: vec!["win64".into()],
        };
        let fields = payload_fields(&game, "win64", &snapshot, None, 9_500_000, false);

        let sha = fields.iter().find(|(n, _, _)| n == "SHA-256").unwrap();
        assert!(sha.1.contains(&snapshot.hashes.sha256), "full digest shown");
        assert_eq!(snapshot.hashes.sha256.len(), 64);

        let other = fields.iter().find(|(n, _, _)| n == "MD5 / SHA-1").unwrap();
        assert!(other.1.contains(&snapshot.hashes.md5));
        assert!(other.1.contains(&snapshot.hashes.sha1));
    }
}

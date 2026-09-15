//! Embed rendering and attachment chunking.

use crate::config::Game;
use crate::eac::{ModuleEntry, Snapshot, short_hash};
use serenity::builder::{CreateEmbed, CreateEmbedFooter};
use serenity::model::Timestamp;

/// Teal accent bar on the left of the embed.
const ACCENT: u32 = 0x00B5C4;

/// Discord renders at most 25 fields; reserve the first few for the summary.
const MAX_FIELDS: usize = 25;
const SUMMARY_FIELDS: usize = 5;

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

fn module_field(module: &ModuleEntry) -> (String, String) {
    let name = match &module.arch {
        Some(arch) if !arch.is_empty() => format!("{} ({})", module.name, arch),
        _ => module.name.clone(),
    };

    let mut value = String::new();
    match module.size {
        Some(size) => value.push_str(&human_bytes(size)),
        None => value.push_str("size unknown"),
    }
    if let Some(hash) = &module.hash {
        value.push_str(&format!("\n`{}`", short_hash(hash)));
    }
    (truncate(&name, 256), truncate(&value, 1024))
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

/// Attach the summary rows shared by the update and check embeds.
fn with_summary(
    mut embed: CreateEmbed,
    game: &Game,
    platform: &str,
    snapshot: &Snapshot,
    max_part: u64,
    attach_raw: bool,
) -> CreateEmbed {
    embed = embed
        .field("Game", &game.name, true)
        .field("Platform", format!("`{platform}`"), true)
        .field("Download", human_bytes(snapshot.size()), true)
        .field("Hash", format!("`{}`", snapshot.short_digest()), false);

    if attach_raw {
        embed = embed.field(
            "Raw Response",
            raw_response_summary(snapshot.size(), max_part),
            false,
        );
    }

    let budget = MAX_FIELDS - SUMMARY_FIELDS;
    let shown = snapshot.modules.len().min(budget);
    for module in &snapshot.modules[..shown] {
        let (name, value) = module_field(module);
        embed = embed.field(name, value, true);
    }
    if snapshot.modules.len() > shown {
        let hidden = snapshot.modules.len() - shown;
        embed = embed.field("…", format!("and {hidden} more module(s)"), false);
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
pub fn build_update_embed(
    game: &Game,
    platform: &str,
    snapshot: &Snapshot,
    previous: Option<&str>,
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

    embed = with_summary(embed, game, platform, snapshot, max_part, attach_raw);

    if let Some(previous) = previous {
        embed = embed.field(
            "Previous Hash",
            format!("`{}`", short_hash(previous)),
            false,
        );
    }

    let footer = cache_footer(snapshot);
    embed.footer(CreateEmbedFooter::new(truncate(&footer, 2048)))
}

/// The embed returned by an on-demand `/eac check`, which reports the current
/// state whether or not anything changed.
pub fn build_check_embed(
    game: &Game,
    platform: &str,
    snapshot: &Snapshot,
    changed: bool,
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
        .colour(if changed { ACCENT } else { 0x4F545C })
        .timestamp(Timestamp::now());

    with_summary(embed, game, platform, snapshot, max_part, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_byte_counts() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(22 * 1024 * 1024), "22.0 MB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
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
        let (name, value) = module_field(&with_arch);
        assert_eq!(name, "driver.sys (arm64)");
        assert_eq!(value, "16.5 MB\n`bc1f4446a7008207`");

        let bare = ModuleEntry {
            name: "client.dll".into(),
            arch: None,
            size: None,
            hash: None,
        };
        let (name, value) = module_field(&bare);
        assert_eq!(name, "client.dll");
        assert_eq!(value, "size unknown");
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(truncate("abcdef", 4), "abc…");
        // Multi-byte input must not be sliced mid-character.
        let s = "ééééé";
        let out = truncate(s, 5);
        assert!(out.ends_with('…'));
        assert!(s.starts_with(out.trim_end_matches('…')));
    }
}

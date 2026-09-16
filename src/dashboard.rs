//! In-Discord dashboard and preset insight view builders.
//!
//! Provides interactive embeds and Discord components (buttons and select
//! menus) for configuring channel routing, tracker settings, and exploring
//! rich preset insights for known EAC games.

use crate::catalog::{self, KnownGame};
use crate::config::Game;
use crate::eac;
use crate::embed;
use crate::settings;
use crate::tracker::Tracker;
use serenity::all::{
    ButtonStyle, ChannelType, CreateActionRow, CreateButton, CreateEmbed, CreateEmbedFooter,
    CreateSelectMenu, CreateSelectMenuKind, CreateSelectMenuOption, Timestamp,
};

/// Build the primary dashboard control panel view.
pub fn build_main_dashboard(
    guild_id: u64,
    tracker: &Tracker,
) -> (CreateEmbed, Vec<CreateActionRow>) {
    let settings = tracker.settings();
    let guild = settings.guild(guild_id);
    let config = tracker.config();
    let tracked_games = settings::guild_games(&guild, config).to_vec();

    let channel_text = match guild.channel_id.or(config.discord.channel_id) {
        Some(cid) => format!("<#{cid}>"),
        None => {
            "⚠️ **Not configured!** (Select a channel below to receive update alerts)".to_string()
        }
    };

    let first_seen_opt = guild
        .announce_on_first_seen
        .unwrap_or(config.tracker.announce_on_first_seen);
    let attach_raw_opt = guild
        .attach_raw_response
        .unwrap_or(config.tracker.attach_raw_response);
    let poll_interval = tracker.poll_interval();

    let games_summary = if tracked_games.is_empty() {
        "*No games tracked yet. Pick a preset from the menu below or click '🚀 Track All Presets' to get started.*"
            .to_string()
    } else {
        tracked_games
            .iter()
            .take(8)
            .map(|g| {
                let platforms_str = if g.platforms.is_empty() {
                    "*(detecting)*".to_string()
                } else {
                    g.platforms
                        .iter()
                        .map(|p| format!("`{p}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                format!("• **{}**: {}", g.name, platforms_str)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let presets_overview = catalog::KNOWN
        .iter()
        .map(|k| {
            let status = if k.is_tracked(&tracked_games) {
                "✅ Tracked"
            } else {
                "⚪ Available"
            };
            format!("• **{}** ({}) — *{}*", k.name, k.publisher, status)
        })
        .collect::<Vec<_>>()
        .join("\n");

    let embed = CreateEmbed::new()
        .title("🛡️ EAC Tracker — Control Panel & Dashboard")
        .description(
            "Real-time configuration dashboard for Easy Anti-Cheat module monitoring. \
             Use the dropdowns and buttons below to manage alert channels, tracker \
             settings, or explore built-in presets.",
        )
        .color(0x5865F2)
        .field("📢 Announcement Channel", channel_text, false)
        .field(
            format!("🎯 Tracked Games ({})", tracked_games.len()),
            games_summary,
            false,
        )
        .field(
            "⚙️ Tracker Settings",
            format!(
                "• **First-Seen Sighting**: {}\n\
                 • **Attach Raw Payload**: {}\n\
                 • **Poll Sweep Interval**: `{}s` ({} min) *(bot-wide)*\n\
                 • **Active Endpoints**: `{} target(s)` polled across bot",
                if first_seen_opt {
                    "🔔 **Enabled**"
                } else {
                    "🔕 **Disabled** (quiet baseline)"
                },
                if attach_raw_opt {
                    "📎 **Enabled** (split >9.5MB)"
                } else {
                    "❌ **Disabled**"
                },
                poll_interval,
                poll_interval / 60,
                tracker.target_count()
            ),
            false,
        )
        .field(
            format!("💡 Known Game Presets Inside ({})", catalog::KNOWN.len()),
            format!(
                "{presets_overview}\n\n*Select any game from the Preset dropdown below to view \
                 architectural insights, CDN endpoints, live probe results, or to track with 1 click.*"
            ),
            false,
        )
        .footer(CreateEmbedFooter::new(
            "EAC Module Tracker • Built-in Presets & Insights",
        ))
        .timestamp(Timestamp::now());

    let mut rows = Vec::new();

    // Row 1: Preset Select Menu
    let preset_options: Vec<CreateSelectMenuOption> = catalog::KNOWN
        .iter()
        .map(|k| {
            let is_tracked = k.is_tracked(&tracked_games);
            let status_desc = if is_tracked {
                "✅ Tracked • Inspect details"
            } else {
                "⚪ Available • Inspect & track"
            };
            CreateSelectMenuOption::new(k.name, k.name)
                .description(format!("{} • {}", k.publisher, status_desc))
        })
        .collect();

    rows.push(CreateActionRow::SelectMenu(
        CreateSelectMenu::new(
            "dash:preset_select",
            CreateSelectMenuKind::String {
                options: preset_options,
            },
        )
        .placeholder("🎮 Explore Known Game Presets & Insights..."),
    ));

    // Row 2: Tracked Games Select Menu (if any tracked)
    if !tracked_games.is_empty() {
        let tracked_options: Vec<CreateSelectMenuOption> = tracked_games
            .iter()
            .take(25)
            .map(|g| {
                let desc = if g.platforms.is_empty() {
                    "0 platforms".to_string()
                } else {
                    format!("{} platform(s): {}", g.platforms.len(), g.platforms.join(", "))
                };
                let desc_truncated = if desc.len() > 100 {
                    format!("{}…", &desc[..99])
                } else {
                    desc
                };
                CreateSelectMenuOption::new(&g.name, &g.name).description(desc_truncated)
            })
            .collect();

        rows.push(CreateActionRow::SelectMenu(
            CreateSelectMenu::new(
                "dash:tracked_select",
                CreateSelectMenuKind::String {
                    options: tracked_options,
                },
            )
            .placeholder("🎯 Manage Tracked Game (Check / Remove)..."),
        ));
    }

    // Row 3: Channel Select Menu
    rows.push(CreateActionRow::SelectMenu(
        CreateSelectMenu::new(
            "dash:channel_select",
            CreateSelectMenuKind::Channel {
                channel_types: Some(vec![ChannelType::Text, ChannelType::News]),
                default_channels: None,
            },
        )
        .placeholder("📢 Set Announcement Channel..."),
    ));

    // Row 4: Toggle Buttons
    let btn_first_seen = CreateButton::new("dash:btn:toggle_first_seen")
        .label(if first_seen_opt {
            "🔔 First Seen: ON"
        } else {
            "🔕 First Seen: OFF"
        })
        .style(if first_seen_opt {
            ButtonStyle::Success
        } else {
            ButtonStyle::Secondary
        });

    let btn_attach_raw = CreateButton::new("dash:btn:toggle_raw")
        .label(if attach_raw_opt {
            "📎 Raw Attach: ON"
        } else {
            "❌ Raw Attach: OFF"
        })
        .style(if attach_raw_opt {
            ButtonStyle::Success
        } else {
            ButtonStyle::Secondary
        });

    let btn_cycle_poll = CreateButton::new("dash:btn:cycle_poll")
        .label(format!("⏱️ Interval: {}s", poll_interval))
        .style(ButtonStyle::Primary);

    rows.push(CreateActionRow::Buttons(vec![
        btn_first_seen,
        btn_attach_raw,
        btn_cycle_poll,
    ]));

    // Row 5: Action Buttons
    let btn_refresh = CreateButton::new("dash:btn:refresh")
        .label("🔄 Refresh")
        .style(ButtonStyle::Primary);

    let btn_add_all = CreateButton::new("dash:btn:add_all_presets")
        .label("🚀 Track All Presets")
        .style(ButtonStyle::Success);

    let btn_status = CreateButton::new("dash:btn:view_status")
        .label("📊 System Status")
        .style(ButtonStyle::Secondary);

    rows.push(CreateActionRow::Buttons(vec![
        btn_refresh,
        btn_add_all,
        btn_status,
    ]));

    (embed, rows)
}

/// Build the preset insight view for a specific known game.
pub fn build_preset_insight(
    guild_id: u64,
    tracker: &Tracker,
    game: &KnownGame,
    live_probes: Option<&[(String, Result<eac::Probe, String>)]>,
) -> (CreateEmbed, Vec<CreateActionRow>) {
    let settings = tracker.settings();
    let guild = settings.guild(guild_id);
    let config = tracker.config();
    let tracked_games = settings::guild_games(&guild, config);
    let is_tracked = game.is_tracked(tracked_games);

    let tracked_entry = tracked_games
        .iter()
        .find(|g| g.product_id == game.product_id && g.deployment_id == game.deployment_id);

    let tracking_status_text = if let Some(g) = tracked_entry {
        format!(
            "✅ **Active in this server**\nConfigured platforms: {}",
            g.platforms
                .iter()
                .map(|p| format!("`{p}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else {
        "⚪ **Not currently tracked in this server**\nClick **Track Preset** below to auto-probe the CDN and start monitoring."
            .to_string()
    };

    let typical_platforms_str = game
        .typical_platforms
        .iter()
        .map(|p| format!("`{p}`"))
        .collect::<Vec<_>>()
        .join(", ");

    let mut embed = CreateEmbed::new()
        .title(format!("🔍 Preset Insight: {}", game.name))
        .description(format!(
            "**Publisher / Studio:** {}\n\n{}\n\n**Typical Platforms:** {}",
            game.publisher, game.insight, typical_platforms_str
        ))
        .color(if is_tracked { 0x2ECC71 } else { 0xF1C40F })
        .field("📊 Tracking Status", tracking_status_text, false)
        .field(
            "🏷️ Epic CDN Identifiers",
            format!(
                "• **Product ID**: `{}`\n• **Deployment ID**: `{}`",
                game.product_id, game.deployment_id
            ),
            false,
        )
        .field(
            "🌐 CDN Endpoint Base",
            format!(
                "`https://modules-cdn.eac-prod.on.epicgames.com/modules/{}/{}/{{platform}}`",
                game.product_id, game.deployment_id
            ),
            false,
        );

    if let Some(probes) = live_probes {
        let mut probe_lines = Vec::new();
        for (platform, result) in probes {
            match result {
                Ok(p) if p.is_module() && !p.suspicious() => {
                    let size_str = p
                        .content_length
                        .map(embed::human_bytes)
                        .unwrap_or_else(|| "unknown size".to_string());
                    probe_lines.push(format!("• `{platform}`: 🟢 **Live Module** ({size_str})"));
                }
                Ok(p) if p.is_module() && p.suspicious() => {
                    let size_str = p
                        .content_length
                        .map(embed::human_bytes)
                        .unwrap_or_else(|| "unknown size".to_string());
                    probe_lines.push(format!("• `{platform}`: ⚠️ **Stub Endpoint** ({size_str})"));
                }
                Ok(p) if p.ok() => {
                    probe_lines.push(format!("• `{platform}`: ⚪ *Empty (0 B / not published)*"));
                }
                Ok(p) => {
                    probe_lines.push(format!("• `{platform}`: ⚪ *HTTP {}*", p.status));
                }
                Err(e) => {
                    probe_lines.push(format!("• `{platform}`: ❌ *Error: {e}*"));
                }
            }
        }
        let probe_summary = if probe_lines.is_empty() {
            "No candidate platforms answered.".to_string()
        } else {
            probe_lines.join("\n")
        };
        embed = embed.field("⚡ Live CDN Probe Results", probe_summary, false);
    } else {
        embed = embed.field(
            "⚡ Live CDN Probe",
            "*Click \"⚡ Probe CDN Now\" below to test candidate platforms against the live Epic CDN in real time.*",
            false,
        );
    }

    embed = embed
        .footer(CreateEmbedFooter::new(format!(
            "Preset: {} • EAC Tracker",
            game.name
        )))
        .timestamp(Timestamp::now());

    let mut buttons = Vec::new();
    if is_tracked {
        buttons.push(
            CreateButton::new(format!("dash:btn:untrack_preset:{}", game.name))
                .label("🗑️ Stop Tracking")
                .style(ButtonStyle::Danger),
        );
    } else {
        buttons.push(
            CreateButton::new(format!("dash:btn:track_preset:{}", game.name))
                .label("➕ Track Preset")
                .style(ButtonStyle::Success),
        );
    }

    buttons.push(
        CreateButton::new(format!("dash:btn:probe_preset:{}", game.name))
            .label("⚡ Probe CDN Now")
            .style(ButtonStyle::Primary),
    );

    buttons.push(
        CreateButton::new("dash:btn:home")
            .label("⬅️ Back to Dashboard")
            .style(ButtonStyle::Secondary),
    );

    (embed, vec![CreateActionRow::Buttons(buttons)])
}

/// Build the management view for a specific tracked game.
pub fn build_tracked_manage(
    _guild_id: u64,
    tracker: &Tracker,
    game: &Game,
) -> (CreateEmbed, Vec<CreateActionRow>) {
    let mut status_lines = Vec::new();
    for platform in &game.platforms {
        if let Some(seen) = tracker.get_seen(&game.product_id, &game.deployment_id, platform) {
            status_lines.push(format!(
                "• `{platform}`: `{}` ({}, <t:{}:R>)",
                eac::short_hash(&seen.digest),
                embed::human_bytes(seen.size),
                seen.seen_at
            ));
        } else {
            status_lines.push(format!("• `{platform}`: *Pending first poll sweep*"));
        }
    }
    let status_str = if status_lines.is_empty() {
        "*No platforms configured.*".to_string()
    } else {
        status_lines.join("\n")
    };

    let embed = CreateEmbed::new()
        .title(format!("🎯 Manage Tracked Game: {}", game.name))
        .description(format!(
            "Manage tracking configuration or trigger an immediate check for **{}**.",
            game.name
        ))
        .color(0x3498DB)
        .field(
            "🏷️ Deployment Details",
            format!(
                "• **Product ID**: `{}`\n• **Deployment ID**: `{}`\n• **Platforms**: {}",
                game.product_id,
                game.deployment_id,
                if game.platforms.is_empty() {
                    "none".to_string()
                } else {
                    game.platforms
                        .iter()
                        .map(|p| format!("`{p}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            ),
            false,
        )
        .field("📡 Current Monitored State", status_str, false)
        .footer(CreateEmbedFooter::new(
            "Tracked Game Management • EAC Tracker",
        ))
        .timestamp(Timestamp::now());

    let buttons = vec![
        CreateButton::new(format!("dash:btn:check_game:{}", game.name))
            .label("⚡ Check Modules Now")
            .style(ButtonStyle::Primary),
        CreateButton::new(format!("dash:btn:remove_game:{}", game.name))
            .label("🗑️ Remove Game")
            .style(ButtonStyle::Danger),
        CreateButton::new("dash:btn:home")
            .label("⬅️ Back to Dashboard")
            .style(ButtonStyle::Secondary),
    ];

    (embed, vec![CreateActionRow::Buttons(buttons)])
}

/// Build the system status view.
pub fn build_status_view(tracker: &Tracker) -> (CreateEmbed, Vec<CreateActionRow>) {
    let lines = tracker.status_lines();
    let body = if lines.is_empty() {
        "No targets have been seen yet. Targets will appear here after their first poll sweep or check."
            .to_string()
    } else {
        lines.join("\n")
    };

    let body_truncated = if body.len() > 2000 {
        format!("{}…", &body[..1990])
    } else {
        body
    };

    let embed = CreateEmbed::new()
        .title("📊 EAC Tracker — System Target Status")
        .description(body_truncated)
        .color(0x5865F2)
        .field(
            "ℹ️ Summary",
            format!(
                "• **Poll Interval**: `{}s`\n• **Total Targets**: `{}`",
                tracker.poll_interval(),
                tracker.target_count()
            ),
            false,
        )
        .footer(CreateEmbedFooter::new("System Status • EAC Tracker"))
        .timestamp(Timestamp::now());

    let buttons = vec![
        CreateButton::new("dash:btn:view_status")
            .label("🔄 Refresh Status")
            .style(ButtonStyle::Primary),
        CreateButton::new("dash:btn:home")
            .label("⬅️ Back to Dashboard")
            .style(ButtonStyle::Secondary),
    ];

    (embed, vec![CreateActionRow::Buttons(buttons)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Discord, Tracker as TrackerCfg};
    use crate::settings::SettingsStore;
    use std::sync::Arc;

    fn test_tracker() -> (Tracker, std::path::PathBuf) {
        let unique = format!(
            "dash-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let dir = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&dir).unwrap();
        let state_path = dir.join("state.json");
        let settings_path = dir.join("settings.json");

        let config = Arc::new(Config {
            discord: Discord {
                token: Some("test-token".into()),
                channel_id: Some(100),
                guild_id: None,
            },
            tracker: TrackerCfg {
                state_path: state_path.to_str().unwrap().to_string(),
                archive_path: "".to_string(),
                ..Default::default()
            },
            games: Vec::new(),
        });
        let settings = Arc::new(SettingsStore::load(&settings_path).unwrap());
        (Tracker::new(config, settings).unwrap(), dir)
    }

    #[test]
    fn main_dashboard_fits_discord_constraints() {
        let (tracker, dir) = test_tracker();
        let (embed, rows) = build_main_dashboard(12345, &tracker);

        // Discord allows at most 5 action rows.
        assert!(rows.len() <= 5, "got {} rows", rows.len());
        assert!(!rows.is_empty());

        let json = serde_json::to_value(&embed).unwrap();
        assert!(
            json["title"]
                .as_str()
                .unwrap()
                .contains("Control Panel & Dashboard")
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn main_dashboard_with_tracked_games_stays_under_five_rows() {
        let (tracker, dir) = test_tracker();
        tracker
            .settings()
            .edit_guild(12345, |g| {
                g.games.push(Game {
                    name: "Apex Legends".to_string(),
                    product_id: "5dcd88f4e2094a698ebffa43438edc33".to_string(),
                    deployment_id: "47a5a1b2e0f64748a96777920ad97fbd".to_string(),
                    platforms: vec!["win64".to_string()],
                });
            })
            .unwrap();

        let (_embed, rows) = build_main_dashboard(12345, &tracker);
        assert_eq!(rows.len(), 5, "must have 5 rows when games are tracked");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn preset_insight_renders_information() {
        let (tracker, dir) = test_tracker();
        let rust = catalog::find("Rust").unwrap();
        let (embed, rows) = build_preset_insight(12345, &tracker, rust, None);

        let json = serde_json::to_value(&embed).unwrap();
        assert!(json["title"].as_str().unwrap().contains("Rust"));
        assert!(
            json["description"]
                .as_str()
                .unwrap()
                .contains("Facepunch Studios")
        );

        // Has 1 action row of buttons
        assert_eq!(rows.len(), 1);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tracked_manage_renders_buttons() {
        let (tracker, dir) = test_tracker();
        let game = Game {
            name: "Rust".to_string(),
            product_id: "429c2212ad284866aee071454c2125b5".to_string(),
            deployment_id: "76796531e86443548754600511f42e9e".to_string(),
            platforms: vec!["win64".to_string()],
        };
        let (embed, rows) = build_tracked_manage(12345, &tracker, &game);

        let json = serde_json::to_value(&embed).unwrap();
        assert!(json["title"].as_str().unwrap().contains("Rust"));
        assert_eq!(rows.len(), 1);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn status_view_renders_correctly() {
        let (tracker, dir) = test_tracker();
        let (embed, rows) = build_status_view(&tracker);

        let json = serde_json::to_value(&embed).unwrap();
        assert!(json["title"].as_str().unwrap().contains("Status"));
        assert_eq!(rows.len(), 1);

        let _ = std::fs::remove_dir_all(dir);
    }
}

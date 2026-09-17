//! In-Discord dashboard and preset insight view builders.
//!
//! Provides clean, minimalist embeds and components for configuring channel
//! routing, tracker options with detailed setting explanations, and inspecting
//! built-in preset insights for known EAC games.

use crate::catalog::{self, KnownGame};
use crate::config::Game;
use crate::eac;
use crate::embed;
use crate::settings;
use crate::tracker::Tracker;
use serenity::all::{
    ButtonStyle, ChannelType, CreateActionRow, CreateButton, CreateEmbed, CreateEmbedFooter,
    CreateInputText, CreateModal, CreateSelectMenu, CreateSelectMenuKind, CreateSelectMenuOption,
    InputTextStyle, Timestamp,
};

/// Clean muted theme colors for minimalist UI.
const COLOR_NEUTRAL: u32 = 0x2B2D31;
const COLOR_ACTIVE: u32 = 0x57F287;
const COLOR_INFO: u32 = 0x5865F2;

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
        None => "Not configured (select a channel below)".to_string(),
    };

    let first_seen_opt = guild
        .announce_on_first_seen
        .unwrap_or(config.tracker.announce_on_first_seen);
    let attach_raw_opt = guild
        .attach_raw_response
        .unwrap_or(config.tracker.attach_raw_response);
    let poll_interval = tracker.poll_interval();

    let nvidia_key = settings::effective_nvidia_key(&guild, config);
    let ai_model = settings::effective_ai_model(&guild, config);
    let ai_delay = settings::effective_ai_delay_ms(&guild, config);
    let ai_enabled = settings::effective_ai_enabled(&guild, config);
    let ghidra_found = crate::ghidra::find_ghidra(config.ai.ghidra_path.as_deref()).is_some();

    let ai_status_desc = if !ai_enabled {
        "Disabled"
    } else if nvidia_key.is_some() {
        "Active (NVIDIA API Connected)"
    } else {
        "Key Required (Configure below)"
    };

    let games_summary = if tracked_games.is_empty() {
        "*None configured yet. Choose a preset below or click 'Track All Presets'.*".to_string()
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
                "[Tracked]"
            } else {
                "[Available]"
            };
            format!("• **{}** ({}) — {}", k.name, k.publisher, status)
        })
        .collect::<Vec<_>>()
        .join("\n");

    let embed = CreateEmbed::new()
        .title("EAC Tracker — Configuration Dashboard")
        .description(
            "Manage alert channels, tracker options, and explore built-in deployment presets. \
             Each setting is documented below.",
        )
        .color(COLOR_NEUTRAL)
        .field(
            "Announce Channel",
            format!(
                "**Current:** {}\n\
                 Destination channel where module updates, binary change digests, and carved PE metadata are posted.",
                channel_text
            ),
            false,
        )
        .field(
            format!("Tracked Games ({})", tracked_games.len()),
            games_summary,
            false,
        )
        .field(
            "Tracker Settings & Explanations",
            format!(
                "• **First-Seen Sighting** (`announce_on_first_seen`): **{}**\n  \
                 *Whether to post an embed when a target is first observed. Disabled by default so starting the bot records quiet baselines instead of announcing every existing module.*\n\n\
                 • **Raw Payload Attachment** (`attach_raw_response`): **{}**\n  \
                 *Uploads the raw CDN container alongside update embeds for offline inspection (automatically split if larger than 9.5 MB).*\n\n\
                 • **Poll Interval** (`poll_interval_secs`): **{}s** ({} min, bot-wide)\n  \
                 *Delay between CDN polling sweeps across all targets. Enforces a 30-second floor to remain polite to the CDN.*\n\n\
                 • **Active Endpoints**: `{} target(s)` polled across bot",
                if first_seen_opt { "Enabled" } else { "Disabled" },
                if attach_raw_opt { "Enabled" } else { "Disabled" },
                poll_interval,
                poll_interval / 60,
                tracker.target_count()
            ),
            false,
        )
        .field(
            format!("Built-In Game Presets ({})", catalog::KNOWN.len()),
            format!(
                "{presets_overview}\n\n\
                 *Select a game below to view architecture insights, CDN endpoints, live probe results, or to track.*"
            ),
            false,
        )
        .field(
            "AI & Reverse Engineering Engine",
            format!(
                "• **Status**: {}\n\
                 • **NVIDIA API Key**: {}\n\
                 • **Model**: `{}`\n\
                 • **Rate Limit Delay**: `{}ms`\n\
                 • **Headless Ghidra**: {}\n  \
                 *Click 'Configure AI' below to enter/update your API key, model, or rate limit delay.*",
                ai_status_desc,
                nvidia_key.map(crate::ai::mask_key).unwrap_or_else(|| "*(not configured)*".to_string()),
                ai_model,
                ai_delay,
                if ghidra_found { "Detected (`analyzeHeadless` available)" } else { "Not detected (PE fallback mode)" }
            ),
            false,
        )
        .footer(CreateEmbedFooter::new(
            "EAC Module Tracker • Minimalist Dashboard",
        ))
        .timestamp(Timestamp::now());

    let mut rows = Vec::new();

    // Row 1: Preset Select Menu
    let preset_options: Vec<CreateSelectMenuOption> = catalog::KNOWN
        .iter()
        .map(|k| {
            let is_tracked = k.is_tracked(&tracked_games);
            let status_desc = if is_tracked {
                "Tracked • View insights"
            } else {
                "Available • View insights & track"
            };
            CreateSelectMenuOption::new(k.name, k.name)
                .description(format!("{} — {}", k.publisher, status_desc))
        })
        .collect();

    rows.push(CreateActionRow::SelectMenu(
        CreateSelectMenu::new(
            "dash:preset_select",
            CreateSelectMenuKind::String {
                options: preset_options,
            },
        )
        .placeholder("Select a known game preset for insights..."),
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
            .placeholder("Select a tracked game to manage..."),
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
        .placeholder("Select announcement channel..."),
    ));

    // Row 4: Toggle Buttons
    let btn_first_seen = CreateButton::new("dash:btn:toggle_first_seen")
        .label(if first_seen_opt {
            "First-Seen: Enabled"
        } else {
            "First-Seen: Disabled"
        })
        .style(if first_seen_opt {
            ButtonStyle::Success
        } else {
            ButtonStyle::Secondary
        });

    let btn_attach_raw = CreateButton::new("dash:btn:toggle_raw")
        .label(if attach_raw_opt {
            "Raw Payload: Attached"
        } else {
            "Raw Payload: Omitted"
        })
        .style(if attach_raw_opt {
            ButtonStyle::Success
        } else {
            ButtonStyle::Secondary
        });

    let btn_cycle_poll = CreateButton::new("dash:btn:cycle_poll")
        .label(format!("Interval: {}s", poll_interval))
        .style(ButtonStyle::Primary);

    rows.push(CreateActionRow::Buttons(vec![
        btn_first_seen,
        btn_attach_raw,
        btn_cycle_poll,
    ]));

    // Row 5: Action Buttons
    let btn_refresh = CreateButton::new("dash:btn:refresh")
        .label("Refresh")
        .style(ButtonStyle::Secondary);

    let btn_add_all = CreateButton::new("dash:btn:add_all_presets")
        .label("Track All Presets")
        .style(ButtonStyle::Primary);

    let btn_status = CreateButton::new("dash:btn:view_status")
        .label("Status")
        .style(ButtonStyle::Secondary);

    let btn_ai = CreateButton::new("dash:btn:configure_ai")
        .label("Configure AI")
        .style(ButtonStyle::Secondary);

    let btn_test_ai = CreateButton::new("dash:btn:test_ai")
        .label("Test AI")
        .style(ButtonStyle::Secondary);

    rows.push(CreateActionRow::Buttons(vec![
        btn_refresh,
        btn_add_all,
        btn_status,
        btn_ai,
        btn_test_ai,
    ]));

    (embed, rows)
}

/// Available platform targets on the Epic Games EAC CDN.
pub const PLATFORM_INFO: &[(&str, &str)] = &[
    ("win64", "Windows 64-bit (standard desktop)"),
    ("winarm_x64_x64", "Windows ARM64 (host with x64 translation)"),
    ("linux32_64", "Linux 32/64-bit runtime (native & Proton)"),
    ("mac64", "macOS 64-bit Mach-O container"),
    ("wow64_win64", "Windows WoW64 (32-on-64 hybrid)"),
    ("win32", "Windows 32-bit legacy"),
    ("wow64", "Windows WoW64 legacy"),
    ("wine64", "Wine 64-bit compatibility target"),
    ("wine32", "Wine 32-bit compatibility target"),
];

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
            "Tracked in this server\nPlatforms: {}",
            g.platforms
                .iter()
                .map(|p| format!("`{p}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else {
        "Not currently tracked in this server. Select platforms below or click 'Track All Live'."
            .to_string()
    };

    let typical_platforms_str = game
        .typical_platforms
        .iter()
        .map(|p| format!("`{p}`"))
        .collect::<Vec<_>>()
        .join(", ");

    let mut embed = CreateEmbed::new()
        .title(format!("Preset Insight: {}", game.name))
        .description(format!(
            "**Developer / Publisher:** {}\n\n{}\n\n**Typical Platforms:** {}",
            game.publisher, game.insight, typical_platforms_str
        ))
        .color(if is_tracked { COLOR_ACTIVE } else { COLOR_INFO })
        .field("Status", tracking_status_text, false)
        .field(
            "Identifiers",
            format!(
                "• Product ID: `{}`\n• Deployment ID: `{}`",
                game.product_id, game.deployment_id
            ),
            false,
        )
        .field(
            "CDN URL Template",
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
                    probe_lines.push(format!("• `{platform}`: Live module ({size_str})"));
                }
                Ok(p) if p.is_module() && p.suspicious() => {
                    let size_str = p
                        .content_length
                        .map(embed::human_bytes)
                        .unwrap_or_else(|| "unknown size".to_string());
                    probe_lines.push(format!("• `{platform}`: Stub endpoint ({size_str}, skipped by default)"));
                }
                Ok(p) if p.ok() => {
                    probe_lines.push(format!("• `{platform}`: Not published (0 B)"));
                }
                Ok(p) => {
                    probe_lines.push(format!("• `{platform}`: HTTP {}", p.status));
                }
                Err(e) => {
                    probe_lines.push(format!("• `{platform}`: Error: {e}"));
                }
            }
        }
        let probe_summary = if probe_lines.is_empty() {
            "No candidate platforms answered.".to_string()
        } else {
            probe_lines.join("\n")
        };
        embed = embed.field("Live CDN Probe Results", probe_summary, false);
    } else {
        embed = embed.field(
            "Live CDN Probe",
            "Click 'Probe CDN' below to query the live Epic Games CDN endpoints and verify published modules.",
            false,
        );
    }

    embed = embed
        .footer(CreateEmbedFooter::new(format!(
            "Preset: {}",
            game.name
        )))
        .timestamp(Timestamp::now());

    let platform_options: Vec<CreateSelectMenuOption> = PLATFORM_INFO
        .iter()
        .map(|(plat, desc)| {
            let is_typical = game.typical_platforms.contains(plat);
            let currently_tracked = tracked_entry
                .is_some_and(|g| g.platforms.iter().any(|p| p == *plat));
            let desc_suffix = if is_typical {
                format!("{desc} [Recommended]")
            } else {
                desc.to_string()
            };
            CreateSelectMenuOption::new(*plat, *plat)
                .description(desc_suffix)
                .default_selection(currently_tracked)
        })
        .collect();

    let placeholder = if is_tracked {
        "Update tracked platform(s)..."
    } else {
        "Choose platform(s) to track..."
    };

    let platform_menu = CreateSelectMenu::new(
        format!("dash:preset_platforms:{}", game.name),
        CreateSelectMenuKind::String {
            options: platform_options,
        },
    )
    .placeholder(placeholder)
    .min_values(1)
    .max_values(PLATFORM_INFO.len() as u8);

    let mut buttons = Vec::new();
    if is_tracked {
        buttons.push(
            CreateButton::new(format!("dash:btn:untrack_preset:{}", game.name))
                .label("Stop Tracking")
                .style(ButtonStyle::Danger),
        );
    } else {
        buttons.push(
            CreateButton::new(format!("dash:btn:track_preset:{}", game.name))
                .label("Track All Live")
                .style(ButtonStyle::Success),
        );
    }

    buttons.push(
        CreateButton::new(format!("dash:btn:probe_preset:{}", game.name))
            .label("Probe CDN")
            .style(ButtonStyle::Primary),
    );

    buttons.push(
        CreateButton::new("dash:btn:home")
            .label("Back")
            .style(ButtonStyle::Secondary),
    );

    (
        embed,
        vec![
            CreateActionRow::SelectMenu(platform_menu),
            CreateActionRow::Buttons(buttons),
        ],
    )
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
            status_lines.push(format!("• `{platform}`: Pending first poll sweep"));
        }
    }
    let status_str = if status_lines.is_empty() {
        "No platforms configured.".to_string()
    } else {
        status_lines.join("\n")
    };

    let embed = CreateEmbed::new()
        .title(format!("Manage Tracked Game: {}", game.name))
        .description(format!(
            "Inspect current state or update tracked platforms for **{}**.",
            game.name
        ))
        .color(COLOR_NEUTRAL)
        .field(
            "Deployment Details",
            format!(
                "• Product ID: `{}`\n• Deployment ID: `{}`\n• Platforms: {}",
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
        .field("Monitored State", status_str, false)
        .footer(CreateEmbedFooter::new(
            "Tracked Game Management",
        ))
        .timestamp(Timestamp::now());

    let platform_options: Vec<CreateSelectMenuOption> = PLATFORM_INFO
        .iter()
        .map(|(plat, desc)| {
            let currently_tracked = game.platforms.iter().any(|p| p == *plat);
            CreateSelectMenuOption::new(*plat, *plat)
                .description(*desc)
                .default_selection(currently_tracked)
        })
        .collect();

    let platform_menu = CreateSelectMenu::new(
        format!("dash:game_platforms:{}", game.name),
        CreateSelectMenuKind::String {
            options: platform_options,
        },
    )
    .placeholder("Update tracked platform(s)...")
    .min_values(1)
    .max_values(PLATFORM_INFO.len() as u8);

    let buttons = vec![
        CreateButton::new(format!("dash:btn:check_game:{}", game.name))
            .label("Check Now")
            .style(ButtonStyle::Primary),
        CreateButton::new(format!("dash:btn:devirt_game:{}", game.name))
            .label("Devirtualize")
            .style(ButtonStyle::Secondary),
        CreateButton::new(format!("dash:btn:remove_game:{}", game.name))
            .label("Remove Game")
            .style(ButtonStyle::Danger),
        CreateButton::new("dash:btn:home")
            .label("Back")
            .style(ButtonStyle::Secondary),
    ];

    (
        embed,
        vec![
            CreateActionRow::SelectMenu(platform_menu),
            CreateActionRow::Buttons(buttons),
        ],
    )
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
        .title("EAC Tracker — System Status")
        .description(body_truncated)
        .color(COLOR_NEUTRAL)
        .field(
            "Summary",
            format!(
                "• Poll Interval: `{}s`\n• Total Targets: `{}`",
                tracker.poll_interval(),
                tracker.target_count()
            ),
            false,
        )
        .footer(CreateEmbedFooter::new("System Status"))
        .timestamp(Timestamp::now());

    let buttons = vec![
        CreateButton::new("dash:btn:view_status")
            .label("Refresh")
            .style(ButtonStyle::Primary),
        CreateButton::new("dash:btn:home")
            .label("Back")
            .style(ButtonStyle::Secondary),
    ];

    (embed, vec![CreateActionRow::Buttons(buttons)])
}


/// Modal dialog for configuring NVIDIA API key, model selection, and rate limiting.
pub fn build_ai_modal(
    current_key: Option<&str>,
    current_model: &str,
    current_delay_ms: u64,
) -> CreateModal {
    let key_placeholder = match current_key {
        Some(k) => format!("Current: {}; leave blank to keep", crate::ai::mask_key(k)),
        None => "nvapi-...".to_string(),
    };

    let key_input = CreateInputText::new(
        InputTextStyle::Short,
        "NVIDIA API Key",
        "ai_key",
    )
    .placeholder(key_placeholder)
    .required(false);

    let model_input = CreateInputText::new(
        InputTextStyle::Short,
        "Model (e.g. z-ai/glm-5-3-flash)",
        "ai_model",
    )
    .placeholder("z-ai/glm-5-3-flash")
    .value(current_model)
    .required(false);

    let delay_input = CreateInputText::new(
        InputTextStyle::Short,
        "Rate Limit Delay (ms)",
        "ai_delay",
    )
    .placeholder("1000")
    .value(current_delay_ms.to_string())
    .required(false);

    CreateModal::new("dash:modal:ai_config", "Configure AI (NVIDIA NIM)")
        .components(vec![
            CreateActionRow::InputText(key_input),
            CreateActionRow::InputText(model_input),
            CreateActionRow::InputText(delay_input),
        ])
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
            ai: Default::default(),
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
                .contains("Configuration Dashboard")
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

        // Has 2 action rows: 1 select menu + 1 button row
        assert_eq!(rows.len(), 2);

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
        assert_eq!(rows.len(), 2);

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
    #[test]
    fn ai_modal_serializes_cleanly() {
        let modal = build_ai_modal(Some("nvapi-12345678901234"), "z-ai/glm-5-3-flash", 1000);
        let val = serde_json::to_value(&modal).unwrap();
        assert_eq!(val["custom_id"], "dash:modal:ai_config");
        let components = val["components"].as_array().unwrap();
        assert_eq!(components.len(), 3);
    }
}

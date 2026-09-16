//! Discord gateway wiring and the `/eac` command surface.
//!
//! Everything a server needs is configured here rather than in a file: the
//! announce channel, which games to track, and the per-guild announcement
//! options. The operator supplies only a bot token.

use crate::catalog;
use crate::config::Game;
use crate::dashboard;
use crate::eac;
use crate::embed;
use crate::settings::{self, MIN_POLL_INTERVAL_SECS};
use crate::tracker::Tracker;
use serenity::all::{
    AutocompleteChoice, ChannelType, CommandDataOption, CommandDataOptionValue,
    CommandInteraction, CommandOptionType, ComponentInteraction, ComponentInteractionDataKind,
    Context, CreateAutocompleteResponse, CreateCommand, CreateCommandOption,
    CreateInteractionResponse, CreateInteractionResponseMessage, CreateSelectMenu,
    CreateSelectMenuKind, CreateSelectMenuOption, EditInteractionResponse, EventHandler, GuildId,
    Interaction, Permissions, Ready,
};
use serenity::async_trait;
use serenity::builder::CreateEmbed;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{error, info};

/// Settings that `/eac set` can change, as (name, description) pairs.
const SETTABLE: &[(&str, &str)] = &[
    (
        "announce_on_first_seen",
        "Post an embed the first time a target is seen (true/false)",
    ),
    (
        "attach_raw_response",
        "Attach the raw CDN response to updates (true/false)",
    ),
    (
        "poll_interval_secs",
        "Seconds between sweeps (minimum 30, applies bot-wide)",
    ),
];

/// custom_id of the catalogue pick list.
const SELECT_KNOWN: &str = "eac:known";

/// Result of storing one game.
struct AddOutcome {
    game: Game,
    replaced: bool,
    /// Platforms that answered, with their download size.
    live: Vec<(String, Option<u64>)>,
    /// Platforms that did not, with why.
    rejected: Vec<String>,
}

impl AddOutcome {
    /// One line per game. `show_rejected` is false when platforms were being
    /// detected, since most candidates not matching is the expected outcome
    /// rather than something the reader needs told about.
    fn describe(&self, show_rejected: bool) -> String {
        let verb = if self.replaced {
            "Updated"
        } else {
            "Now tracking"
        };
        let confirmed = self
            .live
            .iter()
            .map(|(platform, size)| match size {
                Some(bytes) => format!("`{platform}` ({})", embed::human_bytes(*bytes)),
                None => format!("`{platform}`"),
            })
            .collect::<Vec<_>>()
            .join(", ");

        let mut message = format!("{verb} **{}** — verified {confirmed}.", self.game.name);
        // A module far below the observed size range is probably a stub.
        let stubs: Vec<&str> = self
            .live
            .iter()
            .filter(|(_, size)| size.is_some_and(|n| n < eac::MIN_PLAUSIBLE_MODULE_BYTES))
            .map(|(platform, _)| platform.as_str())
            .collect();
        if !stubs.is_empty() {
            message.push_str(&format!(
                "\n\nSuspiciously small, so possibly a stub rather than a module: {}",
                stubs
                    .iter()
                    .map(|p| format!("`{p}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if show_rejected && !self.rejected.is_empty() {
            message.push_str(&format!(
                "\n\nNot published, so skipped:\n{}",
                self.rejected.join("\n")
            ));
        }
        message
    }
}

/// Shorten an id for a select-menu description, which is tight on space.
fn truncate_id(id: &str) -> String {
    match id.char_indices().nth(12) {
        Some((cut, _)) => format!("{}…", &id[..cut]),
        None => id.to_string(),
    }
}

pub struct Handler {
    tracker: Arc<Tracker>,
    /// `ready` fires again on every gateway reconnect; the loop starts once.
    loop_started: AtomicBool,
}

impl Handler {
    pub fn new(tracker: Arc<Tracker>) -> Self {
        Self {
            tracker,
            loop_started: AtomicBool::new(false),
        }
    }

    fn command() -> CreateCommand {
        let game_option = |required: bool| {
            CreateCommandOption::new(CommandOptionType::String, "game", "Tracked game name")
                .required(required)
                .set_autocomplete(true)
        };

        CreateCommand::new("eac")
            .description("Easy Anti-Cheat module tracker")
            // Read-only subcommands stay open; mutating ones are checked in
            // the handler, so a server can decide who may reconfigure.
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "dashboard",
                "Open the interactive control panel to configure the bot and explore preset insights",
            ))
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "setup",
                    "Choose the channel updates are posted to",
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::Channel,
                        "channel",
                        "Channel for update embeds",
                    )
                    .channel_types(vec![ChannelType::Text, ChannelType::News])
                    .required(true),
                ),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "add",
                    "Track a game's EAC modules",
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "game",
                        "Display name, or a name from /eac browse",
                    )
                    .required(true),
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "product_id",
                        "EAC product id — omit for a game in the built-in list",
                    )
                    .required(false),
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "deployment_id",
                        "EAC deployment id — omit for a game in the built-in list",
                    )
                    .required(false),
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "platforms",
                        "Comma separated. Leave empty to detect them automatically",
                    )
                    .required(false),
                ),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "remove",
                    "Stop tracking a game",
                )
                .add_sub_option(game_option(true)),
            )
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "browse",
                "Pick from the built-in list of known games",
            ))
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "list",
                "Show the games and platforms being tracked",
            ))
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "config",
                "Show this server's current configuration",
            ))
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "status",
                "Show the last seen hash for every target",
            ))
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "check",
                    "Fetch a game's modules right now",
                )
                .add_sub_option(game_option(true))
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "platform",
                        "Platform to check (default: all configured platforms)",
                    )
                    .required(false),
                ),
            )
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "set",
                    "Change a tracker option",
                )
                .add_sub_option(
                    SETTABLE.iter().fold(
                        CreateCommandOption::new(
                            CommandOptionType::String,
                            "option",
                            "Which option to change",
                        )
                        .required(true),
                        |opt, (name, _)| opt.add_string_choice(*name, *name),
                    ),
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "value",
                        "New value: true/false, or a number of seconds",
                    )
                    .required(true),
                ),
            )
    }

    /// Games this guild tracks, falling back to the operator's config.
    fn games(&self, guild_id: u64) -> Vec<Game> {
        let guild = self.tracker.settings().guild(guild_id);
        settings::guild_games(&guild, self.tracker.config()).to_vec()
    }

    /// Case-insensitive lookup, also accepting a unique prefix so an
    /// autocomplete suggestion edited by hand still resolves.
    fn find_game(&self, guild_id: u64, needle: &str) -> Option<Game> {
        let needle = needle.trim().to_lowercase();
        let games = self.games(guild_id);
        if let Some(exact) = games.iter().find(|g| g.name.to_lowercase() == needle) {
            return Some(exact.clone());
        }
        let mut prefixed = games
            .iter()
            .filter(|g| g.name.to_lowercase().starts_with(&needle));
        let first = prefixed.next()?;
        prefixed.next().is_none().then(|| first.clone())
    }

    async fn handle_command(&self, ctx: &Context, cmd: &CommandInteraction) {
        let Some(sub) = cmd.data.options.first() else {
            return;
        };
        let options = match &sub.value {
            CommandDataOptionValue::SubCommand(options) => options.as_slice(),
            _ => &[],
        };

        // Everything here is per-server state, so there is nothing to do in a DM.
        let Some(guild_id) = cmd.guild_id.map(|g| g.get()) else {
            let _ = respond(ctx, cmd, "Run `/eac` inside a server, not a DM.").await;
            return;
        };

        // Reconfiguration is gated; reading is not.
        let mutating = matches!(
            sub.name.as_str(),
            "setup" | "add" | "remove" | "set" | "browse" | "dashboard"
        );
        if mutating && !can_manage(cmd) {
            let _ = respond(
                ctx,
                cmd,
                "You need the **Manage Server** permission to change tracker settings.",
            )
            .await;
            return;
        }

        let result = match sub.name.as_str() {
            "dashboard" => self.reply_dashboard(ctx, cmd, guild_id).await,
            "setup" => self.reply_setup(ctx, cmd, guild_id, options).await,
            "add" => self.reply_add(ctx, cmd, guild_id, options).await,
            "browse" => self.reply_browse(ctx, cmd, guild_id).await,
            "remove" => self.reply_remove(ctx, cmd, guild_id, options).await,
            "list" => self.reply_list(ctx, cmd, guild_id).await,
            "config" => self.reply_config(ctx, cmd, guild_id).await,
            "status" => self.reply_status(ctx, cmd).await,
            "check" => self.reply_check(ctx, cmd, guild_id, options).await,
            "set" => self.reply_set(ctx, cmd, guild_id, options).await,
            other => {
                error!(subcommand = %other, "unknown subcommand");
                Ok(())
            }
        };
        if let Err(e) = result {
            error!(error = ?e, "failed to respond to interaction");
        }
    }

    async fn reply_dashboard(
        &self,
        ctx: &Context,
        cmd: &CommandInteraction,
        guild_id: u64,
    ) -> serenity::Result<()> {
        let (embed, components) = dashboard::build_main_dashboard(guild_id, &self.tracker);
        cmd.create_response(
            &ctx.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .embed(embed)
                    .components(components)
                    .ephemeral(true),
            ),
        )
        .await
    }

    async fn reply_setup(
        &self,
        ctx: &Context,
        cmd: &CommandInteraction,
        guild_id: u64,
        options: &[CommandDataOption],
    ) -> serenity::Result<()> {
        let Some(channel) =
            options
                .iter()
                .find(|o| o.name == "channel")
                .and_then(|o| match &o.value {
                    CommandDataOptionValue::Channel(id) => Some(*id),
                    _ => None,
                })
        else {
            return respond(ctx, cmd, "Pick a channel.").await;
        };

        match self
            .tracker
            .settings()
            .edit_guild(guild_id, |g| g.channel_id = Some(channel.get()))
        {
            Ok(()) => {
                let tracked = self.games(guild_id).len();
                let next = if tracked == 0 {
                    "\nNow add a game with `/eac add`."
                } else {
                    ""
                };
                respond(
                    ctx,
                    cmd,
                    format!("Updates will be posted to <#{channel}>.{next}"),
                )
                .await
            }
            Err(e) => {
                error!(error = ?e, "failed to save settings");
                respond(ctx, cmd, format!("Could not save that: {e}")).await
            }
        }
    }

    /// Probe an id pair and store it when something is published.
    ///
    /// `requested` of `None` means detect: every candidate platform is probed
    /// and the ones that answer are kept, because which shape a deployment
    /// uses varies and a single default would be wrong for some.
    async fn try_add(
        &self,
        guild_id: u64,
        name: &str,
        product_id: &str,
        deployment_id: &str,
        requested: Option<&str>,
    ) -> Result<AddOutcome, String> {
        let product_id = product_id.trim();
        let deployment_id = deployment_id.trim();

        // Validate before touching stored state: these values are interpolated
        // straight into a CDN URL.
        settings::validate_name(name).map_err(|e| e.to_string())?;
        settings::validate_id("product_id", product_id).map_err(|e| e.to_string())?;
        settings::validate_id("deployment_id", deployment_id).map_err(|e| e.to_string())?;

        let platforms: Vec<String> = match requested {
            Some(raw) => settings::parse_platforms(raw).map_err(|e| e.to_string())?,
            None => eac::CANDIDATE_PLATFORMS
                .iter()
                .map(|p| (*p).to_string())
                .collect(),
        };

        // Confirm the ids are real before storing them: a wrong pair would
        // otherwise sit in the config as a silently dead target.
        let probes = self
            .tracker
            .probe_platforms(product_id, deployment_id, &platforms)
            .await;

        let mut live = Vec::new();
        let mut stubs = Vec::new();
        let mut rejected = Vec::new();
        for (platform, probe) in platforms.iter().zip(probes) {
            match probe {
                // Detecting means picking the real modules. A stub asked for
                // by name is still honoured, but auto-adding several that all
                // return the same tiny body is just noise.
                Ok(p) if p.is_module() && p.suspicious() && requested.is_none() => {
                    stubs.push((platform.clone(), p.content_length))
                }
                Ok(p) if p.is_module() => live.push((platform.clone(), p.content_length)),
                // A 2xx with an empty body means "not published here", so say
                // that rather than reporting a success that stores nothing.
                Ok(p) if p.ok() => rejected.push(format!("`{platform}` — published nothing (0 B)")),
                Ok(p) => rejected.push(format!("`{platform}` — HTTP {}", p.status)),
                Err(e) => rejected.push(format!("`{platform}` — {e:#}")),
            }
        }
        // Fall back to the stubs rather than refusing outright: a deployment
        // that publishes only small modules is still worth tracking.
        let only_stubs = live.is_empty() && !stubs.is_empty();
        if only_stubs {
            live = std::mem::take(&mut stubs);
        }
        if live.is_empty() {
            return Err(format!("nothing published:\n{}", rejected.join("\n")));
        }
        for (platform, size) in &stubs {
            rejected.push(format!(
                "`{platform}` — {}, probably a stub",
                embed::human_bytes(size.unwrap_or(0))
            ));
        }

        let game = Game {
            name: name.trim().to_string(),
            product_id: product_id.to_string(),
            deployment_id: deployment_id.to_string(),
            platforms: live.iter().map(|(p, _)| p.clone()).collect(),
        };

        // Seed from the config fallback so the first add does not silently
        // drop an operator-configured list.
        let seed = self.games(guild_id);
        let stored = game.clone();
        let replaced = self
            .tracker
            .settings()
            .edit_guild(guild_id, move |g| {
                if g.games.is_empty() {
                    g.games = seed;
                }
                match g
                    .games
                    .iter()
                    .position(|existing| existing.name.eq_ignore_ascii_case(&stored.name))
                {
                    Some(i) => {
                        g.games[i] = stored;
                        true
                    }
                    None => {
                        g.games.push(stored);
                        false
                    }
                }
            })
            .map_err(|e| {
                error!(error = ?e, "failed to save settings");
                format!("could not save: {e}")
            })?;

        Ok(AddOutcome {
            game,
            replaced,
            live,
            rejected,
        })
    }

    async fn reply_add(
        &self,
        ctx: &Context,
        cmd: &CommandInteraction,
        guild_id: u64,
        options: &[CommandDataOption],
    ) -> serenity::Result<()> {
        let name = string_option(options, "game").unwrap_or_default();
        let requested = string_option(options, "platforms").filter(|p| !p.trim().is_empty());

        // Ids may be omitted for a catalogue game, so `/eac add game:Rust`
        // works without looking anything up.
        let (product_id, deployment_id) = match (
            string_option(options, "product_id"),
            string_option(options, "deployment_id"),
        ) {
            (Some(product), Some(deployment)) => (product, deployment),
            _ => match catalog::find(&name) {
                Some(known) => (
                    known.product_id.to_string(),
                    known.deployment_id.to_string(),
                ),
                None => {
                    return respond(
                        ctx,
                        cmd,
                        format!(
                            "**{name}** is not in the built-in list, so it needs \
                             `product_id` and `deployment_id`. Run `/eac browse` to \
                             see the known games, or `eac-tracker discover` to read \
                             the ids out of an install."
                        ),
                    )
                    .await;
                }
            },
        };

        // Probing means a round trip per platform, which can exceed Discord's
        // three-second window.
        if let Err(e) = cmd.defer_ephemeral(&ctx.http).await {
            error!(error = ?e, "failed to defer");
            return Ok(());
        }

        let detecting = requested.is_none();
        match self
            .try_add(
                guild_id,
                &name,
                &product_id,
                &deployment_id,
                requested.as_deref(),
            )
            .await
        {
            Ok(outcome) => {
                let mut message = outcome.describe(!detecting);
                if self.announce_channel(guild_id).is_none() {
                    message.push_str("\n\nNo announce channel is set yet — run `/eac setup`.");
                }
                edit(ctx, cmd, message).await
            }
            Err(e) => {
                edit(
                    ctx,
                    cmd,
                    format!(
                        "Nothing was added — {e}\n\nCheck the ids against the game's \
                         EasyAntiCheat config. The game may also use the legacy EAC \
                         backend, which this CDN does not serve."
                    ),
                )
                .await
            }
        }
    }

    /// Offer the built-in catalogue as a pick list.
    async fn reply_browse(
        &self,
        ctx: &Context,
        cmd: &CommandInteraction,
        guild_id: u64,
    ) -> serenity::Result<()> {
        let tracked = self.games(guild_id);
        let offered = catalog::not_yet_tracked(&tracked);

        if offered.is_empty() {
            return respond(
                ctx,
                cmd,
                "Every game in the built-in list is already tracked. Use `/eac add` \
                 for anything else.",
            )
            .await;
        }

        let options: Vec<CreateSelectMenuOption> = offered
            .iter()
            .map(|known| {
                CreateSelectMenuOption::new(known.name, known.name)
                    .description(format!("product {}", truncate_id(known.product_id)))
            })
            .collect();

        let menu = CreateSelectMenu::new(
            SELECT_KNOWN,
            CreateSelectMenuKind::String {
                options: options.clone(),
            },
        )
        .placeholder("Pick the games to track")
        .min_values(1)
        .max_values(options.len() as u8);

        cmd.create_response(
            &ctx.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(
                        "Known EAC deployments. Pick any number — each is checked \
                         against the CDN before it is added, and ones that publish \
                         nothing are skipped.",
                    )
                    .select_menu(menu)
                    .ephemeral(true),
            ),
        )
        .await
    }

    /// Add everything picked from the catalogue.
    async fn handle_selection(&self, ctx: &Context, mc: &ComponentInteraction) {
        let ComponentInteractionDataKind::StringSelect { values } = &mc.data.kind else {
            return;
        };
        let Some(guild_id) = mc.guild_id.map(|g| g.get()) else {
            return;
        };
        if !permits_manage(mc.member.as_ref().and_then(|m| m.permissions)) {
            let _ = mc
                .create_response(
                    &ctx.http,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .content("You need the **Manage Server** permission.")
                            .ephemeral(true),
                    ),
                )
                .await;
            return;
        }

        if let Err(e) = mc.defer_ephemeral(&ctx.http).await {
            error!(error = ?e, "failed to defer selection");
            return;
        }

        let mut lines = Vec::new();
        for name in values {
            let Some(known) = catalog::find(name) else {
                continue;
            };
            match self
                .try_add(
                    guild_id,
                    known.name,
                    known.product_id,
                    known.deployment_id,
                    None,
                )
                .await
            {
                // Detected platforms, so unpublished candidates are expected.
                Ok(outcome) => lines.push(outcome.describe(false)),
                Err(e) => lines.push(format!("**{}** — not added, {e}", known.name)),
            }
        }
        if self.announce_channel(guild_id).is_none() {
            lines.push("\nNo announce channel is set yet — run `/eac setup`.".to_string());
        }

        let body = truncate_message(&lines.join("\n"), "Nothing was selected.");
        if let Err(e) = mc
            .edit_response(&ctx.http, EditInteractionResponse::new().content(body))
            .await
        {
            error!(error = ?e, "failed to report selection result");
        }
    }

    /// Handle interactive component events from the in-Discord dashboard.
    async fn handle_dashboard_interaction(&self, ctx: &Context, mc: &ComponentInteraction) {
        let Some(guild_id) = mc.guild_id.map(|g| g.get()) else {
            return;
        };
        if !permits_manage(mc.member.as_ref().and_then(|m| m.permissions)) {
            let _ = mc
                .create_response(
                    &ctx.http,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .content("You need the **Manage Server** permission to use the dashboard.")
                            .ephemeral(true),
                    ),
                )
                .await;
            return;
        }

        let custom_id = mc.data.custom_id.as_str();
        match custom_id {
            "dash:preset_select" => {
                let ComponentInteractionDataKind::StringSelect { values } = &mc.data.kind else {
                    return;
                };
                let Some(name) = values.first() else {
                    return;
                };
                let Some(known) = catalog::find(name) else {
                    return;
                };
                let (embed, components) =
                    dashboard::build_preset_insight(guild_id, &self.tracker, known, None);
                let _ = mc
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::UpdateMessage(
                            CreateInteractionResponseMessage::new()
                                .embed(embed)
                                .components(components),
                        ),
                    )
                    .await;
            }
            "dash:tracked_select" => {
                let ComponentInteractionDataKind::StringSelect { values } = &mc.data.kind else {
                    return;
                };
                let Some(name) = values.first() else {
                    return;
                };
                let Some(game) = self.find_game(guild_id, name) else {
                    return;
                };
                let (embed, components) =
                    dashboard::build_tracked_manage(guild_id, &self.tracker, &game);
                let _ = mc
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::UpdateMessage(
                            CreateInteractionResponseMessage::new()
                                .embed(embed)
                                .components(components),
                        ),
                    )
                    .await;
            }
            "dash:channel_select" => {
                let ComponentInteractionDataKind::ChannelSelect { values } = &mc.data.kind else {
                    return;
                };
                let Some(channel_id) = values.first() else {
                    return;
                };
                let _ = self
                    .tracker
                    .settings()
                    .edit_guild(guild_id, |g| g.channel_id = Some(channel_id.get()));
                let (embed, components) =
                    dashboard::build_main_dashboard(guild_id, &self.tracker);
                let _ = mc
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::UpdateMessage(
                            CreateInteractionResponseMessage::new()
                                .embed(embed)
                                .components(components),
                        ),
                    )
                    .await;
            }
            "dash:btn:home" | "dash:btn:refresh" => {
                let (embed, components) =
                    dashboard::build_main_dashboard(guild_id, &self.tracker);
                let _ = mc
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::UpdateMessage(
                            CreateInteractionResponseMessage::new()
                                .embed(embed)
                                .components(components),
                        ),
                    )
                    .await;
            }
            "dash:btn:toggle_first_seen" => {
                let config = self.tracker.config();
                let _ = self.tracker.settings().edit_guild(guild_id, |g| {
                    let current = g
                        .announce_on_first_seen
                        .unwrap_or(config.tracker.announce_on_first_seen);
                    g.announce_on_first_seen = Some(!current);
                });
                let (embed, components) =
                    dashboard::build_main_dashboard(guild_id, &self.tracker);
                let _ = mc
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::UpdateMessage(
                            CreateInteractionResponseMessage::new()
                                .embed(embed)
                                .components(components),
                        ),
                    )
                    .await;
            }
            "dash:btn:toggle_raw" => {
                let config = self.tracker.config();
                let _ = self.tracker.settings().edit_guild(guild_id, |g| {
                    let current = g
                        .attach_raw_response
                        .unwrap_or(config.tracker.attach_raw_response);
                    g.attach_raw_response = Some(!current);
                });
                let (embed, components) =
                    dashboard::build_main_dashboard(guild_id, &self.tracker);
                let _ = mc
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::UpdateMessage(
                            CreateInteractionResponseMessage::new()
                                .embed(embed)
                                .components(components),
                        ),
                    )
                    .await;
            }
            "dash:btn:cycle_poll" => {
                let current = self.tracker.poll_interval();
                let next = match current {
                    30 => 60,
                    60 => 120,
                    120 => 300,
                    300 => 600,
                    _ => 30,
                };
                let _ = self.tracker.settings().set_poll_interval(next);
                let (embed, components) =
                    dashboard::build_main_dashboard(guild_id, &self.tracker);
                let _ = mc
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::UpdateMessage(
                            CreateInteractionResponseMessage::new()
                                .embed(embed)
                                .components(components),
                        ),
                    )
                    .await;
            }
            "dash:btn:view_status" => {
                let (embed, components) = dashboard::build_status_view(&self.tracker);
                let _ = mc
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::UpdateMessage(
                            CreateInteractionResponseMessage::new()
                                .embed(embed)
                                .components(components),
                        ),
                    )
                    .await;
            }
            "dash:btn:add_all_presets" => {
                if let Err(e) = mc
                    .create_response(&ctx.http, CreateInteractionResponse::Acknowledge)
                    .await
                {
                    error!(error = ?e, "failed to acknowledge interaction");
                    return;
                }
                let tracked = self.games(guild_id);
                let untracked = catalog::not_yet_tracked(&tracked);
                for known in untracked {
                    let _ = self
                        .try_add(
                            guild_id,
                            known.name,
                            known.product_id,
                            known.deployment_id,
                            None,
                        )
                        .await;
                }
                let (embed, components) =
                    dashboard::build_main_dashboard(guild_id, &self.tracker);
                let _ = mc
                    .edit_response(
                        &ctx.http,
                        EditInteractionResponse::new()
                            .embed(embed)
                            .components(components),
                    )
                    .await;
            }
            id if id.starts_with("dash:btn:track_preset:") => {
                let name = &id["dash:btn:track_preset:".len()..];
                let Some(known) = catalog::find(name) else {
                    return;
                };
                if let Err(e) = mc
                    .create_response(&ctx.http, CreateInteractionResponse::Acknowledge)
                    .await
                {
                    error!(error = ?e, "failed to acknowledge interaction");
                    return;
                }
                let _ = self
                    .try_add(
                        guild_id,
                        known.name,
                        known.product_id,
                        known.deployment_id,
                        None,
                    )
                    .await;
                let (embed, components) =
                    dashboard::build_preset_insight(guild_id, &self.tracker, known, None);
                let _ = mc
                    .edit_response(
                        &ctx.http,
                        EditInteractionResponse::new()
                            .embed(embed)
                            .components(components),
                    )
                    .await;
            }
            id if id.starts_with("dash:btn:untrack_preset:") => {
                let name = &id["dash:btn:untrack_preset:".len()..];
                let Some(game) = self.find_game(guild_id, name) else {
                    return;
                };
                let seed = self.games(guild_id);
                let _ = self.tracker.settings().edit_guild(guild_id, |g| {
                    if g.games.is_empty() {
                        g.games = seed;
                    }
                    g.games.retain(|existing| existing.name != game.name);
                });
                if let Some(known) = catalog::find(name) {
                    let (embed, components) =
                        dashboard::build_preset_insight(guild_id, &self.tracker, known, None);
                    let _ = mc
                        .create_response(
                            &ctx.http,
                            CreateInteractionResponse::UpdateMessage(
                                CreateInteractionResponseMessage::new()
                                    .embed(embed)
                                    .components(components),
                            ),
                        )
                        .await;
                } else {
                    let (embed, components) =
                        dashboard::build_main_dashboard(guild_id, &self.tracker);
                    let _ = mc
                        .create_response(
                            &ctx.http,
                            CreateInteractionResponse::UpdateMessage(
                                CreateInteractionResponseMessage::new()
                                    .embed(embed)
                                    .components(components),
                            ),
                        )
                        .await;
                }
            }
            id if id.starts_with("dash:btn:probe_preset:") => {
                let name = &id["dash:btn:probe_preset:".len()..];
                let Some(known) = catalog::find(name) else {
                    return;
                };
                if let Err(e) = mc
                    .create_response(&ctx.http, CreateInteractionResponse::Acknowledge)
                    .await
                {
                    error!(error = ?e, "failed to acknowledge interaction");
                    return;
                }
                let candidates: Vec<String> = eac::CANDIDATE_PLATFORMS
                    .iter()
                    .map(|p| (*p).to_string())
                    .collect();
                let probes = self
                    .tracker
                    .probe_platforms(known.product_id, known.deployment_id, &candidates)
                    .await;
                let probe_pairs: Vec<(String, Result<eac::Probe, String>)> = candidates
                    .into_iter()
                    .zip(probes.into_iter().map(|res| res.map_err(|e| e.to_string())))
                    .collect();
                let (embed, components) = dashboard::build_preset_insight(
                    guild_id,
                    &self.tracker,
                    known,
                    Some(&probe_pairs),
                );
                let _ = mc
                    .edit_response(
                        &ctx.http,
                        EditInteractionResponse::new()
                            .embed(embed)
                            .components(components),
                    )
                    .await;
            }
            id if id.starts_with("dash:btn:check_game:") => {
                let name = &id["dash:btn:check_game:".len()..];
                let Some(game) = self.find_game(guild_id, name) else {
                    return;
                };
                if let Err(e) = mc
                    .create_response(&ctx.http, CreateInteractionResponse::Acknowledge)
                    .await
                {
                    error!(error = ?e, "failed to acknowledge interaction");
                    return;
                }
                for platform in &game.platforms {
                    let _ = self.tracker.check(&game, platform).await;
                }
                let (embed, components) =
                    dashboard::build_tracked_manage(guild_id, &self.tracker, &game);
                let _ = mc
                    .edit_response(
                        &ctx.http,
                        EditInteractionResponse::new()
                            .embed(embed)
                            .components(components),
                    )
                    .await;
            }
            id if id.starts_with("dash:btn:remove_game:") => {
                let name = &id["dash:btn:remove_game:".len()..];
                let Some(game) = self.find_game(guild_id, name) else {
                    return;
                };
                let seed = self.games(guild_id);
                let _ = self.tracker.settings().edit_guild(guild_id, |g| {
                    if g.games.is_empty() {
                        g.games = seed;
                    }
                    g.games.retain(|existing| existing.name != game.name);
                });
                let (embed, components) =
                    dashboard::build_main_dashboard(guild_id, &self.tracker);
                let _ = mc
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::UpdateMessage(
                            CreateInteractionResponseMessage::new()
                                .embed(embed)
                                .components(components),
                        ),
                    )
                    .await;
            }
            _ => {}
        }
    }

    async fn reply_remove(
        &self,
        ctx: &Context,
        cmd: &CommandInteraction,
        guild_id: u64,
        options: &[CommandDataOption],
    ) -> serenity::Result<()> {
        let name = string_option(options, "game").unwrap_or_default();
        let Some(game) = self.find_game(guild_id, &name) else {
            return respond(ctx, cmd, format!("No tracked game matches `{name}`.")).await;
        };

        let seed = self.games(guild_id);
        let result = self.tracker.settings().edit_guild(guild_id, |g| {
            if g.games.is_empty() {
                g.games = seed;
            }
            g.games.retain(|existing| existing.name != game.name);
        });

        match result {
            Ok(()) => respond(ctx, cmd, format!("Stopped tracking **{}**.", game.name)).await,
            Err(e) => {
                error!(error = ?e, "failed to save settings");
                respond(ctx, cmd, format!("Could not save that: {e}")).await
            }
        }
    }

    fn announce_channel(&self, guild_id: u64) -> Option<u64> {
        self.tracker.settings().guild(guild_id).channel_id.or(self
            .tracker
            .config()
            .discord
            .channel_id)
    }

    async fn reply_list(
        &self,
        ctx: &Context,
        cmd: &CommandInteraction,
        guild_id: u64,
    ) -> serenity::Result<()> {
        let games = self.games(guild_id);
        if games.is_empty() {
            return respond(
                ctx,
                cmd,
                "Nothing tracked yet. Add a game with `/eac add`, then pick a channel with `/eac setup`.",
            )
            .await;
        }
        let body = games
            .iter()
            .map(|g| {
                format!(
                    "**{}** — `{}` / `{}` → {}",
                    g.name,
                    g.product_id,
                    g.deployment_id,
                    g.platforms
                        .iter()
                        .map(|p| format!("`{p}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        respond(ctx, cmd, truncate_message(&body, "Nothing configured.")).await
    }

    async fn reply_config(
        &self,
        ctx: &Context,
        cmd: &CommandInteraction,
        guild_id: u64,
    ) -> serenity::Result<()> {
        let guild = self.tracker.settings().guild(guild_id);
        let config = self.tracker.config();
        let channel = match self.announce_channel(guild_id) {
            Some(id) => format!("<#{id}>"),
            None => "*not set — run `/eac setup`*".to_string(),
        };

        let body = format!(
            "Channel: {channel}\n\
             Games tracked: {}\n\
             Poll interval: {} seconds\n\
             `announce_on_first_seen`: {}\n\
             `attach_raw_response`: {}",
            self.games(guild_id).len(),
            self.tracker.poll_interval(),
            guild
                .announce_on_first_seen
                .unwrap_or(config.tracker.announce_on_first_seen),
            guild
                .attach_raw_response
                .unwrap_or(config.tracker.attach_raw_response),
        );
        respond(ctx, cmd, body).await
    }

    async fn reply_status(&self, ctx: &Context, cmd: &CommandInteraction) -> serenity::Result<()> {
        let lines = self.tracker.status_lines().join("\n");
        respond(
            ctx,
            cmd,
            truncate_message(&lines, "No targets have been seen yet."),
        )
        .await
    }

    async fn reply_set(
        &self,
        ctx: &Context,
        cmd: &CommandInteraction,
        guild_id: u64,
        options: &[CommandDataOption],
    ) -> serenity::Result<()> {
        let option = string_option(options, "option").unwrap_or_default();
        let raw = string_option(options, "value").unwrap_or_default();
        let value = raw.trim();

        let parse_bool = |v: &str| match v.to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Some(true),
            "false" | "no" | "off" | "0" => Some(false),
            _ => None,
        };

        let outcome = match option.as_str() {
            "announce_on_first_seen" | "attach_raw_response" => {
                let Some(parsed) = parse_bool(value) else {
                    return respond(ctx, cmd, format!("`{value}` is not true or false.")).await;
                };
                let is_announce = option == "announce_on_first_seen";
                self.tracker
                    .settings()
                    .edit_guild(guild_id, |g| {
                        if is_announce {
                            g.announce_on_first_seen = Some(parsed);
                        } else {
                            g.attach_raw_response = Some(parsed);
                        }
                    })
                    .map(|()| format!("`{option}` is now **{parsed}**."))
            }
            "poll_interval_secs" => match value.parse::<u64>() {
                Ok(secs) if secs >= MIN_POLL_INTERVAL_SECS => self
                    .tracker
                    .settings()
                    .set_poll_interval(secs)
                    .map(|()| format!("Polling every **{secs}** seconds (bot-wide).")),
                Ok(_) => {
                    return respond(
                        ctx,
                        cmd,
                        format!("Minimum is {MIN_POLL_INTERVAL_SECS} seconds, to stay polite to the CDN."),
                    )
                    .await;
                }
                Err(_) => {
                    return respond(ctx, cmd, format!("`{value}` is not a number.")).await;
                }
            },
            other => {
                return respond(ctx, cmd, format!("Unknown option `{other}`.")).await;
            }
        };

        match outcome {
            Ok(message) => respond(ctx, cmd, message).await,
            Err(e) => {
                error!(error = ?e, "failed to save settings");
                respond(ctx, cmd, format!("Could not save that: {e}")).await
            }
        }
    }

    async fn reply_check(
        &self,
        ctx: &Context,
        cmd: &CommandInteraction,
        guild_id: u64,
        options: &[CommandDataOption],
    ) -> serenity::Result<()> {
        let name = string_option(options, "game").unwrap_or_default();
        let Some(game) = self.find_game(guild_id, &name) else {
            return respond(ctx, cmd, format!("No tracked game matches `{name}`.")).await;
        };

        let requested = string_option(options, "platform");
        let platforms: Vec<String> = match requested {
            Some(p) if !p.trim().is_empty() => vec![p.trim().to_ascii_lowercase()],
            _ => game.platforms.clone(),
        };

        // Fetching can take seconds; acknowledge inside Discord's 3s window.
        cmd.defer(&ctx.http).await?;

        let max_part = self.tracker.config().tracker.max_attachment_bytes;
        let mut embeds: Vec<CreateEmbed> = Vec::new();
        let mut errors: Vec<String> = Vec::new();

        // 10 is Discord's per-message embed cap.
        for platform in platforms.iter().take(10) {
            match self.tracker.check(&game, platform).await {
                Ok(outcome) => embeds.push(embed::build_check_embed(
                    &game,
                    platform,
                    &outcome.snapshot,
                    outcome.changed,
                    outcome.diff.as_ref(),
                    max_part,
                )),
                Err(e) => errors.push(format!("`{platform}`: {e}")),
            }
        }

        let mut response = EditInteractionResponse::new().embeds(embeds);
        if !errors.is_empty() {
            response = response.content(truncate_message(
                &format!("Some checks failed:\n{}", errors.join("\n")),
                "",
            ));
        }
        cmd.edit_response(&ctx.http, response).await?;
        Ok(())
    }

    async fn handle_autocomplete(&self, ctx: &Context, ac: &CommandInteraction) {
        let partial = ac
            .data
            .autocomplete()
            .map(|opt| opt.value.to_lowercase())
            .unwrap_or_default();

        let choices: Vec<AutocompleteChoice> = ac
            .guild_id
            .map(|g| self.games(g.get()))
            .unwrap_or_default()
            .into_iter()
            .filter(|g| g.name.to_lowercase().contains(&partial))
            .take(25)
            .map(|g| AutocompleteChoice::new(g.name.clone(), g.name))
            .collect();

        let response = CreateInteractionResponse::Autocomplete(
            CreateAutocompleteResponse::new().set_choices(choices),
        );
        if let Err(e) = ac.create_response(&ctx.http, response).await {
            error!(error = ?e, "failed to answer autocomplete");
        }
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        info!(bot = %ready.user.name, guilds = ready.guilds.len(), "connected to Discord");

        let registration = match self.tracker.config().discord.guild_id {
            Some(id) => GuildId::new(id)
                .create_command(&ctx.http, Handler::command())
                .await
                .map(|_| "guild"),
            None => serenity::all::Command::create_global_command(&ctx.http, Handler::command())
                .await
                .map(|_| "global"),
        };
        match registration {
            Ok(scope) => info!(scope, "registered /eac"),
            // Command registration is not fatal: polling still works.
            Err(e) => error!(error = ?e, "failed to register /eac"),
        }

        if !self.loop_started.swap(true, Ordering::SeqCst) {
            let tracker = Arc::clone(&self.tracker);
            let http = Arc::clone(&ctx.http);
            tokio::spawn(async move { tracker.poll_loop(http).await });
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::Command(cmd) if cmd.data.name == "eac" => {
                self.handle_command(&ctx, &cmd).await;
            }
            Interaction::Autocomplete(ac) if ac.data.name == "eac" => {
                self.handle_autocomplete(&ctx, &ac).await;
            }
            Interaction::Component(mc) if mc.data.custom_id.starts_with("dash:") => {
                self.handle_dashboard_interaction(&ctx, &mc).await;
            }
            Interaction::Component(mc) if mc.data.custom_id == SELECT_KNOWN => {
                self.handle_selection(&ctx, &mc).await;
            }
            _ => {}
        }
    }
}

/// Whether the invoking member may reconfigure the tracker.
///
/// Discord resolves the member's effective permissions into the interaction,
/// so this needs no extra lookup. Absent permissions are treated as denied.
fn can_manage(cmd: &CommandInteraction) -> bool {
    permits_manage(cmd.member.as_ref().and_then(|m| m.permissions))
}

/// Discord resolves the member's effective permissions into the interaction,
/// so this needs no extra lookup. Absent permissions are treated as denied.
fn permits_manage(permissions: Option<Permissions>) -> bool {
    permissions.is_some_and(|p| {
        p.contains(Permissions::MANAGE_GUILD) || p.contains(Permissions::ADMINISTRATOR)
    })
}

fn string_option(options: &[CommandDataOption], name: &str) -> Option<String> {
    options
        .iter()
        .find(|o| o.name == name)
        .and_then(|o| match &o.value {
            CommandDataOptionValue::String(s) => Some(s.clone()),
            _ => None,
        })
}

/// Clamp to Discord's 2000-character message limit, substituting `fallback`
/// when there is nothing to say.
fn truncate_message(body: &str, fallback: &str) -> String {
    if body.trim().is_empty() {
        return fallback.to_string();
    }
    if body.chars().count() <= 2000 {
        return body.to_string();
    }
    body.chars().take(1997).collect::<String>() + "..."
}

/// Replace a deferred response.
async fn edit(
    ctx: &Context,
    cmd: &CommandInteraction,
    content: impl Into<String>,
) -> serenity::Result<()> {
    cmd.edit_response(
        &ctx.http,
        EditInteractionResponse::new().content(truncate_message(&content.into(), "Done.")),
    )
    .await
    .map(|_| ())
}

async fn respond(
    ctx: &Context,
    cmd: &CommandInteraction,
    content: impl Into<String>,
) -> serenity::Result<()> {
    cmd.create_response(
        &ctx.http,
        CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .content(content)
                .ephemeral(true),
        ),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_message_substitutes_fallback() {
        assert_eq!(truncate_message("   ", "nothing"), "nothing");
        assert_eq!(truncate_message("hi", "nothing"), "hi");
    }

    #[test]
    fn truncate_message_respects_the_character_limit() {
        let long = "a".repeat(5000);
        let out = truncate_message(&long, "");
        assert_eq!(out.chars().count(), 2000);
        assert!(out.ends_with("..."));
    }

    #[test]
    fn every_settable_option_is_handled() {
        // The command's choice list and the handler must not drift apart.
        for (name, _) in SETTABLE {
            assert!(
                matches!(
                    *name,
                    "announce_on_first_seen" | "attach_raw_response" | "poll_interval_secs"
                ),
                "no handler branch for {name}"
            );
        }
    }

    fn outcome(name: &str, replaced: bool) -> AddOutcome {
        AddOutcome {
            game: Game {
                name: name.into(),
                product_id: "p".into(),
                deployment_id: "d".into(),
                platforms: vec!["wow64_win64".into()],
            },
            replaced,
            live: vec![("wow64_win64".into(), Some(23_068_672))],
            rejected: vec!["`mac64` — HTTP 404".into()],
        }
    }

    #[test]
    fn describes_a_new_and_an_updated_game() {
        let added = outcome("Rust", false).describe(false);
        assert!(added.starts_with("Now tracking **Rust**"), "got {added}");
        assert!(added.contains("`wow64_win64` (22.0 MB)"), "got {added}");

        let updated = outcome("Rust", true).describe(false);
        assert!(updated.starts_with("Updated **Rust**"), "got {updated}");
    }

    #[test]
    fn skipped_platforms_are_reported_only_when_they_were_asked_for() {
        // Detecting: most candidates not matching is expected, not news.
        assert!(!outcome("Rust", false).describe(false).contains("mac64"));
        // Explicitly requested: the reader needs to know one was dropped.
        assert!(outcome("Rust", false).describe(true).contains("mac64"));
    }

    #[test]
    fn select_menu_descriptions_stay_short() {
        assert_eq!(
            truncate_id("429c2212ad284866aee071454c2125b5"),
            "429c2212ad28…"
        );
        assert_eq!(
            truncate_id("prod-fn"),
            "prod-fn",
            "short ids are left alone"
        );
    }

    #[test]
    fn manage_permission_is_required_and_absence_is_denial() {
        assert!(permits_manage(Some(Permissions::MANAGE_GUILD)));
        assert!(permits_manage(Some(Permissions::ADMINISTRATOR)));
        assert!(!permits_manage(Some(Permissions::SEND_MESSAGES)));
        assert!(!permits_manage(None), "unknown permissions must not pass");
    }

    #[test]
    fn the_command_fits_discords_schema() {
        // Discord rejects a command with more than 25 options outright.
        let json = serde_json::to_value(Handler::command()).unwrap();
        let options = json["options"].as_array().expect("subcommands");
        assert!(options.len() <= 25, "too many subcommands");

        let names: Vec<&str> = options
            .iter()
            .map(|o| o["name"].as_str().unwrap())
            .collect();
        for expected in [
            "dashboard", "setup", "add", "browse", "remove", "list", "config", "status", "check", "set",
        ] {
            assert!(names.contains(&expected), "missing /eac {expected}");
        }

        // Every subcommand routed by handle_command must exist on the command,
        // and every subcommand on the command must be routed.
        assert_eq!(names.len(), 10, "a subcommand was added without a route");
    }

    #[test]
    fn catalogue_games_can_be_added_without_ids() {
        // reply_add falls back to the catalogue, so a browse name must resolve.
        for known in catalog::KNOWN {
            assert!(catalog::find(known.name).is_some());
        }
    }
}

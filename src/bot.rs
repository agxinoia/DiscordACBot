//! Discord gateway wiring and the `/eac` command surface.
//!
//! Everything a server needs is configured here rather than in a file: the
//! announce channel, which games to track, and the per-guild announcement
//! options. The operator supplies only a bot token.

use crate::config::Game;
use crate::embed;
use crate::settings::{self, MIN_POLL_INTERVAL_SECS};
use crate::tracker::Tracker;
use serenity::all::{
    AutocompleteChoice, ChannelType, CommandDataOption, CommandDataOptionValue, CommandInteraction,
    CommandOptionType, Context, CreateAutocompleteResponse, CreateCommand, CreateCommandOption,
    CreateInteractionResponse, CreateInteractionResponseMessage, EditInteractionResponse,
    EventHandler, GuildId, Interaction, Permissions, Ready,
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
                    CreateCommandOption::new(CommandOptionType::String, "game", "Display name")
                        .required(true),
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "product_id",
                        "EAC product id from the game's EasyAntiCheat settings",
                    )
                    .required(true),
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "deployment_id",
                        "EAC deployment id",
                    )
                    .required(true),
                )
                .add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "platforms",
                        "Comma separated, e.g. win64 or win64, win32",
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
        let mutating = matches!(sub.name.as_str(), "setup" | "add" | "remove" | "set");
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
            "setup" => self.reply_setup(ctx, cmd, guild_id, options).await,
            "add" => self.reply_add(ctx, cmd, guild_id, options).await,
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

    async fn reply_add(
        &self,
        ctx: &Context,
        cmd: &CommandInteraction,
        guild_id: u64,
        options: &[CommandDataOption],
    ) -> serenity::Result<()> {
        let name = string_option(options, "game").unwrap_or_default();
        let product_id = string_option(options, "product_id").unwrap_or_default();
        let deployment_id = string_option(options, "deployment_id").unwrap_or_default();
        let platforms_raw =
            string_option(options, "platforms").unwrap_or_else(|| "win64".to_string());

        // Validate before touching stored state: these values are interpolated
        // straight into a CDN URL.
        let validated = settings::validate_name(&name)
            .and_then(|()| settings::validate_id("product_id", product_id.trim()))
            .and_then(|()| settings::validate_id("deployment_id", deployment_id.trim()))
            .and_then(|()| settings::parse_platforms(&platforms_raw));

        let platforms = match validated {
            Ok(platforms) => platforms,
            Err(e) => return respond(ctx, cmd, format!("{e}")).await,
        };

        // Validating means a round trip per platform, which can exceed
        // Discord's 3s window.
        if let Err(e) = cmd.defer_ephemeral(&ctx.http).await {
            error!(error = ?e, "failed to defer");
            return Ok(());
        }

        let product_id = product_id.trim().to_string();
        let deployment_id = deployment_id.trim().to_string();

        // Confirm the ids are real before storing them: a wrong pair would
        // otherwise sit in the config as a silently dead target.
        let probes = self
            .tracker
            .probe_platforms(&product_id, &deployment_id, &platforms)
            .await;

        let mut live = Vec::new();
        let mut rejected = Vec::new();
        for (platform, probe) in platforms.iter().zip(probes) {
            match probe {
                Ok(p) if p.ok() => live.push((platform.clone(), p.content_length)),
                Ok(p) => rejected.push(format!("`{platform}` — HTTP {}", p.status)),
                Err(e) => rejected.push(format!("`{platform}` — {e:#}")),
            }
        }

        if live.is_empty() {
            let detail = rejected.join("\n");
            return edit(
                ctx,
                cmd,
                format!(
                    "Nothing is published for those ids, so nothing was added:\n{detail}\n\n                     Check `product_id` and `deployment_id` against the game's                      EasyAntiCheat config, and try `platforms:win64`."
                ),
            )
            .await;
        }

        let game = Game {
            name: name.trim().to_string(),
            product_id,
            deployment_id,
            platforms: live.iter().map(|(p, _)| p.clone()).collect(),
        };

        // Seed from the config fallback so the first /eac add does not
        // silently drop an operator-configured list.
        let seed = self.games(guild_id);
        let replaced = self.tracker.settings().edit_guild(guild_id, |g| {
            if g.games.is_empty() {
                g.games = seed;
            }
            let existing = g
                .games
                .iter()
                .position(|existing| existing.name.eq_ignore_ascii_case(&game.name));
            match existing {
                Some(i) => {
                    g.games[i] = game.clone();
                    true
                }
                None => {
                    g.games.push(game.clone());
                    false
                }
            }
        });

        match replaced {
            Ok(replaced) => {
                let verb = if replaced { "Updated" } else { "Now tracking" };
                let confirmed = live
                    .iter()
                    .map(|(platform, size)| match size {
                        Some(bytes) => {
                            format!("`{platform}` ({})", embed::human_bytes(*bytes))
                        }
                        None => format!("`{platform}`"),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");

                let mut message = format!("{verb} **{}** — verified {confirmed}.", game.name);
                if !rejected.is_empty() {
                    message.push_str(&format!(
                        "\n\nNot published, so skipped:\n{}",
                        rejected.join("\n")
                    ));
                }
                if self.announce_channel(guild_id).is_none() {
                    message.push_str("\n\nNo announce channel is set yet — run `/eac setup`.");
                }
                edit(ctx, cmd, message).await
            }
            Err(e) => {
                error!(error = ?e, "failed to save settings");
                edit(ctx, cmd, format!("Could not save that: {e}")).await
            }
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
            _ => {}
        }
    }
}

/// Whether the invoking member may reconfigure the tracker.
///
/// Discord resolves the member's effective permissions into the interaction,
/// so this needs no extra lookup. Absent permissions are treated as denied.
fn can_manage(cmd: &CommandInteraction) -> bool {
    cmd.member
        .as_ref()
        .and_then(|m| m.permissions)
        .is_some_and(|p| {
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
}

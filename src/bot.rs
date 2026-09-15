//! Discord gateway wiring: registers `/eac` and drives the poll loop.

use crate::tracker::Tracker;
use crate::{config::Game, embed};
use serenity::all::{
    AutocompleteChoice, CommandDataOptionValue, CommandInteraction, CommandOptionType, Context,
    CreateAutocompleteResponse, CreateCommand, CreateCommandOption, CreateInteractionResponse,
    CreateInteractionResponseMessage, EditInteractionResponse, EventHandler, GuildId, Interaction,
    Ready,
};
use serenity::async_trait;
use serenity::builder::CreateEmbed;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{error, info};

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
            .add_option(CreateCommandOption::new(
                CommandOptionType::SubCommand,
                "list",
                "List the games and platforms being tracked",
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
    }

    /// Case-insensitive lookup, also accepting a unique prefix so autocomplete
    /// text that was edited by hand still resolves.
    fn find_game<'a>(&'a self, needle: &str) -> Option<&'a Game> {
        let needle = needle.trim().to_lowercase();
        let games = &self.tracker.config().games;
        games
            .iter()
            .find(|g| g.name.to_lowercase() == needle)
            .or_else(|| {
                let mut matches = games
                    .iter()
                    .filter(|g| g.name.to_lowercase().starts_with(&needle));
                let first = matches.next()?;
                matches.next().is_none().then_some(first)
            })
    }

    async fn handle_command(&self, ctx: &Context, cmd: &CommandInteraction) {
        let Some(sub) = cmd.data.options.first() else {
            return;
        };
        let sub_options = match &sub.value {
            CommandDataOptionValue::SubCommand(options) => options.as_slice(),
            _ => &[],
        };

        let result = match sub.name.as_str() {
            "list" => self.reply_list(ctx, cmd).await,
            "status" => self.reply_status(ctx, cmd).await,
            "check" => self.reply_check(ctx, cmd, sub_options).await,
            other => {
                error!(subcommand = %other, "unknown subcommand");
                Ok(())
            }
        };
        if let Err(e) = result {
            error!(error = ?e, "failed to respond to interaction");
        }
    }

    async fn reply_list(&self, ctx: &Context, cmd: &CommandInteraction) -> serenity::Result<()> {
        let body = self
            .tracker
            .config()
            .games
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

    async fn reply_status(&self, ctx: &Context, cmd: &CommandInteraction) -> serenity::Result<()> {
        let lines = self.tracker.status_lines().join("\n");
        respond(
            ctx,
            cmd,
            truncate_message(&lines, "No targets have been seen yet."),
        )
        .await
    }

    async fn reply_check(
        &self,
        ctx: &Context,
        cmd: &CommandInteraction,
        options: &[serenity::all::CommandDataOption],
    ) -> serenity::Result<()> {
        let name = string_option(options, "game").unwrap_or_default();
        let Some(game) = self.find_game(&name) else {
            return respond(ctx, cmd, format!("No tracked game matches `{name}`.")).await;
        };

        let requested = string_option(options, "platform");
        let platforms: Vec<String> = match requested {
            Some(p) if !p.trim().is_empty() => vec![p.trim().to_string()],
            _ => game.platforms.clone(),
        };

        // Fetching can take seconds; acknowledge inside Discord's 3s window.
        cmd.defer(&ctx.http).await?;

        let max_part = self.tracker.config().tracker.max_attachment_bytes;
        let mut embeds: Vec<CreateEmbed> = Vec::new();
        let mut errors: Vec<String> = Vec::new();

        // 10 is Discord's per-message embed cap.
        for platform in platforms.iter().take(10) {
            match self.tracker.check(game, platform).await {
                Ok(outcome) => embeds.push(embed::build_check_embed(
                    game,
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

    async fn handle_autocomplete(&self, ctx: &Context, ac: &serenity::all::CommandInteraction) {
        let partial = ac
            .data
            .autocomplete()
            .map(|opt| opt.value.to_lowercase())
            .unwrap_or_default();

        let choices: Vec<AutocompleteChoice> = self
            .tracker
            .config()
            .games
            .iter()
            .filter(|g| g.name.to_lowercase().contains(&partial))
            .take(25)
            .map(|g| AutocompleteChoice::new(g.name.clone(), g.name.clone()))
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
        info!(bot = %ready.user.name, "connected to Discord");

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

fn string_option(options: &[serenity::all::CommandDataOption], name: &str) -> Option<String> {
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

async fn respond(
    ctx: &Context,
    cmd: &CommandInteraction,
    content: impl Into<String>,
) -> serenity::Result<()> {
    cmd.create_response(
        &ctx.http,
        CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new().content(content),
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
}

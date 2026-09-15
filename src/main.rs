//! A Discord bot that watches Easy Anti-Cheat module endpoints on the Epic
//! Games CDN and posts an embed whenever a game's modules change.

use anyhow::{Context, Result};
use eac_tracker::{bot, config, settings, tracker};
use serenity::all::GatewayIntents;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("EAC_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config_path = std::env::var("EAC_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("config.toml"));

    // A config file is optional: the token alone is enough to start, and
    // channels and games are configured from Discord with /eac.
    let config = Arc::new(config::Config::load_or_default(&config_path)?);
    info!(
        config = %config_path.display(),
        present = config_path.exists(),
        "loaded configuration"
    );

    let settings = Arc::new(settings::SettingsStore::load(std::path::Path::new(
        &config.tracker.settings_path,
    ))?);
    let tracker = Arc::new(tracker::Tracker::new(
        Arc::clone(&config),
        Arc::clone(&settings),
    )?);

    // Posting embeds and running slash commands need no privileged intents.
    let mut client = serenity::Client::builder(config.token(), GatewayIntents::empty())
        .event_handler(bot::Handler::new(tracker))
        .await
        .context("building Discord client")?;

    let shard_manager = client.shard_manager.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            info!("shutting down");
            shard_manager.shutdown_all().await;
        }
    });

    client.start().await.context("running Discord client")?;
    Ok(())
}

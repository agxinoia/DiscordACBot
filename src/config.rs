//! Configuration loading.
//!
//! The bot reads a TOML file (default `config.toml`, override with `EAC_CONFIG`).
//! The Discord token is deliberately *not* required to live in that file: the
//! `DISCORD_TOKEN` environment variable takes precedence so the config can be
//! committed or shared without leaking credentials.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub discord: Discord,
    #[serde(default)]
    pub tracker: Tracker,
    #[serde(default)]
    pub games: Vec<Game>,
}

#[derive(Debug, Deserialize)]
pub struct Discord {
    /// Optional. `DISCORD_TOKEN` wins when both are set.
    #[serde(default)]
    pub token: Option<String>,
    /// Channel that update embeds are posted to.
    pub channel_id: u64,
    /// Register the `/eac` slash command to this guild only. Guild-scoped
    /// commands appear instantly; global ones can take an hour to propagate.
    #[serde(default)]
    pub guild_id: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct Tracker {
    #[serde(default = "defaults::poll_interval_secs")]
    pub poll_interval_secs: u64,
    #[serde(default = "defaults::request_timeout_secs")]
    pub request_timeout_secs: u64,
    /// Post an embed the first time a target is seen. Off by default so a
    /// fresh deployment does not spam the channel with one embed per target.
    #[serde(default)]
    pub announce_on_first_seen: bool,
    /// Attach the raw CDN response to the update embed.
    #[serde(default = "defaults::attach_raw_response")]
    pub attach_raw_response: bool,
    /// Upload chunk size. Discord's default per-file limit is 10 MB, so the
    /// raw response is split into parts that each stay under this.
    #[serde(default = "defaults::max_attachment_bytes")]
    pub max_attachment_bytes: u64,
    #[serde(default = "defaults::state_path")]
    pub state_path: String,
    #[serde(default = "defaults::user_agent")]
    pub user_agent: String,
    /// Thumbnail shown on the embed. Empty string disables it.
    #[serde(default)]
    pub thumbnail_url: Option<String>,
    /// Override the CDN base URL (mirrors, local testing). Defaults to
    /// [`crate::eac::CDN_BASE`].
    #[serde(default)]
    pub cdn_base: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Game {
    pub name: String,
    pub product_id: String,
    pub deployment_id: String,
    /// e.g. `["win64", "win32"]`.
    pub platforms: Vec<String>,
}

mod defaults {
    pub fn poll_interval_secs() -> u64 {
        300
    }
    pub fn request_timeout_secs() -> u64 {
        30
    }
    pub fn attach_raw_response() -> bool {
        true
    }
    pub fn max_attachment_bytes() -> u64 {
        // A little under 10 MB, leaving room for multipart overhead.
        9_500_000
    }
    pub fn state_path() -> String {
        "state.json".to_string()
    }
    pub fn user_agent() -> String {
        concat!("eac-tracker/", env!("CARGO_PKG_VERSION")).to_string()
    }
}

impl Default for Tracker {
    fn default() -> Self {
        Self {
            poll_interval_secs: defaults::poll_interval_secs(),
            request_timeout_secs: defaults::request_timeout_secs(),
            announce_on_first_seen: false,
            attach_raw_response: defaults::attach_raw_response(),
            max_attachment_bytes: defaults::max_attachment_bytes(),
            state_path: defaults::state_path(),
            user_agent: defaults::user_agent(),
            thumbnail_url: None,
            cdn_base: None,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config at {}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;

        if let Ok(token) = std::env::var("DISCORD_TOKEN")
            && !token.trim().is_empty()
        {
            cfg.discord.token = Some(token);
        }
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if self
            .discord
            .token
            .as_deref()
            .unwrap_or("")
            .trim()
            .is_empty()
        {
            bail!("no Discord token: set DISCORD_TOKEN or discord.token in the config");
        }
        if self.discord.channel_id == 0 {
            bail!("discord.channel_id must be set");
        }
        if self.games.is_empty() {
            bail!("no [[games]] configured — nothing to track");
        }
        for game in &self.games {
            if game.platforms.is_empty() {
                bail!("game '{}' lists no platforms", game.name);
            }
        }
        if self.tracker.poll_interval_secs < 30 {
            bail!("tracker.poll_interval_secs must be at least 30 to stay polite to the CDN");
        }
        if self.tracker.max_attachment_bytes < 1024 {
            bail!("tracker.max_attachment_bytes is implausibly small");
        }
        Ok(())
    }

    pub fn token(&self) -> &str {
        self.discord.token.as_deref().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
[discord]
token = "t"
channel_id = 123

[[games]]
name = "ARC Raiders"
product_id = "9e8b37541e614575b4de303d2c2e44cf"
deployment_id = "35e06571d8ab4de4b98519b624125459"
platforms = ["win64"]
"#;

    fn parse(toml_src: &str) -> Result<Config> {
        let mut cfg: Config = toml::from_str(toml_src)?;
        cfg.discord.token.get_or_insert_with(|| "t".into());
        cfg.validate()?;
        Ok(cfg)
    }

    #[test]
    fn accepts_a_minimal_config_and_applies_defaults() {
        let cfg = parse(VALID).unwrap();
        assert_eq!(cfg.games.len(), 1);
        assert_eq!(cfg.tracker.poll_interval_secs, 300);
        assert!(!cfg.tracker.announce_on_first_seen);
        assert!(cfg.tracker.attach_raw_response);
        assert_eq!(cfg.tracker.state_path, "state.json");
        assert!(cfg.tracker.cdn_base.is_none());
    }

    #[test]
    fn the_shipped_example_config_is_valid() {
        let raw = include_str!("../config.example.toml");
        let cfg: Config = toml::from_str(raw).expect("example config must parse");
        assert!(!cfg.games.is_empty(), "example must configure a game");
    }

    #[test]
    fn rejects_a_config_with_no_games() {
        let src = VALID.split("[[games]]").next().unwrap().to_string();
        assert!(
            parse(&src)
                .unwrap_err()
                .to_string()
                .contains("no [[games]]")
        );
    }

    #[test]
    fn rejects_a_game_with_no_platforms() {
        let src = VALID.replace(r#"platforms = ["win64"]"#, "platforms = []");
        assert!(
            parse(&src)
                .unwrap_err()
                .to_string()
                .contains("no platforms")
        );
    }

    #[test]
    fn rejects_an_impolite_poll_interval() {
        let src = format!("{VALID}\n[tracker]\npoll_interval_secs = 5\n");
        assert!(
            parse(&src)
                .unwrap_err()
                .to_string()
                .contains("poll_interval_secs")
        );
    }

    #[test]
    fn rejects_a_missing_channel() {
        let src = VALID.replace("channel_id = 123", "channel_id = 0");
        assert!(parse(&src).unwrap_err().to_string().contains("channel_id"));
    }

    #[test]
    fn rejects_a_blank_token() {
        let mut cfg: Config = toml::from_str(VALID).unwrap();
        cfg.discord.token = Some("   ".into());
        assert!(cfg.validate().unwrap_err().to_string().contains("token"));
    }
}

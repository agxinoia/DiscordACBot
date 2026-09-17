//! Operator-level configuration.
//!
//! This layer is entirely optional. A deployment needs only `DISCORD_TOKEN`;
//! announce channels and tracked games are configured from Discord and live in
//! [`crate::settings`]. A TOML file (default `config.toml`, override with
//! `EAC_CONFIG`) can still supply paths, timeouts and per-guild fallbacks.
//!
//! The token is deliberately not required to live in that file: the
//! `DISCORD_TOKEN` environment variable takes precedence so the config can be
//! committed or shared without leaking credentials.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub discord: Discord,
    #[serde(default)]
    pub tracker: Tracker,
    #[serde(default)]
    pub games: Vec<Game>,
    #[serde(default)]
    pub ai: AiConfig,
}

#[derive(Debug, Deserialize)]
pub struct Discord {
    /// Optional. `DISCORD_TOKEN` wins when both are set.
    #[serde(default)]
    pub token: Option<String>,
    /// Fallback announce channel for guilds that have not set their own with
    /// `/eac setup`. Optional: the normal path is to configure it in Discord.
    #[serde(default)]
    pub channel_id: Option<u64>,
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
    /// Where guild configuration set through `/eac` is persisted.
    #[serde(default = "defaults::settings_path")]
    pub settings_path: String,
    #[serde(default = "defaults::user_agent")]
    pub user_agent: String,
    /// Thumbnail shown on the embed. Empty string disables it.
    #[serde(default)]
    pub thumbnail_url: Option<String>,
    /// Override the CDN base URL (mirrors, local testing). Defaults to
    /// [`crate::eac::CDN_BASE`].
    #[serde(default)]
    pub cdn_base: Option<String>,
    /// Directory for the payload archive. Set to "" to disable archiving,
    /// which also disables TLSH distance in update embeds, since measuring it
    /// requires the previous payload's bytes.
    #[serde(default = "defaults::archive_path")]
    pub archive_path: String,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct Game {
    pub name: String,
    pub product_id: String,
    pub deployment_id: String,
    /// e.g. `["win64", "win32"]`.
    pub platforms: Vec<String>,
}


#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AiConfig {
    /// Optional default NVIDIA API key.
    #[serde(default)]
    pub nvidia_api_key: Option<String>,
    /// Model to query on NVIDIA NIM (default: "z-ai/glm-5-3-flash").
    #[serde(default = "defaults::ai_model")]
    pub model: String,
    /// Rate limit delay in milliseconds between requests.
    #[serde(default = "defaults::ai_delay_ms")]
    pub delay_ms: u64,
    /// Optional path to Ghidra analyzeHeadless executable.
    #[serde(default)]
    pub ghidra_path: Option<String>,
    /// Whether AI diff analysis is enabled.
    #[serde(default = "defaults::ai_enabled")]
    pub enabled: bool,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            nvidia_api_key: None,
            model: defaults::ai_model(),
            delay_ms: defaults::ai_delay_ms(),
            ghidra_path: None,
            enabled: defaults::ai_enabled(),
        }
    }
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
    pub fn archive_path() -> String {
        "archive".to_string()
    }
    pub fn settings_path() -> String {
        "settings.json".to_string()
    }
    pub fn user_agent() -> String {
        concat!("eac-tracker/", env!("CARGO_PKG_VERSION")).to_string()
    }
    pub fn ai_model() -> String {
        crate::ai::DEFAULT_MODEL.to_string()
    }
    pub fn ai_delay_ms() -> u64 {
        crate::ai::DEFAULT_DELAY_MS
    }
    pub fn ai_enabled() -> bool {
        true
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
            settings_path: defaults::settings_path(),
            user_agent: defaults::user_agent(),
            thumbnail_url: None,
            cdn_base: None,
            archive_path: defaults::archive_path(),
        }
    }
}

impl Config {
    /// Load from `path`, or fall back to defaults when the file is absent.
    /// Only the bot token is required, and it normally comes from the
    /// environment, so a deployment need not have a config file at all.
    pub fn load_or_default(path: &Path) -> Result<Self> {
        if path.exists() {
            return Self::load(path);
        }
        let mut cfg = Config {
            discord: Discord {
                token: None,
                channel_id: None,
                guild_id: None,
            },
            tracker: Tracker::default(),
            games: Vec::new(),
            ai: AiConfig::default(),
        };
        cfg.apply_environment();
        cfg.validate()?;
        Ok(cfg)
    }

    fn apply_environment(&mut self) {
        if let Ok(token) = std::env::var("DISCORD_TOKEN")
            && !token.trim().is_empty()
        {
            self.discord.token = Some(token);
        }
        if let Ok(guild) = std::env::var("DISCORD_GUILD_ID")
            && let Ok(parsed) = guild.trim().parse::<u64>()
        {
            self.discord.guild_id = Some(parsed);
        }
        if let Ok(key) = std::env::var("NVIDIA_API_KEY")
            && !key.trim().is_empty()
        {
            self.ai.nvidia_api_key = Some(key.trim().to_string());
        }
        if let Ok(model) = std::env::var("AI_MODEL")
            && !model.trim().is_empty()
        {
            self.ai.model = model.trim().to_string();
        }
        if let Ok(delay_str) = std::env::var("AI_DELAY_MS")
            && let Ok(delay) = delay_str.trim().parse::<u64>()
        {
            self.ai.delay_ms = delay;
        }
        if let Ok(ghidra) = std::env::var("GHIDRA_PATH")
            && !ghidra.trim().is_empty()
        {
            self.ai.ghidra_path = Some(ghidra.trim().to_string());
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        // Report an absolute path: under systemd the relative default resolves
        // against WorkingDirectory, not wherever the operator was standing.
        let shown = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let raw = std::fs::read_to_string(path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => anyhow!(
                "no config at {} — a config file is optional, so either remove \
                 EAC_CONFIG or create the file",
                shown.display()
            ),
            _ => anyhow::Error::new(e).context(format!("reading config at {}", shown.display())),
        })?;
        let mut cfg: Config =
            toml::from_str(&raw).with_context(|| format!("parsing {}", shown.display()))?;

        cfg.apply_environment();
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
            bail!(
                "no Discord token: set the DISCORD_TOKEN environment variable \
                 (or discord.token in a config file)"
            );
        }
        if self.discord.channel_id == Some(0) {
            bail!("discord.channel_id is not a valid channel id");
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
        assert_eq!(cfg.tracker.archive_path, "archive");
    }

    #[test]
    fn the_shipped_example_config_is_valid() {
        let raw = include_str!("../config.example.toml");
        let cfg: Config = toml::from_str(raw).expect("example config must parse");
        // The example ships with no games and no channel: both are configured
        // from Discord. It must still be a valid file.
        assert!(cfg.games.is_empty());
        assert!(cfg.discord.channel_id.is_none());
        assert_eq!(cfg.tracker.settings_path, "settings.json");
    }

    #[test]
    fn a_missing_config_names_the_absolute_path_and_the_fix() {
        // Relative paths resolve against systemd's WorkingDirectory, so the
        // error has to say where it actually looked.
        let err = Config::load(Path::new("definitely-not-here.toml"))
            .expect_err("a missing config is an error")
            .to_string();

        assert!(err.contains("no config at"), "got: {err}");
        assert!(err.contains('/'), "path must be absolute, got: {err}");
        assert!(err.contains("definitely-not-here.toml"), "got: {err}");
        assert!(
            err.contains("optional"),
            "must say a config file is not required: {err}"
        );
    }

    #[test]
    fn a_token_is_the_only_requirement() {
        // Channels and games are configured from Discord, so a config with
        // neither is valid — that is the normal deployment now.
        let cfg = parse("[discord]\ntoken = \"t\"\n").unwrap();
        assert!(cfg.games.is_empty());
        assert!(cfg.discord.channel_id.is_none());
    }

    #[test]
    fn a_config_file_is_optional_entirely() {
        // Reaching the token check proves the absent file was not itself
        // treated as an error.
        match Config::load_or_default(Path::new("no-such-config.toml")) {
            Err(e) => assert!(
                e.to_string().contains("token"),
                "absent file must not be the error: {e}"
            ),
            // A token in the environment is equally valid.
            Ok(cfg) => assert!(cfg.discord.token.is_some()),
        }
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

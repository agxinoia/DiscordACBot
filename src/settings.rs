//! Configuration managed from Discord at runtime.
//!
//! The operator supplies a bot token and nothing else; channels and tracked
//! games are configured per guild through `/eac` and persisted here. Settings
//! are per-guild because one bot instance can serve several servers, and one
//! server's channel must never become another's.
//!
//! Values in `config.toml` act as fallbacks for a guild that has not set its
//! own, so an existing file-based deployment keeps working unchanged.

use crate::config::{Config, Game};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// What one guild has configured. Every override is optional so an unset
/// value falls through to the operator's defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GuildSettings {
    #[serde(default)]
    pub channel_id: Option<u64>,
    #[serde(default)]
    pub games: Vec<Game>,
    #[serde(default)]
    pub announce_on_first_seen: Option<bool>,
    #[serde(default)]
    pub attach_raw_response: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub guilds: BTreeMap<u64, GuildSettings>,
    /// Applies to the whole bot, not one guild.
    #[serde(default)]
    pub poll_interval_secs: Option<u64>,
}

/// One guild that wants to hear about a particular target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscriber {
    pub guild_id: u64,
    pub channel_id: u64,
    pub announce_on_first_seen: bool,
    pub attach_raw_response: bool,
}

/// A target to poll, and everyone waiting on it. Guilds tracking the same
/// product/deployment/platform share a single fetch.
#[derive(Debug, Clone)]
pub struct Target {
    pub game: Game,
    pub platform: String,
    pub subscribers: Vec<Subscriber>,
}

pub struct SettingsStore {
    path: PathBuf,
    inner: Mutex<Settings>,
}

impl SettingsStore {
    /// Load persisted settings, treating a missing file as empty. A corrupt
    /// file is an error rather than a silent reset — starting over would
    /// quietly discard a server's configuration.
    pub fn load(path: &Path) -> Result<Self> {
        let settings = match std::fs::read_to_string(path) {
            Ok(raw) if raw.trim().is_empty() => Settings::default(),
            Ok(raw) => serde_json::from_str(&raw)
                .with_context(|| format!("parsing settings at {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Settings::default(),
            Err(e) => {
                return Err(e).with_context(|| format!("reading settings at {}", path.display()));
            }
        };
        Ok(Self {
            path: path.to_path_buf(),
            inner: Mutex::new(settings),
        })
    }

    pub fn snapshot(&self) -> Settings {
        self.inner.lock().expect("settings lock poisoned").clone()
    }

    pub fn guild(&self, guild_id: u64) -> GuildSettings {
        self.inner
            .lock()
            .expect("settings lock poisoned")
            .guilds
            .get(&guild_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Mutate one guild's settings and persist the result. The closure's value
    /// is returned only if the write succeeded, so a caller never reports
    /// success for a change that was not saved.
    pub fn edit_guild<R>(
        &self,
        guild_id: u64,
        edit: impl FnOnce(&mut GuildSettings) -> R,
    ) -> Result<R> {
        let (value, snapshot) = {
            let mut settings = self.inner.lock().expect("settings lock poisoned");
            let entry = settings.guilds.entry(guild_id).or_default();
            let value = edit(entry);
            (value, settings.clone())
        };
        self.write(&snapshot)?;
        Ok(value)
    }

    pub fn set_poll_interval(&self, secs: u64) -> Result<()> {
        let snapshot = {
            let mut settings = self.inner.lock().expect("settings lock poisoned");
            settings.poll_interval_secs = Some(secs);
            settings.clone()
        };
        self.write(&snapshot)
    }

    fn write(&self, settings: &Settings) -> Result<()> {
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(settings).context("serialising settings")?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))?;
        Ok(())
    }
}

/// Effective poll interval: the Discord-set value, else the operator default.
pub fn poll_interval(settings: &Settings, config: &Config) -> u64 {
    settings
        .poll_interval_secs
        .unwrap_or(config.tracker.poll_interval_secs)
        .max(MIN_POLL_INTERVAL_SECS)
}

pub const MIN_POLL_INTERVAL_SECS: u64 = 30;

/// A guild's games, falling back to the operator's configured list.
pub fn guild_games<'a>(guild: &'a GuildSettings, config: &'a Config) -> &'a [Game] {
    if guild.games.is_empty() {
        &config.games
    } else {
        &guild.games
    }
}

/// Collapse every guild's configuration into the set of targets to poll, each
/// carrying the guilds that want to hear about it.
pub fn targets(settings: &Settings, config: &Config) -> Vec<Target> {
    let mut by_key: BTreeMap<(String, String, String), Target> = BTreeMap::new();

    for (guild_id, guild) in &settings.guilds {
        // A guild with nowhere to post is not tracking anything yet.
        let Some(channel_id) = guild.channel_id.or(config.discord.channel_id) else {
            continue;
        };
        let subscriber = Subscriber {
            guild_id: *guild_id,
            channel_id,
            announce_on_first_seen: guild
                .announce_on_first_seen
                .unwrap_or(config.tracker.announce_on_first_seen),
            attach_raw_response: guild
                .attach_raw_response
                .unwrap_or(config.tracker.attach_raw_response),
        };

        for game in guild_games(guild, config) {
            for platform in &game.platforms {
                let key = (
                    game.product_id.clone(),
                    game.deployment_id.clone(),
                    platform.clone(),
                );
                by_key
                    .entry(key)
                    .or_insert_with(|| Target {
                        game: game.clone(),
                        platform: platform.clone(),
                        subscribers: Vec::new(),
                    })
                    .subscribers
                    .push(subscriber.clone());
            }
        }
    }
    by_key.into_values().collect()
}

/// Reject anything that would be unsafe or meaningless in a CDN URL.
///
/// These values come from whoever runs the slash command, and they are
/// interpolated straight into a request path, so `..` and `/` must never
/// survive validation.
pub fn validate_id(label: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 64 {
        bail!("{label} must be between 1 and 64 characters");
    }
    // Usually a 32-character hex string, but not always: Fortnite's product
    // id is `prod-fn`. Hyphen and underscore are allowed; `.` and the path
    // separators are not, so `..` cannot appear however it is spelled.
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("{label} may only contain letters, digits, hyphen and underscore — got `{value}`");
    }
    Ok(())
}

pub fn validate_platform(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 32 {
        bail!("platform must be between 1 and 32 characters");
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!("platform may only contain letters, digits, underscore and hyphen — got `{value}`");
    }
    Ok(())
}

pub fn validate_name(value: &str) -> Result<()> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > 64 {
        bail!("game name must be between 1 and 64 characters");
    }
    Ok(())
}

/// Parse a comma or space separated platform list.
pub fn parse_platforms(raw: &str) -> Result<Vec<String>> {
    let platforms: Vec<String> = raw
        .split([',', ' '])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();

    if platforms.is_empty() {
        bail!("list at least one platform, e.g. `win64` or `win64, win32`");
    }
    if platforms.len() > 16 {
        bail!("that is more platforms than any deployment has");
    }
    for platform in &platforms {
        validate_platform(platform)?;
    }
    let mut deduped = platforms.clone();
    deduped.sort();
    deduped.dedup();
    Ok(if deduped.len() == platforms.len() {
        platforms
    } else {
        deduped
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Discord, Tracker as TrackerCfg};

    fn game(name: &str, product: &str) -> Game {
        Game {
            name: name.into(),
            product_id: product.into(),
            deployment_id: "d".into(),
            platforms: vec!["win64".into()],
        }
    }

    fn config() -> Config {
        Config {
            discord: Discord {
                token: Some("t".into()),
                channel_id: None,
                guild_id: None,
            },
            tracker: TrackerCfg::default(),
            games: Vec::new(),
        }
    }

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("eac-settings-{}-{}.json", name, std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn missing_file_loads_as_empty() {
        let store = SettingsStore::load(&temp_path("missing")).unwrap();
        assert!(store.snapshot().guilds.is_empty());
    }

    #[test]
    fn guild_edits_persist_across_restarts() {
        let path = temp_path("persist");
        let store = SettingsStore::load(&path).unwrap();

        store
            .edit_guild(42, |g| {
                g.channel_id = Some(999);
                g.games.push(game("ARC Raiders", "p"));
            })
            .unwrap();

        let reloaded = SettingsStore::load(&path).unwrap();
        let guild = reloaded.guild(42);
        assert_eq!(guild.channel_id, Some(999));
        assert_eq!(guild.games.len(), 1);
        assert_eq!(guild.games[0].name, "ARC Raiders");

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_corrupt_settings_file_is_an_error_not_a_reset() {
        let path = temp_path("corrupt");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(SettingsStore::load(&path).is_err());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn guilds_without_a_channel_track_nothing() {
        let mut settings = Settings::default();
        settings.guilds.insert(
            1,
            GuildSettings {
                channel_id: None,
                games: vec![game("ARC Raiders", "p")],
                ..Default::default()
            },
        );
        assert!(
            targets(&settings, &config()).is_empty(),
            "nowhere to post means nothing to poll"
        );
    }

    #[test]
    fn guilds_tracking_the_same_target_share_one_fetch() {
        let mut settings = Settings::default();
        for guild_id in [1u64, 2] {
            settings.guilds.insert(
                guild_id,
                GuildSettings {
                    channel_id: Some(guild_id * 100),
                    games: vec![game("ARC Raiders", "p")],
                    ..Default::default()
                },
            );
        }

        let targets = targets(&settings, &config());
        assert_eq!(targets.len(), 1, "one fetch, not one per guild");
        assert_eq!(targets[0].subscribers.len(), 2);
        assert_eq!(targets[0].subscribers[0].channel_id, 100);
        assert_eq!(targets[0].subscribers[1].channel_id, 200);
    }

    #[test]
    fn different_products_are_separate_targets() {
        let mut settings = Settings::default();
        settings.guilds.insert(
            1,
            GuildSettings {
                channel_id: Some(10),
                games: vec![game("A", "p1"), game("B", "p2")],
                ..Default::default()
            },
        );
        assert_eq!(targets(&settings, &config()).len(), 2);
    }

    #[test]
    fn config_supplies_fallbacks_for_an_unconfigured_guild() {
        let mut config = config();
        config.discord.channel_id = Some(7);
        config.games = vec![game("Legacy", "p")];

        let mut settings = Settings::default();
        settings.guilds.insert(1, GuildSettings::default());

        let targets = targets(&settings, &config);
        assert_eq!(targets.len(), 1, "file-based deployments keep working");
        assert_eq!(targets[0].subscribers[0].channel_id, 7);
        assert_eq!(targets[0].game.name, "Legacy");
    }

    #[test]
    fn a_guilds_own_settings_win_over_the_fallbacks() {
        let mut config = config();
        config.discord.channel_id = Some(7);
        config.games = vec![game("Legacy", "p1")];

        let mut settings = Settings::default();
        settings.guilds.insert(
            1,
            GuildSettings {
                channel_id: Some(8),
                games: vec![game("Chosen", "p2")],
                ..Default::default()
            },
        );

        let targets = targets(&settings, &config);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].game.name, "Chosen");
        assert_eq!(targets[0].subscribers[0].channel_id, 8);
    }

    #[test]
    fn poll_interval_prefers_the_discord_setting_but_enforces_a_floor() {
        let config = config();
        assert_eq!(poll_interval(&Settings::default(), &config), 300);

        let settings = Settings {
            poll_interval_secs: Some(600),
            ..Default::default()
        };
        assert_eq!(poll_interval(&settings, &config), 600);

        let impolite = Settings {
            poll_interval_secs: Some(1),
            ..Default::default()
        };
        assert_eq!(
            poll_interval(&impolite, &config),
            MIN_POLL_INTERVAL_SECS,
            "a slash command must not be able to hammer the CDN"
        );
    }

    #[test]
    fn ids_that_would_escape_the_url_path_are_rejected() {
        assert!(validate_id("product id", "9e8b37541e614575b4de303d2c2e44cf").is_ok());
        // Not every id is hex: Fortnite's product id is `prod-fn`.
        assert!(validate_id("product id", "prod-fn").is_ok());
        assert!(validate_id("product id", "some_id").is_ok());

        // These are the ones that matter: the value goes straight into a URL.
        assert!(validate_id("product id", "../../etc/passwd").is_err());
        assert!(validate_id("product id", "a/b").is_err());
        assert!(validate_id("product id", "..").is_err());
        assert!(validate_id("product id", "a.b").is_err());
        assert!(validate_id("product id", "").is_err());
        assert!(validate_id("product id", &"a".repeat(65)).is_err());
    }

    #[test]
    fn platform_validation_allows_real_names_only() {
        assert!(validate_platform("win64").is_ok());
        assert!(validate_platform("winarm_x64-x64").is_ok());
        assert!(validate_platform("../win64").is_err());
        assert!(validate_platform("").is_err());
    }

    #[test]
    fn platform_lists_parse_and_normalise() {
        assert_eq!(parse_platforms("win64").unwrap(), vec!["win64"]);
        assert_eq!(
            parse_platforms(" WIN64 , win32 ").unwrap(),
            vec!["win64", "win32"]
        );
        assert_eq!(
            parse_platforms("win64 win32").unwrap(),
            vec!["win64", "win32"]
        );
        assert_eq!(parse_platforms("win64, win64").unwrap(), vec!["win64"]);

        assert!(parse_platforms("").is_err());
        assert!(parse_platforms("  ,  ").is_err());
        assert!(parse_platforms("win64, ../etc").is_err());
    }

    #[test]
    fn game_names_are_bounded() {
        assert!(validate_name("ARC Raiders").is_ok());
        assert!(validate_name("   ").is_err());
        assert!(validate_name(&"x".repeat(65)).is_err());
    }
}

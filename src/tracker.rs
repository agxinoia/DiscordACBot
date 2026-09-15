//! Polling engine: fetch every configured target, compare against the last
//! seen digest, and post an embed when it moves.

use crate::config::{Config, Game};
use crate::eac::{self, Snapshot};
use crate::embed;
use crate::state::{Store, target_key};
use anyhow::{Context, Result};
use serenity::builder::{CreateAttachment, CreateMessage};
use serenity::http::Http;
use serenity::model::id::ChannelId;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{error, info, warn};

/// Discord accepts at most 10 files on a single message.
const MAX_ATTACHMENTS: usize = 10;

/// Result of checking one target.
pub struct Outcome {
    pub snapshot: Snapshot,
    pub previous: Option<String>,
    /// The digest moved from a previously recorded value.
    pub changed: bool,
    /// This target had no recorded digest at all.
    pub first_seen: bool,
}

impl Outcome {
    fn should_announce(&self, announce_on_first_seen: bool) -> bool {
        self.changed || (self.first_seen && announce_on_first_seen)
    }
}

pub struct Tracker {
    config: Arc<Config>,
    client: eac::Client,
    store: Mutex<Store>,
}

impl Tracker {
    pub fn new(config: Arc<Config>) -> Result<Self> {
        let client = eac::Client::with_base(
            config.tracker.cdn_base.as_deref().unwrap_or(eac::CDN_BASE),
            &config.tracker.user_agent,
            Duration::from_secs(config.tracker.request_timeout_secs),
        )?;
        let store = Store::load(std::path::Path::new(&config.tracker.state_path))?;
        Ok(Self {
            config,
            client,
            store: Mutex::new(store),
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Render `/eac status` as plain text.
    pub fn status_lines(&self) -> Vec<String> {
        let store = self.store.lock().expect("state lock poisoned");
        store
            .entries()
            .map(|(key, seen)| {
                format!(
                    "`{}` — `{}` ({}, <t:{}:R>)",
                    key,
                    eac::short_hash(&seen.digest),
                    embed::human_bytes(seen.size),
                    seen.seen_at
                )
            })
            .collect()
    }

    /// Fetch one target and fold the result into persisted state.
    pub async fn check(&self, game: &Game, platform: &str) -> Result<Outcome> {
        let snapshot = self
            .client
            .fetch(&game.product_id, &game.deployment_id, platform)
            .await?;
        let key = target_key(&game.product_id, &game.deployment_id, platform);

        // Kept free of `.await` so the std mutex is never held across a yield.
        let (previous, changed, first_seen) = {
            let mut store = self.store.lock().expect("state lock poisoned");
            let previous = store.get(&key).map(|s| s.digest.clone());
            let first_seen = previous.is_none();
            let changed = previous.as_deref().is_some_and(|p| p != snapshot.digest);

            if first_seen || changed {
                store.record(&key, &snapshot.digest, snapshot.size());
                if let Err(e) = store.save() {
                    // A failed save costs a duplicate announcement next cycle,
                    // which is better than dropping the update entirely.
                    error!(target = %key, error = ?e, "failed to persist state");
                }
            }
            (previous, changed, first_seen)
        };

        Ok(Outcome {
            snapshot,
            previous,
            changed,
            first_seen,
        })
    }

    /// Check every configured target once, announcing the ones that moved.
    pub async fn run_once(&self, http: &Http) {
        for game in &self.config.games {
            for platform in &game.platforms {
                match self.check(game, platform).await {
                    Ok(outcome) => {
                        if outcome.should_announce(self.config.tracker.announce_on_first_seen) {
                            info!(
                                game = %game.name,
                                platform = %platform,
                                digest = %outcome.snapshot.short_digest(),
                                first_seen = outcome.first_seen,
                                "announcing EAC module change"
                            );
                            if let Err(e) = self.announce(http, game, platform, &outcome).await {
                                error!(game = %game.name, platform = %platform, error = ?e,
                                       "failed to post update");
                            }
                        }
                    }
                    Err(e) => {
                        // One unreachable target must not stall the others.
                        warn!(game = %game.name, platform = %platform, error = ?e,
                              "check failed");
                    }
                }
            }
        }
    }

    /// Poll forever on the configured interval.
    pub async fn poll_loop(self: Arc<Self>, http: Arc<Http>) {
        let interval = Duration::from_secs(self.config.tracker.poll_interval_secs);
        info!(
            interval_secs = self.config.tracker.poll_interval_secs,
            targets = self.target_count(),
            "starting poll loop"
        );
        let mut ticker = tokio::time::interval(interval);
        // A missed tick (slow cycle) should not cause a burst of catch-up runs.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            self.run_once(&http).await;
        }
    }

    pub fn target_count(&self) -> usize {
        self.config.games.iter().map(|g| g.platforms.len()).sum()
    }

    async fn announce(
        &self,
        http: &Http,
        game: &Game,
        platform: &str,
        outcome: &Outcome,
    ) -> Result<()> {
        let tracker = &self.config.tracker;
        let embed = embed::build_update_embed(
            game,
            platform,
            &outcome.snapshot,
            outcome.previous.as_deref(),
            tracker.max_attachment_bytes,
            tracker.attach_raw_response,
            tracker.thumbnail_url.as_deref(),
        );

        let mut message = CreateMessage::new().embed(embed);

        if tracker.attach_raw_response {
            let parts = embed::split_parts(&outcome.snapshot.body, tracker.max_attachment_bytes);
            if parts.len() > MAX_ATTACHMENTS {
                warn!(
                    parts = parts.len(),
                    "response exceeds {MAX_ATTACHMENTS} attachments; truncating upload"
                );
            }
            let total = parts.len();
            for (i, part) in parts.into_iter().take(MAX_ATTACHMENTS).enumerate() {
                let filename = attachment_name(&game.name, platform, i + 1, total);
                message = message.add_file(CreateAttachment::bytes(part.to_vec(), filename));
            }
        }

        ChannelId::new(self.config.discord.channel_id)
            .send_message(http, message)
            .await
            .context("sending update message")?;
        Ok(())
    }
}

/// `Rust` + `win64` + part 2 of 3 -> `rust_win64_part2of3.bin`.
fn attachment_name(game: &str, platform: &str, index: usize, total: usize) -> String {
    let base = format!("{}_{}", slug(game), slug(platform));
    if total <= 1 {
        format!("{base}.bin")
    } else {
        format!("{base}_part{index}of{total}.bin")
    }
}

/// Reduce arbitrary text to a filename-safe token.
fn slug(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() {
        "target".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_filename_safe() {
        assert_eq!(slug("Rust"), "rust");
        assert_eq!(slug("ARC Raiders"), "arc_raiders");
        assert_eq!(slug("  ../weird**name  "), "weird_name");
        assert_eq!(slug("!!!"), "target");
    }

    #[test]
    fn names_single_and_multipart_attachments() {
        assert_eq!(attachment_name("Rust", "win64", 1, 1), "rust_win64.bin");
        assert_eq!(
            attachment_name("ARC Raiders", "win64", 2, 3),
            "arc_raiders_win64_part2of3.bin"
        );
    }

    fn outcome(changed: bool, first_seen: bool) -> Outcome {
        Outcome {
            snapshot: Snapshot {
                url: String::new(),
                body: Vec::new(),
                digest: String::new(),
                etag: None,
                last_modified: None,
                modules: Vec::new(),
            },
            previous: None,
            changed,
            first_seen,
        }
    }

    #[test]
    fn first_sighting_is_silent_unless_opted_in() {
        assert!(!outcome(false, true).should_announce(false));
        assert!(outcome(false, true).should_announce(true));
    }

    #[test]
    fn a_real_change_always_announces() {
        assert!(outcome(true, false).should_announce(false));
    }

    #[test]
    fn an_unchanged_target_is_never_announced() {
        assert!(!outcome(false, false).should_announce(true));
    }
}

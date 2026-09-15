//! Polling engine: fetch every configured target, compare against the last
//! seen digest, and post an embed when it moves.

use crate::archive::{Archive, Record};
use crate::config::{Config, Game};
use crate::diff::{self, Diff};
use crate::eac::{self, Snapshot};
use crate::embed;
use crate::settings::{self, SettingsStore, Subscriber};
use crate::state::{Seen, Store, now_unix, target_key};
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
    /// What moved. Present only on a real change, and only as far as the
    /// available history allows.
    pub diff: Option<Diff>,
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
    settings: Arc<SettingsStore>,
    /// `None` when archiving is switched off, which also costs TLSH distance.
    archive: Option<Archive>,
}

impl Tracker {
    pub fn new(config: Arc<Config>, settings: Arc<SettingsStore>) -> Result<Self> {
        let client = eac::Client::with_base(
            config.tracker.cdn_base.as_deref().unwrap_or(eac::CDN_BASE),
            &config.tracker.user_agent,
            Duration::from_secs(config.tracker.request_timeout_secs),
        )?;
        let store = Store::load(std::path::Path::new(&config.tracker.state_path))?;
        let archive = Some(config.tracker.archive_path.trim())
            .filter(|p| !p.is_empty())
            .map(Archive::new);
        Ok(Self {
            config,
            client,
            store: Mutex::new(store),
            settings,
            archive,
        })
    }

    pub fn settings(&self) -> &Arc<SettingsStore> {
        &self.settings
    }

    /// Everything currently worth polling, folded across all guilds.
    pub fn targets(&self) -> Vec<settings::Target> {
        settings::targets(&self.settings.snapshot(), &self.config)
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

        // Each lock scope is kept free of `.await` so the std mutex is never
        // held across a yield.
        let previous: Option<Seen> = {
            let store = self.store.lock().expect("state lock poisoned");
            store.get(&key).cloned()
        };
        let first_seen = previous.is_none();
        let changed = previous
            .as_ref()
            .is_some_and(|p| p.digest != snapshot.digest());

        // Describing the change needs the bytes it replaced, which only the
        // archive has; without it the diff is still produced, just thinner.
        let diff = previous.as_ref().filter(|_| changed).map(|prev| {
            let previous_body = self
                .archive
                .as_ref()
                .and_then(|a| a.load(&prev.digest).ok().flatten());
            diff::compute(prev, &snapshot, previous_body.as_deref())
        });

        if first_seen || changed {
            {
                let mut store = self.store.lock().expect("state lock poisoned");
                store.record(&key, &snapshot);
                if let Err(e) = store.save() {
                    // A failed save costs a duplicate announcement next cycle,
                    // which is better than dropping the update entirely.
                    error!(target = %key, error = ?e, "failed to persist state");
                }
            }
            self.archive_snapshot(game, platform, &snapshot, previous.as_ref());
        }

        Ok(Outcome {
            snapshot,
            previous: previous.map(|p| p.digest),
            changed,
            first_seen,
            diff,
        })
    }

    /// Retain the payload and append an index record. Archiving is never
    /// allowed to fail a poll — a lost archive entry is worth less than a
    /// missed notification.
    fn archive_snapshot(
        &self,
        game: &Game,
        platform: &str,
        snapshot: &Snapshot,
        previous: Option<&Seen>,
    ) {
        let Some(archive) = &self.archive else {
            return;
        };
        if let Err(e) = archive.store(snapshot.digest(), &snapshot.body) {
            warn!(game = %game.name, error = ?e, "failed to archive payload");
            return;
        }
        let record = Record {
            seen_at: now_unix(),
            game: game.name.clone(),
            product_id: game.product_id.clone(),
            deployment_id: game.deployment_id.clone(),
            platform: platform.to_string(),
            url: snapshot.url.clone(),
            size: snapshot.size(),
            hashes: snapshot.hashes.clone(),
            tlsh: snapshot.tlsh.clone(),
            format: snapshot.format.clone(),
            entropy: snapshot.entropy,
            headers: snapshot.headers.clone().into_iter().collect(),
            modules: snapshot.modules.clone(),
            pe: snapshot.pe.clone(),
            previous_sha256: previous.map(|p| p.digest.clone()),
        };
        if let Err(e) = archive.append(&record) {
            warn!(game = %game.name, error = ?e, "failed to append archive index");
        }
    }

    /// Check every tracked target once, announcing the ones that moved.
    ///
    /// Guilds tracking the same product, deployment and platform share a
    /// single fetch; only the announcement fans out.
    pub async fn run_once(&self, http: &Http) {
        for target in self.targets() {
            match self.check(&target.game, &target.platform).await {
                Ok(outcome) => {
                    for subscriber in &target.subscribers {
                        if !outcome.should_announce(subscriber.announce_on_first_seen) {
                            continue;
                        }
                        info!(
                            game = %target.game.name,
                            platform = %target.platform,
                            guild = subscriber.guild_id,
                            digest = %outcome.snapshot.short_digest(),
                            first_seen = outcome.first_seen,
                            "announcing EAC module change"
                        );
                        if let Err(e) = self
                            .announce(http, subscriber, &target.game, &target.platform, &outcome)
                            .await
                        {
                            // One guild's misconfigured channel must not stop
                            // the others from being told.
                            error!(
                                game = %target.game.name,
                                guild = subscriber.guild_id,
                                error = ?e,
                                "failed to post update"
                            );
                        }
                    }
                }
                Err(e) => {
                    // One unreachable target must not stall the others.
                    warn!(game = %target.game.name, platform = %target.platform, error = ?e,
                          "check failed");
                }
            }
        }
    }

    /// Poll forever, re-reading the interval each cycle so `/eac set` takes
    /// effect without a restart.
    pub async fn poll_loop(self: Arc<Self>, http: Arc<Http>) {
        info!(
            interval_secs = self.poll_interval(),
            targets = self.target_count(),
            "starting poll loop"
        );
        loop {
            tokio::time::sleep(Duration::from_secs(self.poll_interval())).await;
            self.run_once(&http).await;
        }
    }

    pub fn poll_interval(&self) -> u64 {
        settings::poll_interval(&self.settings.snapshot(), &self.config)
    }

    pub fn target_count(&self) -> usize {
        self.targets().len()
    }

    async fn announce(
        &self,
        http: &Http,
        subscriber: &Subscriber,
        game: &Game,
        platform: &str,
        outcome: &Outcome,
    ) -> Result<()> {
        let tracker = &self.config.tracker;
        let attach_raw = subscriber.attach_raw_response;
        let embed = embed::build_update_embed(
            game,
            platform,
            &outcome.snapshot,
            outcome.previous.as_deref(),
            outcome.diff.as_ref(),
            tracker.max_attachment_bytes,
            attach_raw,
            tracker.thumbnail_url.as_deref(),
        );

        let mut message = CreateMessage::new().embed(embed);

        if attach_raw {
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

        ChannelId::new(subscriber.channel_id)
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
            snapshot: Snapshot::for_test(b""),
            previous: None,
            changed,
            first_seen,
            diff: None,
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

//! The core loop: first sighting, a real change, and a no-op re-check, with
//! state surviving a restart.

use eac_tracker::config::{Config, Discord, Game, Tracker as TrackerCfg};
use eac_tracker::tracker::Tracker;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A long-lived server whose response body can be swapped between requests,
/// standing in for the CDN publishing a new module set.
struct FakeCdn {
    base: String,
    body: Arc<Mutex<Vec<u8>>>,
}

impl FakeCdn {
    async fn start(initial: &[u8]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let body = Arc::new(Mutex::new(initial.to_vec()));

        let served = Arc::clone(&body);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let served = Arc::clone(&served);
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let mut request = Vec::new();
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => request.extend_from_slice(&buf[..n]),
                        }
                    }
                    let payload = served.lock().unwrap().clone();
                    let mut response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                        payload.len()
                    )
                    .into_bytes();
                    response.extend_from_slice(&payload);
                    let _ = sock.write_all(&response).await;
                    let _ = sock.flush().await;
                });
            }
        });

        Self { base, body }
    }

    fn publish(&self, new_body: &[u8]) {
        *self.body.lock().unwrap() = new_body.to_vec();
    }
}

fn game() -> Game {
    Game {
        name: "ARC Raiders".into(),
        product_id: "9e8b37541e614575b4de303d2c2e44cf".into(),
        deployment_id: "35e06571d8ab4de4b98519b624125459".into(),
        platforms: vec!["win64".into()],
    }
}

fn config(base: &str, state_path: &str) -> Arc<Config> {
    Arc::new(Config {
        discord: Discord {
            token: Some("test".into()),
            channel_id: 1,
            guild_id: None,
        },
        tracker: TrackerCfg {
            cdn_base: Some(base.to_string()),
            state_path: state_path.to_string(),
            ..Default::default()
        },
        games: vec![game()],
    })
}

fn temp_state(name: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "eac-tracker-it-{}-{}.json",
        name,
        std::process::id()
    ));
    let _ = std::fs::remove_file(&p);
    p.to_string_lossy().into_owned()
}

#[tokio::test]
async fn detects_a_change_exactly_once_and_remembers_it_across_restarts() {
    const V1: &[u8] = br#"{"modules":[{"name":"driver.sys","size":100,"hash":"aaaa"}]}"#;
    const V2: &[u8] = br#"{"modules":[{"name":"driver.sys","size":200,"hash":"bbbb"}]}"#;

    let cdn = FakeCdn::start(V1).await;
    let state_path = temp_state("cycle");
    let cfg = config(&cdn.base, &state_path);

    let tracker = Tracker::new(Arc::clone(&cfg)).unwrap();
    let game = game();

    // First sighting: recorded, but not a change.
    let first = tracker.check(&game, "win64").await.unwrap();
    assert!(first.first_seen, "first check must report first_seen");
    assert!(!first.changed, "first sighting is not a change");
    assert!(first.previous.is_none());

    // Nothing published yet, so a re-check is a no-op.
    let again = tracker.check(&game, "win64").await.unwrap();
    assert!(!again.first_seen);
    assert!(!again.changed, "unchanged body must not report a change");
    assert_eq!(again.snapshot.digest, first.snapshot.digest);

    // The CDN publishes new modules.
    cdn.publish(V2);
    let updated = tracker.check(&game, "win64").await.unwrap();
    assert!(updated.changed, "new body must report a change");
    assert_eq!(
        updated.previous.as_deref(),
        Some(first.snapshot.digest.as_str()),
        "the update must carry the digest it replaced"
    );
    assert_eq!(updated.snapshot.modules[0].size, Some(200));

    // The same change must not fire twice.
    let settled = tracker.check(&game, "win64").await.unwrap();
    assert!(!settled.changed, "a change must be announced only once");

    // A fresh process (new Tracker over the same state file) stays quiet.
    drop(tracker);
    let restarted = Tracker::new(cfg).unwrap();
    let after_restart = restarted.check(&game, "win64").await.unwrap();
    assert!(
        !after_restart.first_seen,
        "restart must load persisted state, not re-announce"
    );
    assert!(!after_restart.changed);

    std::fs::remove_file(&state_path).unwrap();
}

#[tokio::test]
async fn counts_every_configured_platform_as_a_target() {
    let cdn = FakeCdn::start(b"x").await;
    let state_path = temp_state("targets");
    let mut cfg = config(&cdn.base, &state_path);
    Arc::get_mut(&mut cfg).unwrap().games[0].platforms = vec!["win64".into(), "win32".into()];

    let tracker = Tracker::new(cfg).unwrap();
    assert_eq!(tracker.target_count(), 2);

    let _ = std::fs::remove_file(&state_path);
}

#[tokio::test]
async fn status_lines_report_seen_targets() {
    let cdn = FakeCdn::start(b"payload").await;
    let state_path = temp_state("status");
    let tracker = Tracker::new(config(&cdn.base, &state_path)).unwrap();

    assert!(tracker.status_lines().is_empty(), "nothing seen yet");
    tracker.check(&game(), "win64").await.unwrap();

    let lines = tracker.status_lines();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("win64"), "got: {}", lines[0]);

    let _ = std::fs::remove_file(&state_path);
}

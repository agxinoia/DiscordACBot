//! End-to-end coverage of the fetch path against a local HTTP server.
//!
//! The real CDN is not reachable from CI, and depending on it would make these
//! tests flaky anyway, so a throwaway listener stands in for it. What is being
//! verified is our side: URL construction, header capture, body digesting,
//! module parsing and failure handling.

use eac_tracker::eac::Client;
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serve exactly one request and return the base URL to aim a client at.
async fn serve_once(
    status_line: &'static str,
    extra_headers: &'static str,
    body: &'static [u8],
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();

        // Read just the request head; there is no request body to drain.
        let mut request = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            let n = sock.read(&mut buf).await.unwrap();
            if n == 0 || request.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
            request.extend_from_slice(&buf[..n]);
            if request.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }

        // An explicit Content-Length in extra_headers wins, so a HEAD
        // response can advertise a size it does not send.
        let mut response = if extra_headers.contains("Content-Length") {
            format!("{status_line}\r\n{extra_headers}\r\n").into_bytes()
        } else {
            format!(
                "{status_line}\r\nContent-Length: {}\r\n{extra_headers}\r\n",
                body.len()
            )
            .into_bytes()
        };
        response.extend_from_slice(body);
        sock.write_all(&response).await.unwrap();
        sock.flush().await.unwrap();
    });

    format!("http://{addr}")
}

fn client(base: &str) -> Client {
    Client::with_base(base, "eac-tracker-test", Duration::from_secs(5)).unwrap()
}

#[tokio::test]
async fn fetches_digests_and_parses_a_json_manifest() {
    const BODY: &[u8] = br#"{"modules":[{"name":"driver.sys","arch":"arm64","size":17301504,"hash":"bc1f4446a7008207"}]}"#;
    let base = serve_once(
        "HTTP/1.1 200 OK",
        "ETag: \"abc123\"\r\nLast-Modified: Mon, 01 Sep 2025 00:00:00 GMT\r\n",
        BODY,
    )
    .await;

    let snapshot = client(&base)
        .fetch("prod", "deploy", "win64")
        .await
        .unwrap();

    assert_eq!(snapshot.url, format!("{base}/prod/deploy/win64"));
    assert_eq!(snapshot.body, BODY);
    assert_eq!(snapshot.size(), BODY.len() as u64);
    assert_eq!(snapshot.etag.as_deref(), Some("\"abc123\""));
    assert_eq!(
        snapshot.last_modified.as_deref(),
        Some("Mon, 01 Sep 2025 00:00:00 GMT")
    );

    // The digest must match an independent SHA-256 of the same bytes.
    assert_eq!(snapshot.digest(), hex::encode(Sha256::digest(BODY)));
    assert_eq!(snapshot.short_digest().len(), 16);

    assert_eq!(snapshot.modules.len(), 1);
    assert_eq!(snapshot.modules[0].name, "driver.sys");
    assert_eq!(snapshot.modules[0].size, Some(17_301_504));
}

#[tokio::test]
async fn identical_bodies_digest_identically_and_changed_ones_do_not() {
    const A: &[u8] = b"module-blob-v1";
    const B: &[u8] = b"module-blob-v2";

    let first = client(&serve_once("HTTP/1.1 200 OK", "", A).await)
        .fetch("p", "d", "win64")
        .await
        .unwrap();
    let same = client(&serve_once("HTTP/1.1 200 OK", "", A).await)
        .fetch("p", "d", "win64")
        .await
        .unwrap();
    let different = client(&serve_once("HTTP/1.1 200 OK", "", B).await)
        .fetch("p", "d", "win64")
        .await
        .unwrap();

    assert_eq!(first.digest(), same.digest());
    assert_ne!(first.digest(), different.digest());
}

#[tokio::test]
async fn non_success_status_is_an_error() {
    let base = serve_once("HTTP/1.1 404 Not Found", "", b"nope").await;
    let err = client(&base)
        .fetch("p", "d", "win64")
        .await
        .expect_err("404 must not be treated as a snapshot");
    assert!(err.to_string().contains("404"), "got: {err}");
}

#[tokio::test]
async fn a_binary_payload_still_yields_modules() {
    const BODY: &[u8] =
        b"\x00\x01MZ\x00C:\\Program Files\\EasyAntiCheat\\driver.sys\x00\xffclient.dll\x00";
    let base = serve_once("HTTP/1.1 200 OK", "", BODY).await;
    let snapshot = client(&base).fetch("p", "d", "win64").await.unwrap();

    let names: Vec<_> = snapshot.modules.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, vec!["driver.sys", "client.dll"]);
    // No cache validators were sent, so the embed falls back to its own note.
    assert!(snapshot.etag.is_none());
}

#[tokio::test]
async fn probe_reports_a_live_target_without_downloading_it() {
    // A HEAD-capable server: the probe must not pull the body.
    let base = serve_once("HTTP/1.1 200 OK", "Content-Length: 22020096\r\n", b"").await;
    let probe = client(&base).probe("p", "d", "win64").await.unwrap();

    assert!(probe.ok());
    assert_eq!(probe.status, 200);
    assert_eq!(probe.platform, "win64");
    assert!(probe.url.ends_with("/p/d/win64"));
}

#[tokio::test]
async fn probe_returns_a_missing_target_as_a_status_not_an_error() {
    let base = serve_once("HTTP/1.1 404 Not Found", "", b"").await;
    let probe = client(&base)
        .probe("wrong", "ids", "win64")
        .await
        .expect("a 404 is an answer, not a failure");

    assert!(!probe.ok());
    assert_eq!(probe.status, 404);
}

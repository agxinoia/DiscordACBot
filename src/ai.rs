//! NVIDIA NIM API client and rate-limiting wrapper.
//!
//! Provides integration with NVIDIA's OpenAI-compatible inference API
//! (`https://integrate.api.nvidia.com/v1`) for binary change analysis,
//! function diff summarization, and reverse-engineering assistance.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;

pub const NVIDIA_API_BASE: &str = "https://integrate.api.nvidia.com/v1";
pub const DEFAULT_MODEL: &str = "z-ai/glm-5.3-flash";
/// Default delay between API calls (1500ms aligns with 40 RPM plan: 60,000 / 40 = 1500ms).
pub const DEFAULT_DELAY_MS: u64 = 1500;

/// Canonicalize model identifier (e.g. normalize z-ai/glm-5-3-flash to z-ai/glm-5.3-flash).
pub fn canonicalize_model(model: &str) -> &str {
    let trimmed = model.trim();
    if trimmed == "z-ai/glm-5-3-flash" || trimmed.is_empty() {
        DEFAULT_MODEL
    } else {
        trimmed
    }
}


/// Mask an API key so it can be safely displayed in Discord UI without leaking secrets.
pub fn mask_key(key: &str) -> String {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return "*(none)*".to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("nvapi-") {
        if rest.len() >= 8 {
            let end = &rest[rest.len() - 4..];
            return format!("nvapi-••••{end}");
        }
        return "nvapi-••••".to_string();
    }
    if trimmed.len() >= 8 {
        let start = &trimmed[..3];
        let end = &trimmed[trimmed.len() - 4..];
        format!("{start}••••{end}")
    } else {
        "••••••••".to_string()
    }
}

/// Global rate limiter state ensuring requests respect the configured delay interval.
#[derive(Debug)]
pub struct RateLimiter {
    last_request: Mutex<Option<Instant>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            last_request: Mutex::new(None),
        }
    }

    /// Wait if necessary to ensure `delay_ms` milliseconds have elapsed since the last request.
    pub async fn throttle(&self, delay_ms: u64) {
        if delay_ms == 0 {
            return;
        }
        let mut last = self.last_request.lock().await;
        let now = Instant::now();
        if let Some(prev) = *last {
            let min_duration = Duration::from_millis(delay_ms);
            let elapsed = now.duration_since(prev);
            if elapsed < min_duration {
                let to_wait = min_duration - elapsed;
                tokio::time::sleep(to_wait).await;
            }
        }
        *last = Some(Instant::now());
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

fn global_limiter() -> &'static RateLimiter {
    static LIMITER: OnceLock<RateLimiter> = OnceLock::new();
    LIMITER.get_or_init(RateLimiter::new)
}

#[derive(Serialize)]
struct ChatCompletionRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    temperature: f32,
    top_p: f32,
    max_tokens: u32,
    stream: bool,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ChatMessage<'a> {
    pub role: &'a str,
    pub content: &'a str,
}

#[derive(Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatChoice>,
    #[serde(default)]
    error: Option<ChatError>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessageResponse,
}

#[derive(Deserialize)]
struct ChatMessageResponse {
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
}

#[derive(Deserialize, Debug)]
struct ChatError {
    message: String,
}

/// Execute a raw chat completion against the NVIDIA NIM endpoint with rate limiting.
pub async fn chat_completion(
    api_key: &str,
    model: &str,
    delay_ms: u64,
    messages: Vec<ChatMessage<'_>>,
    temperature: f32,
    max_tokens: u32,
) -> Result<String> {
    let key = api_key.trim();
    if key.is_empty() {
        bail!("NVIDIA API key is not configured.");
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .build()
        .context("building reqwest client")?;

    let url = format!("{NVIDIA_API_BASE}/chat/completions");
    let active_model = canonicalize_model(model);
    let payload = ChatCompletionRequest {
        model: active_model,
        messages,
        temperature,
        top_p: 1.0,
        max_tokens,
        stream: false,
    };
    let json_bytes = serde_json::to_vec(&payload).context("serializing chat payload")?;

    const MAX_RETRIES: u32 = 3;
    let mut attempt = 0;

    // Floor the request spacing at 1500ms to guarantee never exceeding 40 requests per minute
    let effective_delay = delay_ms.max(1500);

    loop {
        attempt += 1;
        global_limiter().throttle(effective_delay).await;

        let resp = client
            .post(&url)
            .bearer_auth(key)
            .header("Content-Type", "application/json")
            .body(json_bytes.clone())
            .send()
            .await
            .context("sending request to NVIDIA API")?;

        let status = resp.status();

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            if attempt > MAX_RETRIES {
                bail!(
                    "NVIDIA NIM API rate limit (40 RPM) reached. Throttling backoff exhausted after {} retries.                      Aborting request to prevent quota exhaustion.",
                    MAX_RETRIES
                );
            }

            let retry_after_secs = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or_else(|| (attempt as u64) * 2);

            tracing::warn!(
                attempt,
                retry_after_secs,
                "NVIDIA NIM API returned HTTP 429 Too Many Requests; backing off"
            );
            tokio::time::sleep(Duration::from_secs(retry_after_secs)).await;
            continue;
        }

        if status.is_server_error() && attempt <= 2 {
            tracing::warn!(attempt, %status, "NVIDIA API server error; retrying once after 2s");
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }

        let body_text = resp.text().await.context("reading response body")?;

        if !status.is_success() {
            bail!("NVIDIA API returned HTTP {status}: {body_text}");
        }

        let parsed: ChatCompletionResponse = serde_json::from_str(&body_text)
            .with_context(|| format!("parsing JSON response: {body_text}"))?;

        if let Some(err) = parsed.error {
            bail!("NVIDIA API error: {}", err.message);
        }

        let content = parsed
            .choices
            .into_iter()
            .find_map(|c| {
                c.message.content.filter(|s| !s.trim().is_empty())
                    .or(c.message.reasoning_content.filter(|s| !s.trim().is_empty()))
            })
            .context("no content returned in choice")?;

        return Ok(content.trim().to_string());
    }
}

/// Test connection to NVIDIA NIM API with the given key and model.
pub async fn test_connection(api_key: &str, model: &str, delay_ms: u64) -> Result<String> {
    let messages = vec![
        ChatMessage {
            role: "system",
            content: "You are an automated API connectivity verifier. Respond concisely.",
        },
        ChatMessage {
            role: "user",
            content: "Ping. Confirm API connection and state your active model name.",
        },
    ];

    chat_completion(api_key, model, delay_ms, messages, 0.1, 128).await
}

/// Request AI analysis of binary changes between two module snapshots.
pub async fn summarize_diff(
    api_key: &str,
    model: &str,
    delay_ms: u64,
    game: &str,
    platform: &str,
    diff_summary: &str,
    decompilation_context: Option<&str>,
) -> Result<String> {
    let mut prompt = format!(
        "Game: {game}\nPlatform: {platform}\n\nBinary Diff Metrics:\n{diff_summary}\n"
    );

    if let Some(extra) = decompilation_context.filter(|s| !s.trim().is_empty()) {
        prompt.push_str(&format!("\nExtracted Code / Symbol Context:\n{extra}\n"));
    }

    let messages = vec![
        ChatMessage {
            role: "system",
            content: "You are a senior reverse engineering and binary analysis assistant. \
                      Given information about updated binary modules (such as size deltas, \
                      fuzzy hash distances, exported symbols, or decompiled snippets), provide \
                      a crisp, direct, technical summary of the changes in 2-3 focused sentences. \
                      Highlight what moved, likely architectural/toolchain changes (e.g. full rebuilds, \
                      compiler/engine upgrades, dead-code elimination), and the practical impact on reverse engineering.",
        },
        ChatMessage {
            role: "user",
            content: &prompt,
        },
    ];

    chat_completion(api_key, model, delay_ms, messages, 0.4, 1536).await
}

/// Request specialized AI devirtualization and reverse-engineering analysis.
pub async fn devirtualize_analysis(
    api_key: &str,
    model: &str,
    delay_ms: u64,
    game: &str,
    platform: &str,
    module_name: &str,
    heuristic_report: &str,
    decompiled_context: Option<&str>,
) -> Result<String> {
    let mut prompt = format!(
        "Target Game: {game}\nPlatform: {platform}\nModule: {module_name}\n\n=== Static Protection & VM Heuristics ===\n{heuristic_report}\n"
    );

    if let Some(extra) = decompiled_context.filter(|s| !s.trim().is_empty()) {
        let bounded_extra = if extra.len() > 10_000 {
            let end = extra.char_indices().map(|(i, _)| i).take_while(|i| *i < 10_000).last().unwrap_or(extra.len());
            &extra[..end]
        } else {
            extra
        };
        prompt.push_str(&format!(
            "\n=== Extracted Decompiled Functions & Dispatcher Bodies ===\n{bounded_extra}\n"
        ));
    }

    prompt.push_str(
        "\nProvide a comprehensive reverse-engineering report structured as follows:\n         1. **Protection & VM Architecture Identification**: Identify the virtualizer (EAC custom VM, VMProtect, Themida, etc.), describe dispatcher loop structure, virtual program counter (VPC), and virtual registers.\n         2. **Bytecode Handler Analysis**: Analyze candidate bytecode handlers, decode opcode semantics (arithmetic, memory access, stack ops, control flow jumps).\n         3. **Devirtualized High-Level Reconstruction**: Provide clean, reconstructed C pseudo-code recovering the original pre-virtualized logic.\n         4. **Anti-Analysis & Security Insights**: Note any integrity checks, timing detections (RDTSC), or anti-tamper mechanisms present."
    );

    let messages = vec![
        ChatMessage {
            role: "system",
            content: "You are an elite software security researcher and expert in binary devirtualization,                       symbolic execution, and virtual machine-based code protection analysis.                       Analyze the supplied disassembly, decompiled Ghidra snippets, and static heuristics                       to provide a rigorous, actionable devirtualization and native logic recovery report.",
        },
        ChatMessage {
            role: "user",
            content: &prompt,
        },
    ];

    chat_completion(api_key, model, delay_ms, messages, 0.3, 2048).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mask_key() {
        assert_eq!(mask_key(""), "*(none)*");
        assert_eq!(mask_key("   "), "*(none)*");
        assert_eq!(mask_key("nvapi-12345678abcdef"), "nvapi-••••cdef");
        assert_eq!(mask_key("nvapi-short"), "nvapi-••••");
        assert_eq!(mask_key("123456789"), "123••••6789");
        assert_eq!(mask_key("short"), "••••••••");
    }

    #[tokio::test]
    async fn test_rate_limiter_throttles() {
        let limiter = RateLimiter::new();
        let start = Instant::now();
        limiter.throttle(50).await;
        limiter.throttle(50).await;
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(45));
    }

    #[tokio::test]
    async fn test_empty_api_key_fails() {
        let res = chat_completion("", DEFAULT_MODEL, 0, vec![], 0.5, 10).await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("not configured"));
    }
}

//! A Discord bot that watches Easy Anti-Cheat module endpoints on the Epic
//! Games CDN and posts an embed whenever a game's modules change.

use anyhow::{Context, Result};
use eac_tracker::embed::human_bytes;
use eac_tracker::{bot, catalog, config, discover, eac, settings, tracker};
use serenity::all::GatewayIntents;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "\
eac-tracker — Easy Anti-Cheat module update tracker

USAGE:
    eac-tracker                        Run the bot (default)
    eac-tracker discover [PATH]...     Find EAC ids in installed games
    eac-tracker probe ID ID [PLAT]...  Check whether an id pair is live
    eac-tracker catalog [--probe]      List the built-in known games
    eac-tracker --help

PROBE:
    eac-tracker probe <product_id> <deployment_id> [platform]...

    With no platform, every known candidate is tried. Use it to check ids
    found on a website before adding them. Exits non-zero if nothing is
    published for the pair.

    --name NAME      Game name to use in the printed /eac add command
    --json           Emit JSON instead of a report

DISCOVER OPTIONS:
    --probe          Check which platforms each deployment actually publishes
    --depth N        Directory depth to search (default 8)
    --json           Emit JSON instead of pasteable commands

With no PATH, discover searches the usual Steam library locations. Point it at
any readable directory — a Proton prefix or a mounted Windows drive both work.

ENVIRONMENT:
    DISCORD_TOKEN     Bot token. The only required setting.
    DISCORD_GUILD_ID  Register /eac to one server for instant availability.
    EAC_CONFIG        Optional config file path (default config.toml).
    EAC_CDN_BASE      Override the module CDN base URL (mirrors, testing).
    EAC_LOG           Log filter, e.g. debug.
";

/// The CDN to talk to from the command line. `EAC_CDN_BASE` mirrors the
/// `tracker.cdn_base` config key the bot uses.
fn cdn_base() -> String {
    std::env::var("EAC_CDN_BASE")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| eac::CDN_BASE.to_string())
}

fn probe_client() -> Result<eac::Client> {
    eac::Client::with_base(
        &cdn_base(),
        concat!("eac-tracker/", env!("CARGO_PKG_VERSION")),
        Duration::from_secs(20),
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(String::as_str) {
        Some("--help" | "-h" | "help") => {
            print!("{USAGE}");
            Ok(())
        }
        Some("discover") => run_discover(&args[1..]).await,
        Some("probe") => run_probe(&args[1..]).await,
        Some("catalog" | "catalogue") => run_catalog(&args[1..]).await,
        Some(other) if other.starts_with('-') => {
            eprintln!("unknown option `{other}`\n\n{USAGE}");
            std::process::exit(2);
        }
        _ => run_bot().await,
    }
}

fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("EAC_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}

async fn run_bot() -> Result<()> {
    init_logging();

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

async fn run_discover(args: &[String]) -> Result<()> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut probe = false;
    let mut as_json = false;
    let mut depth = 8usize;

    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--probe" => probe = true,
            "--json" => as_json = true,
            "--depth" => {
                depth = rest
                    .next()
                    .and_then(|v| v.parse().ok())
                    .context("--depth needs a number")?;
            }
            other if other.starts_with('-') => {
                eprintln!("unknown discover option `{other}`\n\n{USAGE}");
                std::process::exit(2);
            }
            path => roots.push(PathBuf::from(path)),
        }
    }

    if roots.is_empty() {
        roots = discover::default_roots();
        if roots.is_empty() {
            eprintln!(
                "No Steam library found in the usual places. Pass a path, for example:\n\
                 \n    eac-tracker discover /mnt/games\n\
                 \nA mounted Windows drive or a Proton prefix works too."
            );
            std::process::exit(1);
        }
        eprintln!(
            "Searching {}",
            roots
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let mut findings = Vec::new();
    for root in &roots {
        findings.extend(discover::scan(root, depth));
    }

    if findings.is_empty() {
        eprintln!(
            "Found no EasyAntiCheat configuration. Games that do not use EAC \
             have none, and some launchers keep it outside the install directory."
        );
        return Ok(());
    }

    if probe {
        let client = probe_client()?;
        let candidates: Vec<String> = eac::CANDIDATE_PLATFORMS
            .iter()
            .map(|p| (*p).to_string())
            .collect();

        for finding in &mut findings {
            eprintln!("Probing {}...", finding.game);
            let mut unreachable = 0usize;
            for platform in &candidates {
                match client
                    .probe(&finding.product_id, &finding.deployment_id, platform)
                    .await
                {
                    Ok(p) if p.ok() => {
                        println!("  {:<16} {}", platform, describe_size(p.content_length));
                        finding.platforms.push(platform.clone());
                    }
                    Ok(_) => {}
                    Err(e) => {
                        // `{e:#}` so the cause is shown, not just the context.
                        eprintln!("  {platform}: {e:#}");
                        unreachable += 1;
                    }
                }
            }
            if finding.platforms.is_empty() {
                // A network failure and a wrong id pair are different problems.
                if unreachable == candidates.len() {
                    eprintln!("  could not reach the CDN — see the errors above");
                } else {
                    eprintln!("  nothing published — the ids may be for a different EAC backend");
                }
            }
        }
    }

    if as_json {
        println!("{}", serde_json::to_string_pretty(&findings)?);
        return Ok(());
    }

    println!("\nFound {} deployment(s):\n", findings.len());
    for finding in &findings {
        println!("{}", finding.game);
        println!("  product_id:    {}", finding.product_id);
        println!("  deployment_id: {}", finding.deployment_id);
        if let Some(sandbox) = &finding.sandbox_id {
            println!("  sandbox_id:    {sandbox}  (not used by the tracker)");
        }
        println!("  source:        {}", finding.source.display());
        if probe {
            match finding.platforms.as_slice() {
                [] => println!("  platforms:     none responded"),
                found => println!("  platforms:     {}", found.join(", ")),
            }
        }
        println!("\n  {}\n", finding.command());
    }

    if !probe {
        eprintln!(
            "Platforms are a guess without --probe; re-run with it to confirm \
             which ones each deployment actually publishes."
        );
    }
    Ok(())
}

/// One platform's probe result, for `--json`.
#[derive(serde::Serialize)]
struct ProbeReport {
    platform: String,
    url: String,
    live: bool,
    status: Option<u16>,
    size: Option<u64>,
    error: Option<String>,
}

async fn run_probe(args: &[String]) -> Result<()> {
    let mut positional: Vec<String> = Vec::new();
    let mut as_json = false;
    let mut name = String::from("Game Name");

    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--json" => as_json = true,
            "--name" => {
                name = rest.next().context("--name needs a value")?.clone();
            }
            other if other.starts_with('-') => {
                eprintln!("unknown probe option `{other}`\n\n{USAGE}");
                std::process::exit(2);
            }
            value => positional.push(value.to_string()),
        }
    }

    let [product_id, deployment_id, platforms @ ..] = positional.as_slice() else {
        eprintln!("usage: eac-tracker probe <product_id> <deployment_id> [platform]...");
        std::process::exit(2);
    };

    // Reject malformed ids here rather than sending a doomed request.
    settings::validate_id("product_id", product_id)?;
    settings::validate_id("deployment_id", deployment_id)?;
    let platforms: Vec<String> = if platforms.is_empty() {
        eac::CANDIDATE_PLATFORMS
            .iter()
            .map(|p| (*p).to_string())
            .collect()
    } else {
        for platform in platforms {
            settings::validate_platform(platform)?;
        }
        platforms.to_vec()
    };

    let client = probe_client()?;
    let mut reports = Vec::new();
    let mut live = Vec::new();
    let mut reachable = false;

    for platform in &platforms {
        let report = match client.probe(product_id, deployment_id, platform).await {
            Ok(p) => {
                reachable = true;
                if p.ok() {
                    live.push(platform.clone());
                }
                ProbeReport {
                    platform: platform.clone(),
                    url: p.url.clone(),
                    live: p.ok(),
                    status: Some(p.status),
                    size: p.content_length,
                    error: None,
                }
            }
            Err(e) => ProbeReport {
                platform: platform.clone(),
                url: eac::module_url_with_base(&cdn_base(), product_id, deployment_id, platform),
                live: false,
                status: None,
                size: None,
                error: Some(format!("{e:#}")),
            },
        };
        if !as_json {
            match (&report.error, report.live) {
                (Some(error), _) => println!("  {:<10} unreachable — {error}", report.platform),
                (None, true) => println!(
                    "  {:<10} live    {}",
                    report.platform,
                    report
                        .size
                        .map(eac_tracker::embed::human_bytes)
                        .unwrap_or_else(|| "size unknown".into())
                ),
                (None, false) => println!(
                    "  {:<10} not published (HTTP {})",
                    report.platform,
                    report.status.unwrap_or(0)
                ),
            }
        }
        reports.push(report);
    }

    if as_json {
        println!("{}", serde_json::to_string_pretty(&reports)?);
    } else if live.is_empty() {
        println!();
        if reachable {
            println!(
                "Nothing is published for that pair. Check the ids, or the game may \
                 use the legacy EAC backend (an EasyAntiCheat folder without the \
                 _EOS suffix) rather than this CDN."
            );
        } else {
            println!("Could not reach the CDN — see the errors above.");
        }
    } else {
        println!();
        println!(
            "/eac add game:{name} product_id:{product_id} deployment_id:{deployment_id} platforms:{}",
            live.join(", ")
        );
    }

    if live.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

async fn run_catalog(args: &[String]) -> Result<()> {
    let probe = args.iter().any(|a| a == "--probe");
    let as_json = args.iter().any(|a| a == "--json");
    if let Some(bad) = args
        .iter()
        .find(|a| !matches!(a.as_str(), "--probe" | "--json"))
    {
        eprintln!("unknown catalog option `{bad}`\n\n{USAGE}");
        std::process::exit(2);
    }

    #[derive(serde::Serialize)]
    struct Published {
        platform: String,
        bytes: Option<u64>,
    }

    #[derive(serde::Serialize)]
    struct Entry {
        name: &'static str,
        product_id: &'static str,
        deployment_id: &'static str,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        platforms: Vec<Published>,
    }

    let client = probe.then(probe_client).transpose()?;
    let mut entries = Vec::new();

    for known in catalog::KNOWN {
        let mut platforms = Vec::new();
        if let Some(client) = &client {
            eprintln!("Probing {}...", known.name);
            for platform in eac::CANDIDATE_PLATFORMS {
                match client
                    .probe(known.product_id, known.deployment_id, platform)
                    .await
                {
                    Ok(p) if p.ok() => platforms.push(Published {
                        platform: (*platform).to_string(),
                        bytes: p.content_length,
                    }),
                    Ok(_) => {}
                    Err(e) => eprintln!("  {platform}: {e:#}"),
                }
            }
        }
        entries.push(Entry {
            name: known.name,
            product_id: known.product_id,
            deployment_id: known.deployment_id,
            platforms,
        });
    }

    if as_json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }

    println!();
    for entry in &entries {
        println!("{}", entry.name);
        println!("  product_id:    {}", entry.product_id);
        println!("  deployment_id: {}", entry.deployment_id);
        if probe {
            if entry.platforms.is_empty() {
                println!("  platforms:     none responded");
            } else {
                println!("  platforms:");
                for published in &entry.platforms {
                    println!(
                        "    {:<16} {}",
                        published.platform,
                        describe_size(published.bytes)
                    );
                }
            }
        }
        println!();
    }
    if !probe {
        eprintln!(
            "These pairs come from published research and are not verified here. \
             Re-run with --probe to check them against the CDN."
        );
    }
    Ok(())
}

/// Render a probed size, flagging one too small to be a real module.
///
/// A 2xx does not by itself prove a platform is published — a CDN can answer
/// with an error document — so the size is what distinguishes a real module
/// from an alias that merely resolves.
fn describe_size(bytes: Option<u64>) -> String {
    match bytes {
        Some(n) if n < 4096 => format!("{} — too small to be a module", human_bytes(n)),
        Some(n) => human_bytes(n),
        None => "size not reported".to_string(),
    }
}

//! A Discord bot that watches Easy Anti-Cheat module endpoints on the Epic
//! Games CDN and posts an embed whenever a game's modules change.

use anyhow::{Context, Result};
use eac_tracker::{bot, config, discover, eac, settings, tracker};
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
    eac-tracker --help

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
    EAC_LOG           Log filter, e.g. debug.
";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(String::as_str) {
        Some("--help" | "-h" | "help") => {
            print!("{USAGE}");
            Ok(())
        }
        Some("discover") => run_discover(&args[1..]).await,
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
        let client = eac::Client::with_base(
            eac::CDN_BASE,
            "eac-tracker-discover",
            Duration::from_secs(20),
        )?;
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
                    Ok(p) if p.ok() => finding.platforms.push(platform.clone()),
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

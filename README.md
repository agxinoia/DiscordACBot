# EAC Update Tracker

A Discord bot that watches Easy Anti-Cheat module endpoints on the Epic Games
CDN and posts an embed whenever a game's modules change.

```
EAC Update Detected for ARC Raiders
Game            Platform          Download
ARC Raiders     win64             22.0 MB

Hash
d6b8cbf936b39c52

Raw Response
22.0 MB split into 3 part(s), each under 9.1 MB.

driver.sys (arm64)    usermode.exe (x86)    client.dll (x64)
16.5 MB               1.0 MB                20.0 MB
bc1f4446a7008207      b289b022c438cbf6      272d0e577de143a0
```

## How it works

EAC modules are distributed from an Epic Games endpoint addressed by a game's
product id, deployment id and platform:

```
https://modules-cdn.eac-prod.on.epicgames.com/modules/{product_id}/{deployment_id}/{platform}
```

Every `poll_interval_secs`, the bot fetches each configured target and takes the
SHA-256 of the raw response body. When that digest differs from the one it last
recorded, it posts an embed and (optionally) attaches the raw response.

### A note on the payload format

The response format is not publicly documented and Epic has changed it before,
so **nothing here depends on it**. Change detection is driven purely by hashing
the raw bytes, which is correct for any format.

The per-module breakdown (`driver.sys (arm64) — 16.5 MB`) is a best-effort
enrichment layer on top: `parse_modules` tries JSON first — handling both
`{"modules": [{"name": …}]}` and `{"driver.sys": {"size": …}}` shapes — and
falls back to scanning the blob for embedded module filenames. If both come up
empty you still get a correct update embed, just without the breakdown. If you
learn the real schema, `src/eac.rs` is the only file that needs to change.

## Setup

1. Create an application at the [Discord Developer Portal](https://discord.com/developers/applications),
   add a bot, and copy its token.
2. Invite it with the `bot` and `applications.commands` scopes and the
   **Send Messages**, **Embed Links** and **Attach Files** permissions. No
   privileged intents are required.
3. Configure and run:

```sh
cp config.example.toml config.toml
$EDITOR config.toml            # set discord.channel_id and your games
export DISCORD_TOKEN=...       # preferred over putting the token in the file
cargo run --release
```

`EAC_CONFIG` overrides the config path; `EAC_LOG` takes a
[tracing filter](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html)
such as `EAC_LOG=debug`.

### Finding product and deployment ids

Both come from the game's own EasyAntiCheat configuration — typically
`EasyAntiCheat/Settings.json` in the install directory, or the launcher's EOS
config. The pair in `config.example.toml` is ARC Raiders' Win64 deployment,
taken from the URL above.

## Deploying on Linux

The only build dependencies are a linker and the Rust toolchain. There is no
OpenSSL or other native library in the dependency graph: TLS is pure-Rust
rustls and the CA roots are compiled into the binary, so the release build
links against nothing but `libc`, `libm` and `libgcc_s`.

**Install Rust with rustup, not `apt install rustc`.** This crate uses edition
2024, which needs Rust 1.85 or newer; the rustc packaged by current Ubuntu
releases is older than that and will fail to build it.

```sh
sudo apt update
sudo apt install -y build-essential git
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"

git clone https://github.com/agxinoia/DiscordBot.git
cd DiscordBot
cargo build --release
```

Then install the binary, config and credentials:

```sh
sudo useradd --system --no-create-home --shell /usr/sbin/nologin eac-tracker
sudo install -d -o eac-tracker -g eac-tracker /opt/eac-tracker
sudo install -o eac-tracker -g eac-tracker -m 755 \
    target/release/eac-tracker /opt/eac-tracker/
sudo install -o eac-tracker -g eac-tracker -m 644 \
    config.toml /opt/eac-tracker/

sudo install -m 600 deploy/eac-tracker.env.example /etc/eac-tracker.env
sudo $EDITOR /etc/eac-tracker.env        # set DISCORD_TOKEN

sudo install -m 644 deploy/eac-tracker.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now eac-tracker
journalctl -u eac-tracker -f
```

The unit runs as an unprivileged system user under `ProtectSystem=strict`.
`ReadWritePaths=/opt/eac-tracker` is what allows `state.json` to be written —
remove that line and the bot cannot persist digests. If you move
`tracker.state_path` elsewhere, add that path to `ReadWritePaths` too.

Two things to expect on a first run:

- **Polling does not begin until the gateway connects.** The loop is started
  from the `ready` event, so a bad token shows up as reconnect warnings with no
  `state.json` appearing.
- **Nothing is posted on the first sweep**, because `announce_on_first_seen`
  defaults to false. Set it true temporarily to confirm the embed renders
  without waiting for a real EAC update.

If you build on one machine and copy the binary to another, the target needs a
glibc at least as new as the build host's. Building on the target avoids it.

## Configuration

| Key | Default | Meaning |
| --- | --- | --- |
| `discord.channel_id` | — | Channel that update embeds are posted to. |
| `discord.token` | — | Overridden by `DISCORD_TOKEN`. Prefer the env var. |
| `discord.guild_id` | unset | Register `/eac` to one guild (instant) instead of globally (up to an hour). |
| `tracker.poll_interval_secs` | `300` | Seconds between sweeps. Minimum 30. |
| `tracker.announce_on_first_seen` | `false` | Post an embed the first time a target is seen. Leave off so a fresh deployment does not fire one embed per target on startup. |
| `tracker.attach_raw_response` | `true` | Attach the raw CDN response to the embed. |
| `tracker.max_attachment_bytes` | `9500000` | Upload chunk size, just under Discord's 10 MB per-file limit. Bodies larger than this are split into numbered parts (max 10 per message). |
| `tracker.state_path` | `state.json` | Where last-seen digests are persisted. |
| `tracker.thumbnail_url` | unset | Image shown in the embed's corner. |
| `tracker.cdn_base` | official CDN | Override for mirrors and testing. |

State is written atomically (temp file + rename). A corrupt state file is a
startup error rather than a silent reset, since resetting would re-announce
every tracked module.

## Commands

| Command | Description |
| --- | --- |
| `/eac list` | Games and platforms being tracked. |
| `/eac status` | Last seen hash, size and time per target. |
| `/eac check <game> [platform]` | Fetch now and report current state, changed or not. Game name autocompletes. |

`/eac check` reaches out to the CDN on demand and is available to everyone in
the guild by default. If that matters for your server, restrict the command
under **Server Settings → Integrations**.

## Development

```sh
cargo test           # unit + integration tests
cargo clippy --all-targets
cargo fmt
```

The integration tests under `tests/` run against a throwaway local HTTP server
rather than the real CDN, so they are hermetic and cover the full cycle: first
sighting, a published change, a no-op re-check, and state surviving a restart.

## Layout

| File | Responsibility |
| --- | --- |
| `src/eac.rs` | CDN client, hashing, module parsing. |
| `src/state.rs` | Persisted last-seen digests. |
| `src/tracker.rs` | Poll loop, change detection, posting. |
| `src/embed.rs` | Embed rendering and attachment chunking. |
| `src/bot.rs` | Gateway wiring and `/eac`. |
| `deploy/` | systemd unit and environment file template. |

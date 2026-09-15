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
3. Run it. The token is the only thing you need to supply:

```sh
export DISCORD_TOKEN=...
cargo run --release
```

4. In your server, configure it with slash commands:

```
/eac setup channel:#eac-updates
/eac add game:ARC Raiders
        product_id:9e8b37541e614575b4de303d2c2e44cf
        deployment_id:35e06571d8ab4de4b98519b624125459
        platforms:win64
```

That is the whole setup. Settings are stored per server in `settings.json`, so
one bot can serve several servers without their configuration colliding.

`/eac` registers globally by default, which can take up to an hour to appear.
For instant availability while setting up, set `DISCORD_GUILD_ID` to your
server's id and the command registers to that server only.

There is no config file in the normal case. `config.toml` is optional and only
changes operator-level defaults — paths, timeouts, and fallbacks for servers
that have not configured themselves. See `config.example.toml`.

### Finding product and deployment ids

Both come from the game's own EasyAntiCheat configuration — typically
`EasyAntiCheat/Settings.json` in the install directory, or the launcher's EOS
config. The pair in `config.example.toml` is ARC Raiders' Win64 deployment,
taken from the URL above.

## The archive

Change detection alone is lossy: once a new payload lands, the bytes it
replaced are gone. The archive keeps every distinct payload so history can be
diffed after the fact.

```
archive/
  index.jsonl              # one JSON object per fetch, append-only
  blobs/<xx>/<sha256>.bin  # payloads, sharded by the first byte of the digest
```

Blobs are content-addressed, so an unchanged payload is never stored twice, and
each index record carries `previous_sha256` to chain a payload to the one it
replaced. Each record holds the full MD5/SHA-1/SHA-256, the TLSH fuzzy hash,
the detected container format, Shannon entropy, **every response header**, the
parsed module list, and the PE metadata below.

The archive grows without bound; it is a corpus, not a cache. Prune it yourself
if disk matters, but note that deleting a blob costs the TLSH distance on the
next update — tlsh2 cannot rebuild a comparable hash from a stored string, so
measuring how far a build moved requires the previous payload's actual bytes.

## What gets extracted

Beyond "the hash changed", each update reports as much as the payload allows.
All of it is best-effort and never fails a poll.

**Always**
- Full MD5, SHA-1 and SHA-256, untruncated, for cross-referencing external
  sample databases.
- TLSH fuzzy hash, and the distance from the previous payload: 0 is identical,
  under ~30 a small patch, over ~200 effectively unrelated. This is what tells
  you whether an update is worth opening before you open it.
- Container format from magic bytes, and Shannon entropy — a packer appearing
  or disappearing shows up here first.
- Every response header. `Last-Modified` is the closest thing the CDN gives to
  a publish timestamp.

**When the payload is a PE image**
- COFF `TimeDateStamp`, the single most useful field for correlating a build
  against other artifacts, and reported as a rebuild when it moves.
- Machine type, which is what actually makes the platform concrete (`arm64`
  versus `x64`), subsystem, image base, export count.
- The CodeView PDB path, i.e. the build machine's source layout.
- Per-section raw size and entropy.
- Imported DLLs.
- `VS_VERSIONINFO`: file and product versions plus CompanyName,
  OriginalFilename and the rest of StringFileInfo.
- Authenticode certificates from the attribute certificate table: signer
  subject, issuer, serial and validity window. A signing-certificate rotation
  is a notable event in its own right.

If the payload turns out to be an encrypted or custom container rather than a
PE or an archive of them, the PE layer simply reports nothing and the rest
still works. Identifying that container would be the next step, and
`src/analysis.rs` is where it would go.

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

Install the binary and the token. No config file is needed:

```sh
sudo useradd --system --no-create-home --shell /usr/sbin/nologin eac-tracker
sudo install -d -o eac-tracker -g eac-tracker /opt/eac-tracker
sudo install -o eac-tracker -g eac-tracker -m 755 \
    target/release/eac-tracker /opt/eac-tracker/

sudo install -m 600 deploy/eac-tracker.env.example /etc/eac-tracker.env
sudo nano /etc/eac-tracker.env           # set DISCORD_TOKEN
```

Editing that file is not optional — it ships with a placeholder token. Use a
concrete editor rather than `sudo $EDITOR`: when `EDITOR` is unset that expands
to `sudo /etc/eac-tracker.env`, which tries to *execute* the file and fails with
a permission error.

Finally, install and start the service:

```sh
sudo install -m 644 deploy/eac-tracker.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now eac-tracker
journalctl -u eac-tracker -f
```

The unit runs as an unprivileged system user under `ProtectSystem=strict`.
`ReadWritePaths=/opt/eac-tracker` is what allows `state.json` to be written —
remove that line and the bot cannot persist digests. If you move
`tracker.state_path` elsewhere, add that path to `ReadWritePaths` too.

Once it is running, configure it from Discord with `/eac setup` and `/eac add`.
The bot writes `settings.json`, `state.json` and `archive/` into
`/opt/eac-tracker`, which is what `ReadWritePaths` in the unit permits.

Two things to expect on a first run:

- **Polling does not begin until the gateway connects.** The loop is started
  from the `ready` event, so a bad token shows up as reconnect warnings with no
  `state.json` appearing.
- **Nothing is posted on the first sweep**, because `announce_on_first_seen`
  defaults to false — the first pass records baselines. To confirm the embed
  renders without waiting for a real EAC update, run `/eac check`, or
  `/eac set option:announce_on_first_seen value:true`.

If you build on one machine and copy the binary to another, the target needs a
glibc at least as new as the build host's. Building on the target avoids it.

## Configuration

Everything below is optional and lives in `config.toml`. Per-server settings
(`/eac setup`, `/eac add`, `/eac set`) override these and are stored separately
in `settings.path`; the values here apply to servers that have not set their own.

| Key | Default | Meaning |
| --- | --- | --- |
| `discord.token` | — | Overridden by `DISCORD_TOKEN`. Prefer the env var. |
| `discord.guild_id` | unset | Register `/eac` to one guild (instant) instead of globally (up to an hour). Also `DISCORD_GUILD_ID`. |
| `discord.channel_id` | unset | Fallback announce channel for servers that have not run `/eac setup`. |
| `tracker.poll_interval_secs` | `300` | Seconds between sweeps. Minimum 30. |
| `tracker.announce_on_first_seen` | `false` | Post an embed the first time a target is seen. Leave off so a fresh deployment does not fire one embed per target on startup. |
| `tracker.attach_raw_response` | `true` | Attach the raw CDN response to the embed. |
| `tracker.max_attachment_bytes` | `9500000` | Upload chunk size, just under Discord's 10 MB per-file limit. Bodies larger than this are split into numbered parts (max 10 per message). |
| `tracker.state_path` | `state.json` | Where last-seen digests are persisted. |
| `tracker.settings_path` | `settings.json` | Where per-server configuration from `/eac` is persisted. |
| `tracker.archive_path` | `archive` | Payload archive directory. `""` disables it, which also disables TLSH distance. |
| `tracker.thumbnail_url` | unset | Image shown in the embed's corner. |
| `tracker.cdn_base` | official CDN | Override for mirrors and testing. |

State is written atomically (temp file + rename). A corrupt state file is a
startup error rather than a silent reset, since resetting would re-announce
every tracked module.

## Commands

Changing configuration requires the **Manage Server** permission; reading does
not. Replies are ephemeral, so configuring the bot does not clutter the channel.

| Command | Description |
| --- | --- |
| `/eac setup channel:<#channel>` | Choose where updates are posted. |
| `/eac add game:<name> product_id:<id> deployment_id:<id> platforms:<list>` | Track a game. `platforms` defaults to `win64` and accepts a comma separated list. |
| `/eac remove game:<name>` | Stop tracking a game. |
| `/eac list` | Games and platforms being tracked. |
| `/eac config` | This server's current configuration. |
| `/eac status` | Last seen hash, size and time per target. |
| `/eac check game:<name> [platform]` | Fetch now and report current state, changed or not. |
| `/eac set option:<option> value:<value>` | Change `announce_on_first_seen`, `attach_raw_response` or `poll_interval_secs`. |

Game names autocomplete. `poll_interval_secs` applies to the whole bot rather
than one server, has a 30-second floor, and takes effect without a restart.

Values passed to `/eac add` are validated before they are stored: product and
deployment ids must be alphanumeric, because they are interpolated straight
into a CDN URL.

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
| `src/analysis.rs` | Hashes, entropy, TLSH, PE and Authenticode parsing. |
| `src/archive.rs` | Content-addressed payload store and JSONL index. |
| `src/diff.rs` | What changed between two payloads. |
| `src/state.rs` | Persisted last-seen digests. |
| `src/tracker.rs` | Poll loop, change detection, posting. |
| `src/embed.rs` | Embed rendering and attachment chunking. |
| `src/bot.rs` | Gateway wiring and `/eac`. |
| `src/settings.rs` | Per-server configuration set from Discord. |
| `deploy/` | systemd unit and environment file template. |

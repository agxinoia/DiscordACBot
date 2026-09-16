# EAC Update Tracker

A Discord bot that watches Easy Anti-Cheat module endpoints on the Epic Games
CDN and posts an embed whenever a game's modules change.

```
EAC Update Detected for ARC Raiders
Game            Platform          Download
ARC Raiders     wow64_win64       22.0 MB

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

### The payload format

The response is a **binary container**, not a single image. The modules sit at
offsets inside it, and one response carries several architectures at once —
which is why a platform segment is a composite like `wow64_win64` rather than
plain `win64`.

The bot carves the embedded PE images out of the container and reports each
one's architecture, size, hash and build timestamp. That is where the
per-module breakdown in an update embed comes from.

Nothing *depends* on the format, though. Change detection is the SHA-256 of the
raw body, which is correct whatever the container turns out to be, and module
extraction degrades in stages: a JSON manifest if there is one, then carved PE
images, then a plain scan for embedded filenames. If Epic changes the container
and all three come up empty, updates are still detected and reported correctly —
just without the breakdown. `src/analysis.rs` is where that would be fixed.

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
        platforms:wow64_win64
```

That is the whole setup. Settings are stored per server in `settings.json`, so
one bot can serve several servers without their configuration colliding.

`/eac` registers globally by default, which can take up to an hour to appear.
For instant availability while setting up, set `DISCORD_GUILD_ID` to your
server's id and the command registers to that server only.

There is no config file in the normal case. `config.toml` is optional and only
changes operator-level defaults — paths, timeouts, and fallbacks for servers
that have not configured themselves. See `config.example.toml`.

### Running the command-line tools

`cargo build --release` leaves the binary at `./target/release/eac-tracker`; it
is not on your `PATH`. Either call it by that path, or install it once:

```sh
cargo install --path .        # puts eac-tracker on PATH via ~/.cargo/bin
```

The examples below write `eac-tracker` for brevity. Without installing, prefix
them with `./target/release/`.

### Finding product and deployment ids

Both ids come from the game's own EasyAntiCheat configuration, shipped inside
the install directory. The bot can find them for you:

```sh
eac-tracker discover                      # searches the usual Steam libraries
eac-tracker discover /mnt/games --probe   # any readable path
```

`discover` walks the directory for small config files that mention
`productid` and `deploymentid`, handling both the JSON and INI shapes, and
prints a ready-to-paste command per game:

```
ARC Raiders
  product_id:    9e8b37541e614575b4de303d2c2e44cf
  deployment_id: 35e06571d8ab4de4b98519b624125459
  source:        .../ARC Raiders/EasyAntiCheat_EOS/Settings.json

  /eac add game:ARC Raiders product_id:9e8b... deployment_id:35e0... platforms:wow64_win64
```

`--probe` additionally asks the CDN which platforms each deployment actually
publishes, which is the only reliable way to learn the platform string. Without
it, `wow64_win64` is assumed. `--json` emits machine-readable output, and
`--depth N` bounds the search.

It runs wherever the files are readable: a native Linux Steam install, a Proton
prefix, or a Windows drive mounted on the server. Games that do not use EAC have
no such config, and a few launchers keep it outside the install directory.

Doing it by hand is a search for the key names rather than a specific file,
since Epic has moved and renamed these:

```sh
grep -ri --include='*.json' --include='*.ini' -e deploymentid -e productid \
  ~/.steam/steam/steamapps/common/<Game>/
```

### Checking ids you found elsewhere

For ids from a wiki, a repo or a forum post, check them before trusting them —
deployments get rotated, and a stale pair looks identical to a good one until it
silently never updates:

```sh
eac-tracker probe 9e8b37541e614575b4de303d2c2e44cf 35e06571d8ab4de4b98519b624125459
```

```
  wow64_win64      live    22.0 MB
  winarm_x64_x64   live    18.4 MB
  mac64            not published (HTTP 404)

/eac add game:Game Name product_id:9e8b... deployment_id:35e0... platforms:wow64_win64, winarm_x64_x64
```

With no platform argument every candidate is tried. It exits non-zero when
nothing is published, so it scripts cleanly, and `--json` gives machine-readable
output. Probing is a HEAD request, so it costs nothing like a download.

You do not need to do this before `/eac add`, which probes the ids itself and
refuses ones that publish nothing.

### Platform strings

A deployment publishes only some platforms, and which ones varies more than you
would expect. Measured against the live CDN, with the size each returns:

| Platform | Apex Legends | ARC Raiders | Fortnite | Rust |
| --- | --- | --- | --- | --- |
| `win64` | 32.4 MB | 21.9 MB | 32.8 MB | 32.7 MB |
| `winarm_x64_x64` | 22.5 MB | 17.1 MB | 22.8 MB | 22.9 MB |
| `mac64` | — | *0 B* | 9.2 MB | 9.5 MB |
| `linux32_64` | 8.2 MB | 10.2 MB | *0 B* | — |
| `wow64_win64` | — | *13.8 KB* | — | — |
| `win32` | — | *13.8 KB* | — | — |
| `wow64` | — | *13.8 KB* | — | — |
| `wine64` | — | — | *0 B* | — |
| `wine32` | — | — | *0 B* | — |

Real modules run 8–33 MB. The italic entries are the reason a 2xx is not taken
as proof on its own:

- **0 B** — the CDN answers `200 Content-Length: 0` for platforms a deployment
  does not publish. These are rejected: tracking one would watch a target that
  can never meaningfully change.
- **13.8 KB, identical across three legacy platform names** — a stub rather
  than a module. Small enough to be obvious once the size is shown, and
  invisible if you only look at the status. Detection skips these — adding
  three targets that all return the same tiny body is noise — but naming one
  explicitly in `platforms:` still tracks it, flagged. A deployment that
  publishes nothing but stubs falls back to them rather than being refused.

Both naming shapes are real. The bare names are OS types; the composites name a
*combination* of targets, because one response bundles several architectures.
There is no way to predict the set from the ids, which is why `/eac add` probes
every candidate and keeps whatever genuinely publishes something.

### The built-in list

A small catalogue of known deployments ships with the bot, so common games need
no ids at all:

```
/eac browse
```

gives a pick list — tick any number and they are added together. Each is probed
first, and anything that publishes nothing is skipped rather than stored, so the
list being stale costs you nothing. Games already tracked are left out of the
offer, matched by id rather than name so a locally renamed game is not offered
twice.

`/eac add game:Rust` works too: when a name matches the catalogue, the ids are
filled in for you.

| Game | product_id | deployment_id |
| --- | --- | --- |
| ARC Raiders | `9e8b37541e614575b4de303d2c2e44cf` | `35e06571d8ab4de4b98519b624125459` |
| Rust | `429c2212ad284866aee071454c2125b5` | `76796531e86443548754600511f42e9e` |
| Apex Legends | `5dcd88f4e2094a698ebffa43438edc33` | `47a5a1b2e0f64748a96777920ad97fbd` |
| Fortnite | `prod-fn` | `62a9473a2dca46b29ccf17577fcf42d7` |

These come from published research and are **not verified against the CDN by
this project**. Check them from the command line, where the CDN is reachable:

```sh
eac-tracker catalog --probe
```

Deployments get rotated, so entries going stale is expected. Add one to
`src/catalog.rs` to extend the list.

Note Fortnite's product id is not a hex string — ids are opaque, so anything
that is not a path separator is accepted.

### Interactive in-Discord Dashboard & Preset Insights

Run `/eac dashboard` in your server to launch the interactive control panel:

- **Channel Configuration**: Select the alert channel directly using Discord's channel dropdown.
- **Tracked Games Management**: Inspect configured platforms, trigger immediate module checks, or remove games.
- **Instant Settings Toggles**: Click buttons to toggle `announce_on_first_seen` (ON/OFF), `attach_raw_response` (ON/OFF), or cycle `poll_interval_secs` (30s, 60s, 120s, 300s, 600s).
- **Known Game Presets & Insights**: Select any preset (Apex Legends, ARC Raiders, Fortnite, Rust) to view architectural insights, CDN endpoints, and module notes.
- **Real-Time Live Probes**: Test candidate platforms on the live Epic Games CDN to view published module sizes and stub detection before tracking.
- **1-Click Tracking**: Track any preset or click **🚀 Track All Presets** to auto-detect platforms and begin monitoring immediately.

### Two EAC backends

This tracker follows the **EOS-based** EAC, distributed from
`modules-cdn.eac-prod.on.epicgames.com`. Games on the **legacy** backend do not
publish there and cannot be tracked by this bot, however correct their ids are.

The install layout is the tell: an `EasyAntiCheat_EOS/` folder means EOS, while
a bare `EasyAntiCheat/` folder is usually legacy. `probe` gives the definitive
answer — a pair that reports "not published" on every platform is either wrong
or not on this CDN.

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
| `/eac dashboard` | Open the interactive control panel to configure the announce channel, toggle tracker settings, inspect preset insights for known games, and track/manage games with buttons and select menus. |
| `/eac setup channel:<#channel>` | Choose where updates are posted. |
| `/eac browse` | Pick from the built-in list of known games. |
| `/eac add game:<name> [product_id:<id> deployment_id:<id>] [platforms:<list>]` | Track a game. Ids may be omitted for a game in the built-in list. Each platform is probed first; ones that publish nothing are rejected, not stored. Omit `platforms` to detect them automatically. |
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
| `src/discover.rs` | Scans installed games for EAC ids. |
| `src/catalog.rs` | Built-in list of known deployments. |
| `deploy/` | systemd unit and environment file template. |

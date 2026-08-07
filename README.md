# geotop

> htop-style real-time network & log monitor with a live global
> geolocation map.

`geotop` watches your network interface, your nginx/apache access log,
or both – and renders every incoming connection onto a world map.
Run it in your terminal with `ratatui` + `ratatui-image`
(Kitty Graphics Protocol → Sixel → half-block fallback), or open a
native GUI window with `--gui`.

https://crates.io/crates/geotop

![geotop GUI dashboard](https://raw.githubusercontent.com/ozkanpakdil/geotop/main/assets/geotop-gui.jpg)

> Native GUI mode (`--gui`) showing the live world map, top-talkers bar
> chart, throughput sparkline and the live connection log.

---

## Features

- **Dual ingestion engine**: sniff raw packets with `pnet` *and* tail
  nginx/apache logs at the same time. Both streams feed one unified
  event bus.
- **Auto-resolving geolocation**: bundled with an IP2Location
  LITE-DB11 wrapper (`ip2location 0.6`, auto-detects Location vs
  Proxy from the `.BIN` header byte, mmap-backed). On first run
  `geotop` downloads & extracts the DB for you via pure-Rust
  streaming – no `unzip` / `curl` shell-outs.
- **Lock-free hot-reload**: an `arc_swap::ArcSwapOption<DB>` is
  swapped atomically when the on-disk `.BIN` changes; lookups never
  block on a Mutex. `geotop` polls mtimes every 60s while it runs
  so external DB updates are picked up without restarting.
- **Proxy / VPN / datacenter / Tor detection** via IP2PROXY-LITE-PX11
  (optional, auto-downloaded).
- **High-resolution dynamic map** rendered through `ratatui-image 11`,
  auto-detecting the highest protocol your terminal supports:
  1. **Kitty Graphics Protocol** – crisp, per-pixel.
  2. **Sixel**.
  3. **Half-block / Unicode Braille** – universal fallback.
- **Three panels, htop-style**: map (top), live connection log
  (bottom), top talkers + throughput sparkline + proxy breakdown
  (right).
- **Glowing connection dots** that pulse and fade, with a "home
  location" indicator showing where *you* sit.
- **Auto-detected home marker**: on startup geotop looks up your public
  IP and places a persistent, larger marker at that lat/lon. Override
  with `--home lat,lon` or `map.home.lat` / `map.home.lon` in config.
- **Matrix-style connection lines** from home to every active marker,
  toggled with `l` (TUI) or from the top bar (GUI). Each line is a thin
  parabolic arc that animates, drawing out from home toward the node.
- **Native GUI** with `--gui`: zoomable/pannable map, scrollable live
  log, top-talkers bar chart, throughput sparkline. Mouse wheel zooms,
  drag pans, hover over a marker to see IP/city/proxy details,
  city/country labels scale with zoom.
- **Text-only mode** with `--no-map` for a compact htop-style
  dashboard when your terminal cannot display images or you only care
  about the numbers. Works in both the terminal (`--no-map`) and the
  native GUI (`--gui --no-map`).
- **Pause / clear / focus / quit** bound to single keys.

---

## Screenshots

### Terminal UI (`--gui` omitted)

![geotop TUI dashboard](https://raw.githubusercontent.com/ozkanpakdil/geotop/main/assets/geotop-tui.png)

The terminal dashboard renders the world map with half-block fallback
so it works in any terminal. The right-hand panel shows top talkers,
proxy/datacenter/Tor share and a throughput sparkline; the bottom
panel is the live connection log.

### CLI help

![geotop --help](https://raw.githubusercontent.com/ozkanpakdil/geotop/main/assets/geotop-help.png)

---

## Installation

| Platform | How |
|---|---|
| **macOS** (recommended) | `brew tap ozkanpakdil/geotop https://github.com/ozkanpakdil/geotop` then `brew install --cask geotop` — prebuilt, Apple-signed & notarized universal2 binary, no Gatekeeper prompt |
| macOS (direct download) | Grab `geotop-darwin-universal.tar.gz` from the [latest release](https://github.com/ozkanpakdil/geotop/releases), extract, then strip quarantine (see below) |
| Linux | Grab `geotop-x86_64-unknown-linux-gnu.tar.gz` from the [latest release](https://github.com/ozkanpakdil/geotop/releases), extract, put `geotop` on your `PATH` |
| Windows | Grab `geotop-x86_64-pc-windows-msvc.zip` from the [latest release](https://github.com/ozkanpakdil/geotop/releases), extract, put `geotop.exe` on your `PATH` |
| From source (any platform) | `cargo install geotop` — needs the Rust toolchain; compiles locally so it is never quarantined |

> **Why `cargo install`?** It compiles the binary on your own machine, so
> macOS never stamps it with a quarantine flag and Gatekeeper never blocks
> it. It needs the Rust toolchain, though — which is why the prebuilt
> binaries and Homebrew cask exist for non-developer users.

### macOS: "cannot be opened because it is from an unidentified developer"

The prebuilt macOS binary is **Apple-signed and notarized**, so a clean
`brew tap ozkanpakdil/geotop https://github.com/ozkanpakdil/geotop && brew install --cask geotop` runs with no warning. If you
downloaded the tarball straight from a browser instead, macOS attaches a
quarantine flag. Strip it once after extracting and the binary runs:

```bash
tar -xzf geotop-darwin-universal.tar.gz
xattr -dr com.apple.quarantine geotop
./geotop --gui
```

If you grabbed a per-arch tarball (`geotop-aarch64-apple-darwin.tar.gz` or
`geotop-x86_64-apple-darwin.tar.gz`) that predates the notarized universal
build, the same `xattr` one-liner unblocks it.

---

## Quick start

```bash
# 1. install
git clone https://github.com/ozkanpakdil/geotop
cd geotop
cargo install --path .

# 2. get a free IP2Location LITE token (only needed once)
#    see "IP2Location token" below, then either:
#    export GEOTOP_DOWNLOAD_TOKEN=<your-token>
#    or pass --download-token <your-token>
#    NOTE: sudo strips env vars, so under sudo use `sudo -E`, the inline
#    `sudo GEOTOP_DOWNLOAD_TOKEN=… geotop …` form, or --download-token.
#    Details in "The token under sudo" below.

# sniff all network interfaces
sudo -E geotop --all-interfaces

# tail a web log (no privileges needed)
geotop -f /var/log/nginx/access.log

# sniff a network interface (needs CAP_NET_RAW / sudo)
sudo -E geotop -i eth0

# both at once
sudo geotop -i en0 -f /var/log/nginx/access.log

# list interfaces available to the sniffer
geotop list-ifaces

# pre-download the IP2Location databases and exit
geotop update-dbs
```

On first run `geotop` creates `~/.local/share/geotop/data/` (via the
`directories` crate — XDG platform equivalent on Linux, AppData on
Windows, `~/Library/Application Support/geotop` on macOS) and
downloads:

- `IP2LOCATION-LITE-DB11.BIN` (geo lookup)
- `IP2PROXY-LITE-PX11.BIN`     (proxy / VPN / Tor / hosting flags)

Default download URLs (`src/db_downloader.rs`):

| DB    | URL                                                                          |
|-------|------------------------------------------------------------------------------|
| Geo   | `https://www.ip2location.com/download?file=DB11LITEBIN[&token=…]`            |
| Proxy | `https://www.ip2location.com/download?file=PX11LITEBIN[&token=…]`            |

DB management is implemented in [`src/db_downloader.rs`](src/db_downloader.rs)

---

## IP2Location token

The free IP2Location LITE databases require an authenticated
download token. `geotop` cannot silently download them without one.

### What happens if you forget the token

If you run `geotop` without a token and without pre-staged `.BIN`
files, the app:

1. Prints a prompt telling you a token is required.
2. Opens the IP2Location LITE signup/download page in your default
   browser.
3. Exits with instructions on how to provide the token.

```bash
$ geotop -f /var/log/nginx/access.log
╔════════════════════════════════════════════════════════════════════╗
║  IP2Location download token required                               ║
╠════════════════════════════════════════════════════════════════════╣
║  geotop needs a free IP2Location LITE token to download the        ║
║  geolocation database. Opening the signup page in your browser…  ║
╚════════════════════════════════════════════════════════════════════╝
Error: no GEOTOP_DOWNLOAD_TOKEN set.

1. Sign up for a free token at https://www.ip2location.com/free/download?file=DB11LITEBIN
2. Export it in your shell: export GEOTOP_DOWNLOAD_TOKEN=<your-token>
3. Or stage the .BIN files manually with --db-path / --proxy-db-path
4. Or provide the token on the command line with --download-token <your-token>
5. Re-run geotop
```

### How to provide the token

Choose whichever is most convenient for your workflow:

| Method | Example |
|--------|---------|
| Shell environment variable | `export GEOTOP_DOWNLOAD_TOKEN=<your-token>` |
| Command-line flag | `geotop --download-token <your-token> -f access.log` |
| Pre-stage the DBs | `geotop --db-path /path/to/IP2LOCATION-LITE-DB11.BIN --proxy-db-path /path/to/IP2PROXY-LITE-PX11.BIN -f access.log` |

The `--download-token` flag and `GEOTOP_DOWNLOAD_TOKEN` environment
variable are accepted by the normal run modes **and** by the
`update-dbs` subcommand.

### The token under `sudo` (raw-capture modes)

Raw packet capture (`--all-interfaces`, `-i`) needs root, so you run
`geotop` under `sudo`. **`sudo` strips environment variables by default**,
so a plain `export GEOTOP_DOWNLOAD_TOKEN=…` will *not* reach geotop and
you'll get `Error: no GEOTOP_DOWNLOAD_TOKEN set.` Use one of these instead:

```bash
# 1) -E preserves your exported environment through sudo
sudo -E geotop --all-interfaces

# 2) pass the var inline (works regardless of sudo's env policy)
sudo GEOTOP_DOWNLOAD_TOKEN=<your-token> geotop --all-interfaces

# 3) pass the flag directly (no env var needed at all)
sudo geotop --all-interfaces --download-token <your-token>
```

> **Database location under `sudo`:** `geotop` keeps its databases in
> `~/.geotop/`. Under `sudo`, `$HOME` is **root's home** (`/var/root` on
> macOS, `/root` on Linux), so the DBs are read from / written to
> `/var/root/.geotop`, *not* your user's `~/.geotop`. If you already
> downloaded the DBs as your normal user, point geotop at that directory
> so it doesn't re-download ~1.7 GB:
>
> ```bash
> sudo geotop --all-interfaces --db-dir "$HOME/.geotop"   # $HOME is expanded by YOUR shell before sudo runs
> ```
>
> The simplest workflow: download the DBs once as your normal user with
> `geotop update-dbs`, then run the capture under `sudo` with
> `--db-dir /home/<you>/.geotop` (or `/Users/<you>/.geotop` on macOS).

---

## Environment variables

Example `export GEOTOP_DOWNLOAD_TOKEN=<your-token>`

| Name                       | Effect                                                                 |
|----------------------------|------------------------------------------------------------------------|
| `GEOTOP_DOWNLOAD_TOKEN`    | IP2Location `&token=…` query value used when downloading LITE DBs. The same convention as GeoSentinel-Ingress's `GEOSENTINEL_DOWNLOAD_TOKEN`. |
| `RUST_LOG` / `-v`, `-vv`   | Standard tracing filter. `-v` raises the default to `debug`, `-vv` to `trace`. |

If your environment can't reach the IP2Location CDN (corporate
firewall, air-gapped machine, …) download the BINs manually and pass:

```bash
geotop --db-path /path/to/IP2LOCATION-LITE-DB11.BIN \
       --proxy-db-path /path/to/IP2PROXY-LITE-PX11.BIN \
       -f /var/log/nginx/access.log
```

---

## CLI

CLI flags and subcommands are defined in [`src/main.rs`](src/main.rs#L54) via `clap`'s derive macros.  Run:

```bash
geotop --help
geotop <COMMAND> --help
```

for the canonical, always-up-to-date reference.  Key groups:
- ingestion: `-i/--interface`, `-f/--file`, `--all-interfaces`
- databases: `--db-dir`, `--db-path`, `--proxy-db-path`, `--no-proxy`, `--download-token`
- display: `--gui`, `--no-map`, `--home`
- config / logging: `-C/--config`, `-v/--verbose`

### Keyboard controls

#### TUI (terminal)

| Key              | Action                                                   |
|------------------|----------------------------------------------------------|
| `Tab`            | Cycle focus between Map → Log → Metrics                  |
| `1` / `2` / `3`  | Jump straight to one of the three panels                 |
| `p`              | Pause ingestion (map freezes, counters keep counting)    |
| `c`              | Clear all active dots                                    |
| `l`              | Toggle Matrix-style connection lines (home → markers)    |
| `↑` / `↓`        | Scroll the live log                                      |
| `q` / `Esc`      | Quit                                                     |

#### GUI (`--gui`)

| Input            | Action                                                   |
|------------------|----------------------------------------------------------|
| `p`              | Pause ingestion                                          |
| `c`              | Clear all active dots                                    |
| `l`              | Toggle Matrix-style connection lines                     |
| `+` / `=`        | Zoom in on the map                                       |
| `-`              | Zoom out on the map                                      |
| `0`              | Reset zoom/pan to the full-world view                    |
| `q` / `Esc`      | Quit                                                     |
| Mouse wheel      | Zoom in/out at the cursor position                       |
| Drag             | Pan the zoomed map                                       |
| Hover marker     | Tooltip with IP, country, city and proxy type            |
| Hover home pulse | Tooltip with public IP, detected city and coordinates    |

---

## Configuration

`geotop` reads a JSON config file from `~/.geotop/config.json` (the same
directory as the IP2Location databases).  Use `-C, --config <PATH>` to point
at a different file.  All fields are optional and fall back to the same
defaults used before configuration existed.

A documented example ships at [`assets/config.example.json`](https://github.com/ozkanpakdil/geotop/blob/main/assets/config.example.json):

```bash
cp assets/config.example.json ~/.geotop/config.json
# edit to taste
```

### What you can configure

| Group | Fields | Effect |
|-------|--------|--------|
| Top-level | `marker_ttl_seconds` | How long a packet/log marker stays on the map (1–3600 s). |
| Top-level | `max_markers` | Maximum number of live markers retained. |
| Top-level | `marker_style` | Marker shape: `dot`, `ring`, `cross`, or `x`. Default `ring`. |
| Top-level | `marker_size` | Marker radius / arm length in pixels (1–20). Default `8`. |
| Top-level | `download_token` | IP2Location LITE download token, persisted here automatically the first time you supply it via `--download-token` or `GEOTOP_DOWNLOAD_TOKEN`, so you don't have to re-pass it on every run (especially under `sudo`, which strips env vars). Editable by hand. |
| Top-level | `gui_max_fps` | Cap on the GUI's idle-animation frame rate. The map animation (arc growth, dot fade, home pulse) is driven at this rate; user interaction (zoom/pan/hover) still repaints at full speed. Lowering it reduces CPU/GPU use and the per-frame texture upload. `0` = uncapped (vsync). Default `60`. Hot-reloadable. |
| `map.home` | `marker_style`, `marker_size` | Home marker shape/size. Default size `14`. |
| `map.labels` | `show_country_labels` | Show country names on the map (GUI). |
| `map.labels` | `show_city_labels` | Show city names next to markers when zoomed in (GUI). |
| `map.labels` | `city_label_zoom` | Minimum GUI zoom level before city labels appear. |
| `connection_lines` | `enabled` | Start with Matrix-style lines on/off. |
| `connection_lines` | `color`, `glow_size` | Line color and glow radius. |
| `colors` | `info`, `warn`, `alert`, `focus`, `dim`, `home`, `ocean`, `land` | Hex colors (`#RRGGBB` or `#RRGGBBAA`) used in both TUI and GUI. |
| `fonts` | `tui_font_width`, `tui_font_height` | Override the terminal font size `ratatui-image` uses in TUI mode. |
| `fonts` | `gui_body`, `gui_heading` | Base text sizes in GUI mode. |
| `fonts` | `gui_font_file` | Path to a custom `.ttf`/`.otf` font for GUI mode. |
| `window` | `width`, `height`, `min_width`, `min_height` | Native GUI window geometry. |

**Note on `map.home.lat/lon`:** If you do not set them and do not pass `--home`, geotop auto-detects your public IP at startup and geolocates it. The config values are used only as a fallback if detection fails.

### Hot-reload

The config file is watched while `geotop` runs.  Changes to **colors**, marker
TTL, `max_markers`, `marker_style`, `marker_size`, `home`, `connection_lines`,
`map.labels`, GUI fonts, and `window` size apply immediately.  A log line tells
you what changed.

---

## Marker colors and severity

Connection dots change color based on how suspicious the traffic looks:

| Color | Meaning |
|-------|---------|
| Green (`info`) | Normal traffic. |
| Yellow (`warn`) | A single source IP has generated ≥ 30 events within the current session. |
| Red (`alert`) | Proxy / VPN / datacenter / Tor traffic, or an HTTP 4xx/5xx status from a log event. |

Private, loopback, link-local and unspecified addresses are never plotted
on the map, but they still appear in the live log.

---

## Architecture

```
                     ┌───────────────────────┐
                     │    IP2Location LITE    │
                     │    IP2PROXY LITE       │   on disk under
                     │    *.BIN (mmap)        │◄── ~/.local/share/geotop/data
                     └───────────┬───────────┘
                                 │ DB::from_file (ip2location 0.6)
                                 ▼
                     ┌───────────────────────┐
                     │  DatabaseManager       │
                     │  ArcSwapOption<DB>     │  lock-free hot-swap
                     │  max_age + mtime poll  │
                     └───────────┬───────────┘
                                 │ .geo() / .proxy()
                                 ▼
┌─────────────────┐    mpsc::UnboundedSender<ConnectionEvent>
│ pcap_sniffer.rs │───┐        ┌──────────────────┐
└─────────────────┘   ├────────►│   AppState       │
                      │        │  parking_lot +   │
┌─────────────────┐   │        │   dashmap        │
│  log_tailer.rs  │───┘        │  dots / log /    │
└─────────────────┘            │  counters /      │
                               │  throughput      │
                               └────────┬─────────┘
                                        │ tick() + ingest()
                                        ▼
              ┌───────────────────────────────────────────────┐
              │  main loop  (Crossterm backend, 100 ms tick)  │
              │                                               │
              │  dashboard() → render Map / Log / Metrics     │
              │      └── ratatui-image 11 (Kitty/Sixel/HB)    │
              └───────────────────────────────────────────────┘
```

**Threading model**

| Thread / task                | Owned by                              |
|------------------------------|---------------------------------------|
| UI render loop               | Tokio multi-thread runtime            |
| `pcap_sniffer`               | Dedicated `std::thread` (pnet is blocking) |
| `log_tailer`                 | Dedicated `std::thread` (notify is blocking) |
| `DatabaseManager::hot_reload`| Tokio `interval` task, every 60 s     |
| IP2Location lookups          | Synchronous on the UI tick (LRU hot path) |

Ingestion workers never hold locks into the UI thread; all
communication is via `mpsc::UnboundedSender<ConnectionEvent>` and a
short-held Mutex/DashMap inside `AppState`. The DB lookup path is
lock-free (`arc_swap::load_full`).

---

## Database licensing & attribution

The IP2Location LITE DB11 and IP2PROXY LITE PX11 are free for
non-commercial use under the [CC-BY-SA 4.0][ccbysa] license. By
running `geotop` you agree to those terms and to attribute
IP2Location. Commercial deployments require a paid license from
<https://www.ip2location.com>.

[ccbysa]: https://creativecommons.org/licenses/by-sa/4.0/

`--no-proxy` skips the IP2PROXY download and disables proxy / VPN /
Tor / hosting classification (useful in air-gapped environments
without access to the IP2PROXY CDN).

---

## Building from source

```bash
# standard build
cargo build --release

# strip + optimise
cargo build --release --locked

# try the bundled sample log (no network, no privileges, DBs required
# unless you point --db-path at an existing BIN)
./target/release/geotop -f ./samples/example.log
```

Requirements:

- **Rust 1.74+** (uses `let … else` chains, `AtomicU64`, and
  `Default`-clap derive).
- **libpcap headers** (Linux) or the equivalent on macOS/Windows for
  `pnet`'s raw datalink channel. Not needed if you only use `-f`.
- The IP2LOCATION-LITE-DB11.BIN file (auto-downloaded with a token, or pre-staged
  with `--db-path` / `--download-token`).

---

## Releasing

Releases are cut with [`cargo-release`](https://crates.io/crates/cargo-release),
which bumps the version in `Cargo.toml`/`Cargo.lock`, commits, and tags in one
step — far less error-prone than hand-editing the manifest and `git tag`. The
behaviour is pinned in [`release.toml`](https://github.com/ozkanpakdil/geotop/blob/main/release.toml): tags use a `v` prefix
(so `0.1.1` becomes tag `v0.1.1`, which triggers the GA release) and
`cargo publish` is disabled (the GitHub Actions `publish-crate` job handles
crates.io instead).

```bash
# one-time install
cargo install cargo-release

# bump 0.1.0 -> 0.1.1, update Cargo.lock, commit, and create the v0.1.1 tag
cargo release 0.1.1 --execute

# push the version commit + tag (this triggers the GitHub Actions release)
git push origin main
git push origin v0.1.1
```

`cargo release 0.1.1` defaults to a dry run; `--execute` makes it actually
commit and tag. It will refuse if `0.1.1` is not greater than the current
`Cargo.toml` version.

What the GitHub Actions release (`.github/workflows/release.yml`) does on a
pushed `v*` tag:

- Builds stripped release binaries for Linux (`x86_64`), macOS (`x86_64` and
  `aarch64`), and Windows (`x86_64`).
- Verifies the bundled world-map GeoJSON is embedded in each binary.
- Creates a GitHub Release with auto-generated notes and the binaries attached.
- Publishes the crate to crates.io (requires the `CARGO_REGISTRY_TOKEN`
  repository secret).

### Build version string

A `build.rs` sets the `BUILD_VERSION` compile-time env var to `git describe
--tags --always --dirty` when available, falling back to the `Cargo.toml`
version otherwise. So:

- a **tagged** build reports `geotop v0.1.1`
- an **untagged dev** build reports something like `geotop be7a4ce-dirty`

`geotop --version` uses this string; `Cargo.toml`'s `version` remains the
authoritative crate version that `cargo-release` bumps.

---

## Project layout

```
src/
├── main.rs                # CLI parsing + Tokio runtime + main loop + DB wiring
├── db_downloader.rs       # DatabaseManager: download / extract / mmap / hot-reload
│                          #   (mirrors GeoSentinel-Ingress/src/db_manager.rs)
├── event.rs               # Shared ConnectionEvent / Source / Severity
├── home.rs                # Public-IP detection for the home marker
├── geo/
│   ├── mod.rs
│   └── lookup.rs          # GeoInfo + GeoLookup (LRU cache over DatabaseManager)
├── ingest/
│   ├── mod.rs
│   ├── log_tailer.rs      # notify-based log tailer w/ CLF regex
│   └── pcap_sniffer.rs    # pnet datalink listener
└── ui/
    ├── mod.rs
    ├── app.rs             # Shared state: dots, counters, throughput, focus
    ├── gui.rs             # Native egui/eframe window
    ├── layout.rs          # Grid layout for the dashboard
    ├── map_renderer.rs    # image::RgbaImage buffer + lat/lon projection
    └── panels.rs          # Map / log / metrics widget renderers
```

---

## Troubleshooting

### "permission denied" while opening datalink

`pnet` opens a raw (Linux) or BPF (macOS) socket. Either run with
`sudo`, add `cap_net_raw` to the binary (`setcap cap_net_raw=ep
target/release/geotop`), or use `-f` to switch to log mode (no
privileges needed).

### Map looks chunky / blocky

You're hitting the half-block fallback. Use a terminal with full
Kitty graphics support – recent versions of:

- Kitty
- WezTerm (`wezterm.gui.set_config{ front_end = "WebGpu" }`)
- iTerm2 (with `Terminal > Enable experimental image support`)
- Konsole (≥ 22.04)
- foot

…all advertise support for the Kitty Graphics Protocol.

### CPU pinned at 100 %

That's `pnet` in promiscuous mode. Add `-f /var/log/nginx/access.log`
and drop `-i` if you don't actually need raw packets; log-only mode
is essentially free.

### DBs won't download

Your firewall is blocking `www.ip2location.com`, or you have not set
a download token. You can:

- Sign up for a free token at <https://www.ip2location.com/free/download?file=DB11LITEBIN>
  and provide it via `GEOTOP_DOWNLOAD_TOKEN` or `--download-token`, or
- If you're running under `sudo` and see `Error: no GEOTOP_DOWNLOAD_TOKEN set.`,
  remember `sudo` strips env vars — see [The token under `sudo`](#the-token-under-sudo-raw-capture-modes).
- Download the BINs from another machine and pass `--db-path` /
  `--proxy-db-path` explicitly, or
- Run with `--no-proxy` if you don't care about the proxy / VPN /
  hosting classification.

### Stale `.BIN` after on-disk update

`geotop` polls the `data_dir` mtime every 60s and atomically
replaces the in-memory `Arc<DB>` when the file changes. If you've
copied a new BIN into place and want to force a reload faster than
that, restart `geotop` (`q` and re-launch).

---

## Roadmap

- [ ] Tofu-style country heatmap aggregate view

---

## License

Dual-licensed under MIT or Apache-2.0, at your option. The
auto-downloaded IP2Location data remains governed by its own
CC-BY-SA 4.0 terms – see **Database licensing & attribution** above.

# geotop

> htop-style real-time network & log monitor with a live global
> geolocation map.

`geotop` watches your network interface, your nginx/apache access log,
or both – and renders every incoming connection onto a world map in
your terminal. Built on `ratatui` + `ratatui-image` so it scales from
modern Kitty/WezTerm (Kitty Graphics Protocol) down to any TTY
(half-block glyph fallback).

![hero](assets/world-dark.png)

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
  (optional, auto-downloaded, same `&token=` auth convention as
  **[GeoSentinel-Ingress][geos]**).
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
- **Pause / clear / focus / quit** bound to single keys.

---

## Quick start

```bash
# 1. install
git clone https://github.com/yourname/geotop
cd geotop
cargo install --path .

# sniff all network interfaces
sudo geotop --all-interfaces

# tail a web log (no privileges needed)
geotop -f /var/log/nginx/access.log

# sniff a network interface (needs CAP_NET_RAW / sudo)
sudo geotop -i eth0

# both at once
sudo geotop -i en0 -f /var/log/nginx/access.log

# list interfaces available to the sniffer
geotop list-ifaces

# 6. pre-download the IP2Location databases and exit
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
and is structurally identical to the manager in
**[GeoSentinel-Ingress][geos]**: `DatabaseManager` with
`ensure_databases()` (download → extract via `zip` crate → open with
`DB::from_file` → `print_db_info()` self-check) and `hot_reload()`
(mtime polling, atomic `ArcSwapOption::store`).

[geos]: https://github.com/yourorg/GeoSentinel-Ingress

---

## Environment variables

Example `export GEOTOP_DOWNLOAD_TOKEN=twU8vNJ0rXsqy9BY9Z5FjXLvmJHe5o9zv5f8lEwpmDxDg8WNOiC5HdcEYtcuSeaA`

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

```
USAGE:
    geotop [OPTIONS] [-i IFACE] [-f PATH]...
    geotop <COMMAND>

OPTIONS:
    -i, --interface <IFACE>            Network interface to sniff (e.g. `eth0`,
                                       `wlan0`, `en0`). Use `geotop list-ifaces`
                                       to discover available interfaces
    -f, --file <PATH>                  Web server log file(s) to tail. Repeat
                                       the flag for multiple files
        --db-dir <DIR>                 Override the directory containing the
                                       IP2LOCATION / IP2PROXY DBs
        --db-path <PATH>               Skip the auto-downloader and use this
                                       exact `.BIN` file
        --proxy-db-path <PATH>         Skip the auto-downloader for IP2PROXY
                                       and use this exact `.BIN` file
        --map-path <PATH>              Path to a dark-mode equirectangular world
                                       map (PNG/JPEG)
        --no-map                       Disable the map panel (text-only dashboard)
        --home <LAT,LON>               Host coordinates for the pulsing "home"
                                       dot. Format: `lat,lon`
        --no-proxy                     Disable proxy / VPN / datacenter
                                       classification
    -v, --verbose...                   Verbose logging (`-v`, `-vv`, …)
    -h, --help                         Print help
    -V, --version                      Print version

COMMANDS:
    list-ifaces                        Print the available network interfaces
                                       and exit
    update-dbs                         Download + extract IP2Location + IP2Proxy
                                       LITE DBs into `--db-dir` and exit
    help                               Print this message
```

### Keyboard controls

| Key              | Action                                                   |
|------------------|----------------------------------------------------------|
| `Tab`            | Cycle focus between Map → Log → Metrics                  |
| `1` / `2` / `3`  | Jump straight to one of the three panels                 |
| `p`              | Pause ingestion (map freezes, counters keep counting)    |
| `c`              | Clear all active dots                                    |
| `↑` / `↓`        | Scroll the live log                                      |
| `q` / `Esc`      | Quit                                                     |

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

# try the bundled sample log (no network, no privileges, no DBs needed
# if you point --db-path at an existing BIN – see below)
./target/release/geotop -f ./samples/example.log
```

Requirements:

- **Rust 1.74+** (uses `let … else` chains, `AtomicU64`, and
  `Default`-clap derive).
- **libpcap headers** (Linux) or the equivalent on macOS/Windows for
  `pnet`'s raw datalink channel. Not needed if you only use `-f`.
- The IP2LOCATION-LITE-DB11.BIN file (auto-downloaded, or pre-staged
  with `--db-path`).
- A world map PNG – drop one into `./assets/world-dark.png` (any
  dark equirectangular projection, 2048×1024 recommended) or pass
  `--map-path /path/to/yours.png`. Without one and without
  `--map-path`, the dashboard still works in `--no-map` mode (or
  shows solid black if you have a TTY but no map file).

---

## Project layout

```
src/
├── main.rs                # CLI parsing + Tokio runtime + main loop + DB wiring
├── db_downloader.rs       # DatabaseManager: download / extract / mmap / hot-reload
│                          #   (mirrors GeoSentinel-Ingress/src/db_manager.rs)
├── event.rs               # Shared ConnectionEvent / Source / Severity
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

Your firewall is blocking `www.ip2location.com`. Either:

- Export `GEOTOP_DOWNLOAD_TOKEN=<your-token>` to use IP2Location's
  authenticated download endpoint, or
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

- [ ] Mouse-driven map zoom/pan
- [ ] Tofu-style country heatmap aggregate view

---

## License

Dual-licensed under MIT or Apache-2.0, at your option. The
auto-downloaded IP2Location data remains governed by its own
CC-BY-SA 4.0 terms – see **Database licensing & attribution** above.

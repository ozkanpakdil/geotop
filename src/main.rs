//! `geotop` — htop-style real-time network & log monitor with a live
//! global geolocation map.
//!
//! CLI is parsed with `clap`; the Tokio runtime drives ingestion +
//! the UI tick loop. All long-lived workers are spawned onto their
//! own threads (pnet and notify aren't async-friendly) and bridge
//! into the UI via an `mpsc::UnboundedSender<ConnectionEvent>`.

mod db_downloader;
mod event;
mod geo;
mod ingest;
mod ui;

use std::io::stdout;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use image::RgbaImage;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Margin;
use ratatui::Terminal;
use tokio::sync::mpsc;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crossterm::event::{self as cterm_event, Event, KeyCode, KeyEventKind};
use eframe::egui;
use ratatui_image::picker::Picker;
use ratatui_image::Resize;

use crate::db_downloader::{DbConfig, LOCATION_DB_NAME, PROXY_DB_NAME, DatabaseManager};
use crate::event::ConnectionEvent;
use crate::geo::lookup::GeoLookup;
use crate::ingest::pcap_sniffer;
use crate::ui::app::{AppState, Panel};
use crate::ui::gui::GuiApp;
use crate::ui::map_renderer::{HomeLocation, MapRenderer};
use crate::ui::panels;
use crate::ui::layout::dashboard;

/// geotop – htop-style real-time network & log monitor.
#[derive(Parser, Debug)]
#[command(name = "geotop", version, about, long_about = None)]
struct Cli {
    /// Subcommand.
    #[command(subcommand)]
    command: Option<Command>,

    /// Network interface(s) to sniff (e.g. `eth0`, `en0`).
    /// Repeat the flag for multiple interfaces, or use `--all-interfaces`.
    /// Use `geotop list-ifaces` to discover available interfaces.
    #[arg(short = 'i', long, value_name = "IFACE")]
    interface: Vec<String>,

    /// Sniff traffic from every non-loopback interface (requires root/BPF access).
    #[arg(long)]
    all_interfaces: bool,

    /// Web server log file(s) to tail. Repeat the flag for multiple files.
    #[arg(short = 'f', long = "file", value_name = "PATH")]
    files: Vec<PathBuf>,

    /// Override the directory containing the IP2LOCATION / IP2PROXY DBs.
    #[arg(long, value_name = "DIR")]
    db_dir: Option<PathBuf>,

    /// Skip the auto-downloader and use this exact `.BIN` file.
    #[arg(long, value_name = "PATH")]
    db_path: Option<PathBuf>,

    /// Skip the auto-downloader for IP2PROXY and use this exact `.BIN` file.
    #[arg(long, value_name = "PATH")]
    proxy_db_path: Option<PathBuf>,

    /// Path to a dark-mode equirectangular world map (PNG/JPEG/SVG).
    #[arg(long, value_name = "PATH")]
    map_path: Option<PathBuf>,

    /// Disable the world map (text-only dashboard).
    #[arg(long)]
    no_map: bool,

    /// Open a native GUI window instead of the terminal UI (Linux/macOS/Windows).
    #[arg(long)]
    gui: bool,

    /// Host coordinates for the "home" pulse. Format: `lat,lon`.
    #[arg(long, value_name = "LAT,LON")]
    home: Option<String>,

    /// Disable proxy / VPN / datacenter classification.
    #[arg(long)]
    no_proxy: bool,

    /// Verbose logging (`-v`, `-vv`, …).
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    verbose: u8,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List available network interfaces and exit.
    ListIfaces,
    /// Download the IP2Location + IP2Proxy LITE DBs and exit.
    UpdateDbs,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    init_logging(cli.verbose);

    // Subcommand short-circuits (no async needed).
    if let Some(Command::ListIfaces) = &cli.command {
        println!("Available interfaces:");
        for (iface, addrs) in pcap_sniffer::list_interfaces() {
            let addr_list: Vec<String> = addrs
                .iter()
                .map(|a| a.ip().to_string())
                .collect();
            let addr_str = if addr_list.is_empty() {
                String::new()
            } else {
                format!("  [{}]", addr_list.join(", "))
            };
            println!("  {iface}{addr_str}");
        }
        return Ok(());
    }

    // We need an async runtime for DB downloads / ingestion. Build it once and
    // use it for the async setup, then either run the GUI on the main thread or
    // the TUI inside the runtime.
    let rt = tokio::runtime::Runtime::new().context("creating tokio runtime")?;

    if let Some(Command::UpdateDbs) = &cli.command {
        return rt.block_on(async {
            let dir = default_db_dir(&cli)?;
            db_downloader::ensure_dbs(&dir).await.map(|_| ())
        });
    }

    if cli.interface.is_empty() && !cli.all_interfaces && cli.files.is_empty() {
        anyhow::bail!(
            "must supply at least one of `-i IFACE`, `--all-interfaces`, or `-f /path/to/access.log`.\nTry `geotop --help`."
        );
    }

    rt.block_on(async {
        // ---------- resolve DBs ------------------------------------------------
        let db_dir = default_db_dir(&cli)?;
        let token = std::env::var("GEOTOP_DOWNLOAD_TOKEN").unwrap_or_default();

        let mut db_cfg = DbConfig {
            data_dir: db_dir,
            token,
            proxy_enabled: !cli.no_proxy,
            ..DbConfig::default()
        };
        if let Some(p) = &cli.db_path {
            db_cfg.geo_db = p
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| LOCATION_DB_NAME.into());
            db_cfg.data_dir = p.parent().map(|d| d.to_path_buf()).unwrap_or(db_cfg.data_dir);
        }
        if let Some(p) = &cli.proxy_db_path {
            db_cfg.proxy_db = p
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| PROXY_DB_NAME.into());
        }
        if let Some(d) = &cli.db_dir {
            db_cfg.data_dir = d.clone();
        }

        let db_mgr = DatabaseManager::new(db_cfg);
        db_mgr
            .ensure_databases()
            .await
            .context("ensuring IP2Location/IP2Proxy databases")?;

        // Kick off a periodic hot-reload watcher so external `.BIN` updates are
        // picked up without restarting geotop (matches GeoSentinel).
        let mgr_for_reload = db_mgr.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                tick.tick().await;
                if let Err(e) = mgr_for_reload.hot_reload().await {
                    warn!(error = %e, "hot-reload tick failed");
                }
            }
        });

        let geo = GeoLookup::new(db_mgr.clone());

        // ---------- ui dispatch ------------------------------------------------
        let home = parse_home(&cli.home).unwrap_or(HomeLocation { lat: 39.0, lon: -77.0 }); // DC-ish default
        if cli.gui {
            return run_gui(cli, db_mgr, geo, home).await;
        }
        run_tui(cli, db_mgr, geo, home).await
    })
}

/// Setup ingestion workers and run the native GUI window on the main thread.
async fn run_gui(cli: Cli, db_mgr: Arc<DatabaseManager>, geo: Arc<GeoLookup>, home: HomeLocation) -> Result<()> {
    let (tx, rx) = mpsc::unbounded_channel::<ConnectionEvent>();
    let cancel_handles = spawn_ingest_workers(&cli, tx).await?;

    let renderer = if cli.no_map {
        MapRenderer::load(None)?
    } else {
        MapRenderer::load(cli.map_path.as_deref())?
    };
    let app = GuiApp::new(rx, geo, renderer, home);

    // eframe::run_native must run on the main thread (winit requirement).
    // The async runtime is dropped after this returns, which also drops the
    // ingestion worker tasks. Build options without the non-Send hooks.
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size(egui::vec2(1280.0, 720.0))
            .with_min_inner_size(egui::vec2(640.0, 360.0)),
        ..Default::default()
    };

    let result = eframe::run_native(
        "geotop",
        options,
        Box::new(|_cc| Ok(Box::new(app))),
    );
    let result = result.map_err(|e| anyhow::anyhow!("eframe error: {e}"))?;

    for (name, cancel) in cancel_handles {
        cancel.cancel();
        info!("stopped {name} worker");
    }
    info!("geotop GUI exiting cleanly");
    Ok(result)
}

/// Setup ingestion workers and run the terminal UI inside the async runtime.
async fn run_tui(
    cli: Cli,
    _db_mgr: Arc<DatabaseManager>,
    geo: Arc<GeoLookup>,
    home: HomeLocation,
) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<ConnectionEvent>();
    let handles = spawn_ingest_workers(&cli, tx).await?;

    let renderer = if !cli.no_map {
        Some(MapRenderer::load(cli.map_path.as_deref())?)
    } else {
        None
    };

    let state = Arc::new(AppState::new(geo.clone()));
    let mut terminal = setup_terminal()?;

    // Pick the highest graphics protocol the terminal supports.
    let picker = if crossterm::tty::IsTty::is_tty(&std::io::stdin()) {
        Picker::from_query_stdio().unwrap_or_else(|e| {
            warn!("terminal query failed ({e:?}), falling back to halfblocks");
            Picker::halfblocks()
        })
    } else {
        warn!("stdin is not a TTY, using halfblocks fallback");
        Picker::halfblocks()
    };
    info!("graphics protocol: {:?}", picker.protocol_type());
    info!("font size: {:?}", picker.font_size());

    let picker = if picker.font_size().width <= picker.font_size().height {
        warn!("reported font aspect looks non-square/tall, ignoring queried font size");
        Picker::halfblocks()
    } else {
        picker
    };

    let mut map_protocol: ratatui_image::protocol::Protocol = make_fallback_state();

    // ---------- main loop --------------------------------------------------
    let tick_rate = Duration::from_millis(100);
    let mut last = Instant::now();

    loop {
        // 1) drain the channel without blocking – bounded by render cadence.
        while let Ok(ev) = rx.try_recv() {
            state.ingest(ev);
        }

        // 2) app tick.
        state.tick();

        // 3) render.
        let map_img: Option<RgbaImage> = renderer.as_ref().map(|r| {
            let dots = state.dots.lock();
            let snapshot: Vec<_> = dots.iter().cloned().collect();
            drop(dots);
            r.redraw(&snapshot, home)
        });

        terminal.draw(|f| {
            let areas = dashboard(f.area());

            let dyn_img: Option<image::DynamicImage> =
                map_img.clone().map(image::DynamicImage::ImageRgba8);
            let inner = areas.map.inner(Margin::new(1, 1));
            let size = ratatui::layout::Size::new(inner.width.max(1), inner.height.max(1));
            let proto = dyn_img
                .as_ref()
                .and_then(|img| picker.new_protocol(img.clone(), size, Resize::Fit(None)).ok());
            if let Some(p) = proto {
                map_protocol = p;
            } else {
                warn!("failed to encode map for size {size:?}, reusing previous frame");
            }

            let display_img = map_img.clone().unwrap_or_else(blank_map_image);
            panels::render(f, &state, areas, display_img, &map_protocol, inner);
        })?;

        // 4) input.
        let elapsed = last.elapsed();
        let poll = tick_rate.saturating_sub(elapsed);
        if cterm_event::poll(poll).unwrap_or(false) {
            handle_event(state.clone(), &cterm_event::read()?)?;
            if state.quit_requested() {
                break;
            }
        }
        last = Instant::now();
    }

    // ---------- teardown ---------------------------------------------------
    for (name, cancel) in handles {
        cancel.cancel();
        info!("stopped {name} worker");
    }
    teardown_terminal(&mut terminal)?;
    info!("geotop exiting cleanly");
    Ok(())
}

/// Spawn packet sniffers and log tailers. Returns worker cancel tokens.
async fn spawn_ingest_workers(
    cli: &Cli,
    tx: mpsc::UnboundedSender<ConnectionEvent>,
) -> Result<Vec<(&'static str, tokio_util::sync::CancellationToken)>> {
    let mut handles = Vec::new();

    let sniff_ifaces: Vec<String> = if cli.all_interfaces {
        pcap_sniffer::list_interfaces()
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    } else {
        cli.interface.clone()
    };

    for iface in sniff_ifaces {
        let h = pcap_sniffer::spawn(iface.clone(), tx.clone())
            .with_context(|| format!("starting pcap sniffer on {iface}"))?;
        handles.push(("pcap", h.cancel));
    }
    for f in &cli.files {
        let h = ingest::log_tailer::spawn(f.clone(), tx.clone())
            .with_context(|| format!("starting log tailer on {}", f.display()))?;
        handles.push(("log", h.cancel));
    }

    Ok(handles)
}

fn default_db_dir(cli: &Cli) -> Result<PathBuf> {
    if let Some(p) = &cli.db_dir {
        return Ok(p.clone());
    }
    let base = directories::ProjectDirs::from("dev", "geotop", "geotop")
        .map(|p| p.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("./data"));
    Ok(base)
}

fn parse_home(s: &Option<String>) -> Option<HomeLocation> {
    let s = s.as_ref()?;
    let (lat, lon) = s.split_once(',')?;
    Some(HomeLocation {
        lat: lat.trim().parse().ok()?,
        lon: lon.trim().parse().ok()?,
    })
}

fn handle_event(state: std::sync::Arc<AppState>, ev: &Event) -> Result<()> {
    if let Event::Key(k) = ev {
        if k.kind != KeyEventKind::Press {
            return Ok(());
        }
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => state.quit(),
            KeyCode::Char('p') => state.toggle_pause(),
            KeyCode::Char('c') => state.request_clear(),
            KeyCode::Tab => state.cycle_focus(),
            KeyCode::Char('1') => state.set_focus(Panel::Map),
            KeyCode::Char('2') => state.set_focus(Panel::Log),
            KeyCode::Char('3') => state.set_focus(Panel::Metrics),
            _ => {}
        }
    }
    Ok(())
}

fn init_logging(verbosity: u8) {
    let level = match verbosity {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<std::io::Stdout>>> {
    let mut out = stdout();
    execute!(out, EnterAlternateScreen).context("entering alternate screen")?;
    enable_raw_mode().context("enabling raw mode (is stdin a TTY?)")?;
    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

fn teardown_terminal(t: &mut Terminal<CrosstermBackend<std::io::Stdout>>) -> Result<()> {
    disable_raw_mode().ok();
    execute!(t.backend_mut(), LeaveAlternateScreen).ok();
    t.show_cursor().ok();
    Ok(())
}

fn blank_map_image() -> RgbaImage {
    image::RgbaImage::from_pixel(640, 320, image::Rgba([16, 16, 16, 255]))
}

/// Build a halfblock `Protocol` from a fallback Picker — used when the
/// terminal didn't advertise any graphics protocol (no Kitty/Sixel).
fn make_fallback_state() -> ratatui_image::protocol::Protocol {
    let picker = Picker::halfblocks();
    let size = ratatui::layout::Size::new(80, 24);
    // For halfblocks the picker is infallible.
    picker
        .new_protocol(
            image::DynamicImage::ImageRgba8(blank_map_image()),
            size,
            Resize::Fit(None),
        )
        .expect("halfblock protocol never fails")
}

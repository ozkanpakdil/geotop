//! Central application state. Everything the render loop reads lives
//! here. Mutations happen on ingestion threads (via channels) and on
//! the UI tick; both paths acquire short-held Mutex-guards so we never
//! hold a lock across a render call.

use std::collections::VecDeque;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use parking_lot::{Mutex, RwLock};
use ratatui::widgets::ListState;
use serde::Serialize;

use crate::config::Config;
use crate::event::{is_plottable_ip, ConnectionEvent, Severity};
use crate::geo::lookup::{GeoInfo, GeoLookup};

/// Cap on scrollback lines in the live log panel.
const MAX_LOG_ROWS: usize = 2_000;

/// Threshold for the "Warn" severity (packets-per-minute per IP).
const WARN_THRESHOLD: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Map,
    Log,
    Metrics,
}

#[derive(Debug, Clone, Default)]
pub struct PerIpMetrics {
    pub hits: u64,
    pub last_seen: DateTime<Utc>,
}

/// One dot on the map. Lifetime is governed by `created_at`; the
/// renderer fades pixels proportionally to age.
#[derive(Debug, Clone)]
pub struct MapDot {
    pub lat: f64,
    pub lon: f64,
    #[allow(dead_code)]
    pub country: String,
    #[allow(dead_code)]
    pub city: String,
    pub severity: Severity,
    #[allow(dead_code)]
    pub src_ip: IpAddr,
    #[allow(dead_code)]
    pub proxy: Option<crate::geo::lookup::ProxyKind>,
    pub created_at: Instant,
}

/// Aggregate stats exposed to the metrics panel.
#[derive(Debug, Default, Clone, Serialize)]
pub struct Stats {
    pub total_events: u64,
    pub events_per_sec: f64,
    pub bytes_total: u64,
    pub proxy_pct: f64,
    pub datacenter_pct: f64,
    pub tor_pct: f64,
}

pub struct AppState {
    /// Live dots on the map.
    pub dots: Mutex<VecDeque<MapDot>>,
    /// Bounded scrollback for the live-log panel.
    pub log: Mutex<VecDeque<LogRow>>,
    /// Per-IP hit counters (for warn escalation).
    pub per_ip: DashMap<IpAddr, PerIpMetrics>,
    /// Per-country counters (for top talkers).
    pub per_country: DashMap<String, u64>,
    /// Aggregate stats (cheap to recompute on every tick).
    pub stats: Mutex<Stats>,
    /// Geo lookup handle.
    pub geo: Arc<GeoLookup>,
    /// Shared configuration; may be hot-reloaded.
    pub config: Arc<RwLock<Config>>,
    /// Selected panel (Tab cycles).
    pub focused: Mutex<Panel>,
    /// When true, ingestion continues but the map is frozen.
    pub paused: AtomicBool,
    /// When true, all dots are wiped on the next tick.
    pub clear_requested: AtomicBool,
    /// Toggle for Matrix-style connection lines from the home marker.
    pub connection_lines: AtomicBool,
    /// Rolling 1-second packet ring for the throughput sparkline.
    pub throughput_ring: Mutex<VecDeque<u64>>,
    /// Spinner / lifecycle flag – set to `true` to exit the main loop.
    pub should_quit: AtomicBool,
    /// Log-panel list state (scroll position).
    pub log_state: Mutex<ListState>,
}

impl AppState {
    pub fn new(geo: Arc<GeoLookup>, config: Arc<RwLock<Config>>) -> Self {
        let max_markers = config.read().max_markers;
        let lines_on = config.read().connection_lines.enabled;
        Self {
            dots: Mutex::new(VecDeque::with_capacity(max_markers)),
            log: Mutex::new(VecDeque::with_capacity(MAX_LOG_ROWS)),
            per_ip: DashMap::new(),
            per_country: DashMap::new(),
            stats: Mutex::new(Stats::default()),
            geo,
            config,
            focused: Mutex::new(Panel::Map),
            paused: AtomicBool::new(false),
            clear_requested: AtomicBool::new(false),
            connection_lines: AtomicBool::new(lines_on),
            throughput_ring: Mutex::new(VecDeque::with_capacity(120)),
            should_quit: AtomicBool::new(false),
            log_state: Mutex::new(ListState::default()),
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn toggle_pause(&self) {
        let prev = self.paused.load(Ordering::Relaxed);
        self.paused.store(!prev, Ordering::Relaxed);
    }

    pub fn request_clear(&self) {
        self.clear_requested.store(true, Ordering::Relaxed);
    }

    pub fn quit(&self) {
        self.should_quit.store(true, Ordering::Relaxed);
    }

    pub fn quit_requested(&self) -> bool {
        self.should_quit.load(Ordering::Relaxed)
    }

    pub fn set_focus(&self, p: Panel) {
        *self.focused.lock() = p;
    }

    pub fn cycle_focus(&self) {
        use crate::ui::map_renderer::focus_target;
        let next = focus_target(*self.focused.lock());
        *self.focused.lock() = next;
    }

    /// Called by the UI tick once per frame.
    pub fn tick(&self) {
        // 1. clear-dots request
        if self.clear_requested.swap(false, Ordering::Relaxed) {
            self.dots.lock().clear();
        }

        // 2. expire old dots
        let cfg = self.config.read();
        let ttl = cfg.marker_ttl();
        let max_markers = cfg.max_markers;
        drop(cfg);

        let now = Instant::now();
        let mut dots = self.dots.lock();
        while let Some(front) = dots.front() {
            if now.duration_since(front.created_at) > ttl {
                dots.pop_front();
            } else {
                break;
            }
        }
        while dots.len() > max_markers {
            dots.pop_front();
        }

        // 3. throughput ring (1-second bins, keep 120 entries)
        let total_dots = dots.len();
        drop(dots);
        let events_per_sec = {
            let mut ring = self.throughput_ring.lock();
            if ring.len() >= 120 {
                ring.pop_front();
            }
            ring.push_back(0);
            let total: u64 = ring.iter().sum();
            total as f64 / ring.len().max(1) as f64
        };

        let mut stats = self.stats.lock();
        stats.events_per_sec = events_per_sec;
        stats.proxy_pct = if total_dots > 0 {
            let proxy_hits = self
                .per_country
                .iter()
                .filter(|c| c.key().starts_with('~'))
                .count();
            (proxy_hits as f64 / total_dots as f64 * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        };
    }

    /// Push a freshly-ingested event into all the shared structures.
    pub fn ingest(&self, ev: ConnectionEvent) {
        let plot = !self.is_paused();

        let hits = {
            let mut entry = self.per_ip.entry(ev.src_ip).or_default();
            entry.hits += 1;
            entry.last_seen = ev.timestamp;
            entry.hits
        };
        let escalated = if hits >= WARN_THRESHOLD {
            Severity::Warn
        } else {
            ev.severity
        };

        if let Some(info) = self.geo.lookup(ev.src_ip) {
            if plot && is_plottable_ip(ev.src_ip) {
                self.plot(&info, escalated, &ev);
            }
            let key = info
                .country_code
                .clone()
                .or(info.country_name.clone())
                .unwrap_or_else(|| "??".into());
            *self.per_country.entry(key).or_insert(0) += 1;

            self.push_log_row(LogRow {
                timestamp: ev.timestamp,
                src_ip: ev.src_ip,
                country: info.country_name.clone().unwrap_or_default(),
                city: info.city.clone().unwrap_or_default(),
                proxy: info.proxy_kind,
                http_status: ev.http_status,
                http_method: ev.http_method.clone(),
                http_path: ev.http_path.clone(),
                user_agent: ev.user_agent.clone(),
                protocol: ev.protocol.clone(),
                bytes: ev.bytes.unwrap_or(0),
                source: ev.source,
                severity: escalated,
            });
        } else {
            self.push_log_row(LogRow {
                timestamp: ev.timestamp,
                src_ip: ev.src_ip,
                country: String::new(),
                city: String::new(),
                proxy: None,
                http_status: ev.http_status,
                http_method: ev.http_method.clone(),
                http_path: ev.http_path.clone(),
                user_agent: ev.user_agent.clone(),
                protocol: ev.protocol.clone(),
                bytes: ev.bytes.unwrap_or(0),
                source: ev.source,
                severity: escalated,
            });
        }

        let mut ring = self.throughput_ring.lock();
        if let Some(tail) = ring.back_mut() {
            *tail += 1;
        }
        drop(ring);

        let mut stats = self.stats.lock();
        if let Some(b) = ev.bytes {
            stats.bytes_total += b;
        }
        stats.total_events += 1;
    }

    fn plot(&self, info: &GeoInfo, severity: Severity, ev: &ConnectionEvent) {
        if let (Some(lat), Some(lon)) = (info.latitude, info.longitude) {
            let max_markers = self.config.read().max_markers;
            let mut dots = self.dots.lock();
            if dots.len() >= max_markers {
                dots.pop_front();
            }
            dots.push_back(MapDot {
                lat,
                lon,
                country: info.country_name.clone().unwrap_or_default(),
                city: info.city.clone().unwrap_or_default(),
                severity,
                src_ip: ev.src_ip,
                proxy: info.proxy_kind,
                created_at: Instant::now(),
            });
        }
    }

    fn push_log_row(&self, row: LogRow) {
        let mut log = self.log.lock();
        if log.len() >= MAX_LOG_ROWS {
            log.pop_front();
        }
        log.push_back(row);
        let mut state = self.log_state.lock();
        state.select(Some(log.len().saturating_sub(1)));
    }
}

/// One row shown in the live-log panel.
#[derive(Debug, Clone)]
pub struct LogRow {
    pub timestamp: DateTime<Utc>,
    pub src_ip: IpAddr,
    pub country: String,
    pub city: String,
    pub proxy: Option<crate::geo::lookup::ProxyKind>,
    pub http_status: Option<u16>,
    #[allow(dead_code)]
    pub http_method: Option<String>,
    pub http_path: Option<String>,
    #[allow(dead_code)]
    pub user_agent: Option<String>,
    #[allow(dead_code)]
    pub protocol: String,
    #[allow(dead_code)]
    pub bytes: u64,
    #[allow(dead_code)]
    pub source: crate::event::Source,
    pub severity: Severity,
}

impl LogRow {
    #[allow(dead_code)]
    pub fn fmt_line(&self) -> String {
        let proxy = self.proxy.map(|p| p.label()).unwrap_or("    ");
        let city = if self.city.is_empty() {
            "—"
        } else {
            &self.city
        };
        let cc = if self.country.is_empty() {
            "—"
        } else {
            &self.country
        };
        let status = self
            .http_status
            .map(|s| format!("{s}"))
            .unwrap_or_else(|| "   ".into());
        let path = self
            .http_path
            .as_deref()
            .map(|p| if p.len() > 32 { &p[..32] } else { p })
            .unwrap_or("");
        format!(
            "{ts} {src:<15} {cc:<14} {city:<14} [{proxy}] [{sev}] {proto:<5} {status:<3} {path}",
            ts = self.timestamp.format("%H:%M:%S"),
            src = self.src_ip,
            cc = cc,
            city = city,
            proxy = proxy,
            sev = self.severity.label(),
            proto = self.protocol,
            status = status,
            path = path,
        )
    }
}

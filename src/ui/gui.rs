//! Native GUI window mode powered by `eframe`/`egui`.
//!
//! Activated with `--gui`. Reuses `MapRenderer` for the world map, then draws
//! metrics, top-talkers, and the live connection log as egui panels on top.

use std::collections::VecDeque;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use eframe::egui;
use image::RgbaImage;
use parking_lot::{Mutex, RwLock};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::config::Config;
use crate::event::{is_plottable_ip, ConnectionEvent, Severity, Source};
use crate::geo::lookup::{GeoInfo, GeoLookup, ProxyKind};
use crate::ui::app::MapDot;
use crate::ui::map_renderer::{HomeLocation, MapRenderer};

const MAX_LOG_ROWS: usize = 2_000;
const WARN_THRESHOLD: u64 = 30;

/// Shared state between the async ingestion workers and the egui UI thread.
pub struct GuiState {
    pub dots: VecDeque<MapDot>,
    pub log: VecDeque<LogRow>,
    pub per_ip: DashMap<IpAddr, PerIpMetrics>,
    pub per_country: DashMap<String, u64>,
    pub total_events: u64,
    pub bytes_total: u64,
    pub throughput_ring: VecDeque<u64>,
    pub paused: AtomicBool,
    pub clear_requested: AtomicBool,
    pub connection_lines: AtomicBool,
    pub should_quit: AtomicBool,
    /// Shared configuration; read each frame so hot-reload applies live.
    pub config: Arc<RwLock<Config>>,
}

impl GuiState {
    pub fn new(config: Arc<RwLock<Config>>) -> Self {
        let max_markers = config.read().max_markers;
        Self {
            dots: VecDeque::with_capacity(max_markers),
            log: VecDeque::with_capacity(MAX_LOG_ROWS),
            per_ip: DashMap::new(),
            per_country: DashMap::new(),
            total_events: 0,
            bytes_total: 0,
            throughput_ring: VecDeque::with_capacity(120),
            paused: AtomicBool::new(false),
            clear_requested: AtomicBool::new(false),
            connection_lines: AtomicBool::new(false),
            should_quit: AtomicBool::new(false),
            config,
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

    pub fn should_quit(&self) -> bool {
        self.should_quit.load(Ordering::Relaxed)
    }

    /// Age out old dots and advance the throughput ring.
    pub fn tick(&mut self) {
        if self.clear_requested.swap(false, Ordering::Relaxed) {
            self.dots.clear();
        }

        let cfg = self.config.read();
        let ttl = cfg.marker_ttl();
        let max_markers = cfg.max_markers;
        drop(cfg);

        let now = Instant::now();
        while let Some(front) = self.dots.front() {
            if now.duration_since(front.created_at) > ttl {
                self.dots.pop_front();
            } else {
                break;
            }
        }
        while self.dots.len() > max_markers {
            self.dots.pop_front();
        }

        if self.throughput_ring.len() >= 120 {
            self.throughput_ring.pop_front();
        }
        self.throughput_ring.push_back(0);
    }

    /// Push a freshly ingested event into all shared structures.
    /// `info` may be `None` for addresses the geo database can't resolve
    /// (e.g. many IPv6 addresses); the event is still logged and counted.
    pub fn ingest(&mut self, ev: ConnectionEvent, info: Option<&GeoInfo>) {
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

        if plot && is_plottable_ip(ev.src_ip) {
            if let Some(info) = info {
                if let (Some(lat), Some(lon)) = (info.latitude, info.longitude) {
                    let max_markers = self.config.read().max_markers;
                    if self.dots.len() >= max_markers {
                        self.dots.pop_front();
                    }
                    self.dots.push_back(MapDot {
                        lat,
                        lon,
                        country: info.country_name.clone().unwrap_or_default(),
                        city: info.city.clone().unwrap_or_default(),
                        severity: escalated,
                        src_ip: ev.src_ip,
                        proxy: info.proxy_kind,
                        created_at: Instant::now(),
                    });
                }
            }
        }

        if let Some(info) = info {
            let key = info
                .country_code
                .clone()
                .or(info.country_name.clone())
                .unwrap_or_else(|| "??".into());
            *self.per_country.entry(key).or_insert(0) += 1;
        }

        if self.log.len() >= MAX_LOG_ROWS {
            self.log.pop_front();
        }
        self.log.push_back(LogRow {
            timestamp: ev.timestamp,
            src_ip: ev.src_ip,
            country: info.and_then(|i| i.country_name.clone()).unwrap_or_default(),
            city: info.and_then(|i| i.city.clone()).unwrap_or_default(),
            proxy: info.and_then(|i| i.proxy_kind),
            http_status: ev.http_status,
            http_method: ev.http_method,
            http_path: ev.http_path,
            protocol: ev.protocol,
            bytes: ev.bytes.unwrap_or(0),
            source: ev.source,
            severity: escalated,
        });

        if let Some(tail) = self.throughput_ring.back_mut() {
            *tail += 1;
        }

        if let Some(b) = ev.bytes {
            self.bytes_total += b;
        }
        self.total_events += 1;
    }

    pub fn events_per_sec(&self) -> f64 {
        let total: u64 = self.throughput_ring.iter().sum();
        total as f64 / self.throughput_ring.len().max(1) as f64
    }

    pub fn proxy_pct(&self) -> f64 {
        if self.dots.is_empty() {
            return 0.0;
        }
        let proxy_hits = self
            .per_country
            .iter()
            .filter(|c| c.key().starts_with('~'))
            .count();
        (proxy_hits as f64 / self.dots.len() as f64 * 100.0).clamp(0.0, 100.0)
    }
}

#[derive(Debug, Default, Clone)]
pub struct PerIpMetrics {
    pub hits: u64,
    pub last_seen: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct LogRow {
    pub timestamp: DateTime<Utc>,
    pub src_ip: IpAddr,
    pub country: String,
    pub city: String,
    pub proxy: Option<ProxyKind>,
    pub http_status: Option<u16>,
    #[allow(dead_code)]
    pub http_method: Option<String>,
    pub http_path: Option<String>,
    #[allow(dead_code)]
    pub protocol: String,
    #[allow(dead_code)]
    pub bytes: u64,
    #[allow(dead_code)]
    pub source: Source,
    pub severity: Severity,
}

/// The egui app state.
pub struct GuiApp {
    state: Arc<Mutex<GuiState>>,
    events: mpsc::UnboundedReceiver<ConnectionEvent>,
    geo: Arc<GeoLookup>,
    map_renderer: Option<Arc<MapRenderer>>,
    home: HomeLocation,
    config: Arc<RwLock<Config>>,
    map_texture: Option<egui::TextureHandle>,
    last_map: Option<RgbaImage>,
    /// Last config whose fonts / window size were applied; used for hot-reload.
    last_applied_config: Config,
    /// Current map zoom (1.0 = full world).
    map_zoom: f32,
    /// Current map pan in screen pixels.
    map_pan: egui::Vec2,
    /// Pan value at the start of an active drag.
    drag_start_pan: Option<egui::Vec2>,
    /// Pointer position at the start of an active drag.
    drag_start_pos: Option<egui::Pos2>,
}

impl GuiApp {
    pub fn new(
        events: mpsc::UnboundedReceiver<ConnectionEvent>,
        geo: Arc<GeoLookup>,
        map_renderer: Option<Arc<MapRenderer>>,
        home: HomeLocation,
        config: Arc<RwLock<Config>>,
    ) -> Self {
        let initial_config = config.read().clone();
        Self {
            state: Arc::new(Mutex::new(GuiState::new(config.clone()))),
            events,
            geo,
            map_renderer,
            home,
            config,
            map_texture: None,
            last_map: None,
            last_applied_config: initial_config,
            map_zoom: 1.0,
            map_pan: egui::Vec2::ZERO,
            drag_start_pan: None,
            drag_start_pos: None,
        }
    }

    /// Compute the map renderer viewport from the current GUI zoom/pan.
    fn current_viewport(&self,
        _size: egui::Vec2,
        rect: egui::Rect,
    ) -> Option<crate::ui::map_renderer::Viewport> {
        if self.map_zoom <= 1.0 + f32::EPSILON {
            return None;
        }
        let zoom = self.map_zoom.clamp(1.0, 10.0) as f64;
        // Pan is in screen pixels relative to the centered map. Convert to a
        // lat/lon offset on the equirectangular world map.  Because the map
        // has a natural 2:1 aspect ratio, zoom scales both axes by the same
        // factor; the output rect is already constrained to 2:1, so we do not
        // need to include its aspect here.
        let half_lon_deg = (180.0 / zoom).min(180.0);
        let half_lat_deg = (90.0 / zoom).min(90.0);
        let px_per_lon = rect.width() as f64 / (2.0 * half_lon_deg);
        let px_per_lat = rect.height() as f64 / (2.0 * half_lat_deg);
        let lon_offset = (self.map_pan.x as f64) / px_per_lon;
        let lat_offset = (self.map_pan.y as f64) / px_per_lat;

        Some(crate::ui::map_renderer::Viewport {
            zoom,
            center_lat: (self.home.lat + lat_offset).clamp(-80.0, 80.0),
            center_lon: (self.home.lon - lon_offset).clamp(-180.0, 180.0),
        })
    }

    /// Convert a lat/lon to screen coordinates inside the displayed map rect,
    /// respecting zoom and pan.
    fn map_to_screen(
        &self,
        lat: f64,
        lon: f64,
        _size: egui::Vec2,
        rect: egui::Rect,
    ) -> Option<egui::Pos2> {
        let vp = self.current_viewport(rect.size(), rect)?;
        let (px, py) = vp.latlon_to_pixel(lat, lon, rect.width() as u32, rect.height() as u32);
        Some(egui::pos2(rect.min.x + px as f32, rect.min.y + py as f32))
    }

    /// Drain the ingestion channel and fold events into shared state.
    fn drain_events(&mut self) {
        let mut state = self.state.lock();
        while let Ok(ev) = self.events.try_recv() {
            let info = self.geo.lookup(ev.src_ip);
            state.ingest(ev, info.as_ref());
        }
    }

    /// Render the current map (base + dots + home pulse) to an egui texture.
    fn no_map(&self) -> bool {
        self.map_renderer.is_none()
    }

    fn render_full_log(&self,
        ui: &mut egui::Ui,
        colors: &crate::config::ColorConfig,
    ) {
        ui.heading("Live Connections");
        ui.separator();
        self.render_log_rows(ui, colors, 200);
    }

    /// Compact log strip used at the bottom of the normal GUI layout.
    fn render_log_strip(&self,
        ui: &mut egui::Ui,
        colors: &crate::config::ColorConfig,
    ) {
        ui.horizontal(|ui: &mut egui::Ui| {
            ui.heading("Live Connections");
        });
        ui.separator();
        self.render_log_rows(ui, colors, 6);
    }

    fn render_log_rows(&self,
        ui: &mut egui::Ui,
        colors: &crate::config::ColorConfig,
        limit: usize,
    ) {
        let visible: Vec<LogRow> = {
            let state = self.state.lock();
            state.log.iter().rev().take(limit).cloned().collect()
        };

        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .show(ui, |ui: &mut egui::Ui| {
                for row in visible {
                    let sev_color = match row.severity {
                        Severity::Info => colors.info.to_egui(),
                        Severity::Warn => colors.warn.to_egui(),
                        Severity::Alert => colors.alert.to_egui(),
                    };
                    let proxy = row.proxy.map(|p| p.label()).unwrap_or("    ");
                    let status = row.http_status.map(|s| format!("{s}")).unwrap_or_default();
                    let path = row
                        .http_path
                        .as_deref()
                        .map(|p| if p.len() > 50 { &p[..50] } else { p })
                        .unwrap_or("");
                    let ip = row.src_ip.to_string();
                    ui.horizontal(|ui: &mut egui::Ui| {
                        ui.label(
                            egui::RichText::new(row.timestamp.format("%H:%M:%S").to_string())
                                .color(colors.dim.to_egui()),
                        );
                        let ip_label = ui.add(
                            egui::Label::new(egui::RichText::new(&ip).color(colors.focus.to_egui()))
                                .sense(egui::Sense::click()),
                        );
                        if ip_label.clicked() {
                            ui.ctx().copy_text(ip.clone());
                        }
                        if ip_label.hovered() {
                            ip_label.show_tooltip_text("Click to copy IP");
                        }
                        ui.label(egui::RichText::new(format!("[{proxy}]")).color(sev_color));
                        ui.label(format!("{:<14}", row.country));
                        ui.label(format!("{:<14}", row.city));
                        ui.label(egui::RichText::new(status).color(sev_color));
                        ui.label(path);
                    });
                }
            });
    }

    fn ensure_map_texture(
        &mut self,
        ctx: &egui::Context,
        rect: egui::Rect,
    ) -> Option<egui::TextureHandle> {
        let renderer = self.map_renderer.as_ref()?;
        let dots: Vec<MapDot> = {
            let state = self.state.lock();
            state
                .dots
                .iter()
                .map(|d| MapDot {
                    lat: d.lat,
                    lon: d.lon,
                    country: d.country.clone(),
                    city: d.city.clone(),
                    severity: d.severity,
                    src_ip: d.src_ip,
                    proxy: d.proxy,
                    created_at: d.created_at,
                })
                .collect()
        };

        let lines_enabled = self.state.lock().connection_lines.load(Ordering::Relaxed);
        let viewport = self.current_viewport(rect.size(), rect);
        let img = renderer.redraw(&dots, &self.home, lines_enabled, viewport);
        self.last_map = Some(img.clone());

        let size = [img.width() as usize, img.height() as usize];
        let pixels = img.into_raw();
        let color_image = egui::ColorImage::from_rgba_unmultiplied(size, &pixels);

        if let Some(tex) = self.map_texture.as_mut() {
            tex.set(color_image, egui::TextureOptions::LINEAR);
            self.map_texture.clone()
        } else {
            let handle = ctx.load_texture("world-map", color_image, egui::TextureOptions::LINEAR);
            self.map_texture = Some(handle.clone());
            Some(handle)
        }
    }
}

impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Read the latest config (hot-reload may have updated it).
        let cfg = self.config.read().clone();
        let colors = cfg.colors.clone();
        let diff = self.last_applied_config.diff(&cfg);
        // Only overwrite the home marker from config when the user actually
        // edited the config.  Otherwise we keep the auto-detected home (which
        // carries the public IP and city/country label).
        if diff.home {
            self.home = cfg.home_location();
        }
        if diff.gui_font {
            apply_egui_style(ui.ctx(), &cfg);
        }
        if diff.window {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                    cfg.window.width as f32,
                    cfg.window.height as f32,
                )));
        }
        if diff.any() {
            self.last_applied_config = cfg.clone();
        }

        // Drain incoming events and run the per-frame state tick.
        self.drain_events();
        self.state.lock().tick();

        // --- Top status bar ---
        let (events_per_sec, total_events, bytes_total, active_dots, paused) = {
            let state = self.state.lock();
            (
                state.events_per_sec(),
                state.total_events,
                state.bytes_total,
                state.dots.len(),
                state.is_paused(),
            )
        };

        egui::Panel::top("status").show(ui, |ui: &mut egui::Ui| {
            ui.horizontal(|ui: &mut egui::Ui| {
                ui.heading("geotop");
                ui.separator();
                ui.label(format!("● {active_dots} active"));
                ui.label(format!("{events_per_sec:.1} evt/s"));
                ui.label(format!("events: {total_events}"));
                ui.label(format!("bytes: {}", human_bytes(bytes_total)));
                if paused {
                    ui.label(egui::RichText::new("⏸ PAUSED").color(colors.alert.to_egui()));
                }
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui: &mut egui::Ui| {
                        ui.label("[p] pause  [c] clear  [l] lines  [q/Esc] quit");
                    },
                );
            });
        });

        // --- Right-side metrics panel ---
        egui::Panel::right("metrics")
            .resizable(false)
            .default_size(240.0)
            .show(ui, |ui: &mut egui::Ui| {
                ui.heading("Top Talkers");
                ui.separator();

                let mut country_counts: Vec<(String, u64)> = {
                    let state = self.state.lock();
                    state
                        .per_country
                        .iter()
                        .map(|kv| (kv.key().clone(), *kv.value()))
                        .collect()
                };
                country_counts.sort_by(|a, b| b.1.cmp(&a.1));
                country_counts.truncate(10);

                let max = country_counts.first().map(|x| x.1).unwrap_or(0).max(1);
                for (cc, n) in &country_counts {
                    let bar_width = (*n as f32 / max as f32) * 160.0;
                    ui.horizontal(|ui: &mut egui::Ui| {
                        ui.label(format!("{cc:<3}"));
                        ui.painter().rect_filled(
                            egui::Rect::from_min_size(ui.cursor().min, egui::vec2(bar_width, 12.0)),
                            2.0,
                            colors.info.to_egui(),
                        );
                        ui.add_space(bar_width + 4.0);
                        ui.label(format!("{n}"));
                    });
                }

                ui.separator();
                let proxy_pct = {
                    let state = self.state.lock();
                    state.proxy_pct()
                };
                ui.label(format!("Proxy / DC / Tor: {proxy_pct:.0}% (approx)"));

                ui.separator();
                ui.heading("Throughput");
                let ring: Vec<u64> = {
                    let state = self.state.lock();
                    state.throughput_ring.iter().copied().collect()
                };
                sparkline(ui, &ring, colors.info.to_egui());
            });

        // --- Bottom log strip (only when the map is shown) ---
        if !self.no_map() {
            egui::Panel::bottom("log")
                .resizable(false)
                .default_size(180.0)
                .show(ui, |ui: &mut egui::Ui| {
                    self.render_log_strip(ui, &colors);
                });
        }

        // --- Central content: map or full log when --no-map is set ---
        egui::CentralPanel::default().show(ui, |ui: &mut egui::Ui| {
            if self.no_map() {
                self.render_full_log(ui, &colors);
                return;
            }

            let available = ui.available_rect_before_wrap();
            // Keep the world map's natural 2:1 aspect ratio, letterboxed in the
            // remaining central area so side/top/bottom panels stay visible.
            let map_aspect = 2.0_f32;
            let panel_aspect = available.width() / available.height().max(1.0);
            let rect = if panel_aspect > map_aspect {
                let h = available.height();
                let w = h * map_aspect;
                let x = available.min.x + (available.width() - w) * 0.5;
                egui::Rect::from_min_size(egui::pos2(x, available.min.y), egui::vec2(w, h))
            } else {
                let w = available.width();
                let h = w / map_aspect;
                let y = available.min.y + (available.height() - h) * 0.5;
                egui::Rect::from_min_size(egui::pos2(available.min.x, y), egui::vec2(w, h))
            };
            // Map interactions first so the texture and markers both use the
            // same freshly-updated zoom/pan state this frame.
            let response = ui.interact(rect, ui.id().with("map"), egui::Sense::drag());
            let hover_pos = response.hover_pos();

            // Zoom with scroll wheel.
            if let Some(hover) = hover_pos {
                let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                if scroll != 0.0 {
                    let old_zoom = self.map_zoom;
                    self.map_zoom = (self.map_zoom * (1.0 + scroll * 0.001)).clamp(1.0, 10.0);
                    // Zoom towards the mouse pointer.
                    let zoom_factor = self.map_zoom / old_zoom;
                    let pointer_offset = hover.to_vec2() - rect.center().to_vec2();
                    self.map_pan = self.map_pan * zoom_factor + pointer_offset * (zoom_factor - 1.0);
                }
            }

            // Pan by dragging.
            if response.drag_started() {
                self.drag_start_pan = Some(self.map_pan);
                self.drag_start_pos = response.interact_pointer_pos();
            }
            if let (Some(start_pan), Some(start_pos)) = (self.drag_start_pan, self.drag_start_pos) {
                if let Some(current) = response.interact_pointer_pos() {
                    self.map_pan = start_pan + (current - start_pos);
                }
            }
            if response.drag_stopped() {
                self.drag_start_pan = None;
                self.drag_start_pos = None;
            }

            if let Some(tex) = self.ensure_map_texture(ui.ctx(), rect) {
                ui.painter().image(
                    tex.id(),
                    rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );

                // Draw active markers and connection lines as screen-space overlays so
                // they remain visible even after the texture is scaled down.
                let (dots, lines_enabled, ttl, marker_size, home_size, now, line_color) = {
                    let state = self.state.lock();
                    let cfg = state.config.read();
                    (
                        state.dots.iter().cloned().collect::<Vec<_>>(),
                        state.connection_lines.load(Ordering::Relaxed),
                        cfg.marker_ttl(),
                        cfg.marker_size,
                        cfg.map.home.marker_size,
                        Instant::now(),
                        cfg.connection_lines.color.to_egui(),
                    )
                };

                let map_to_screen = |lat: f64, lon: f64| {
                    if let Some(pos) = self.map_to_screen(lat, lon, tex.size_vec2(), rect) {
                        return pos;
                    }
                    let (px, py) = crate::ui::map_renderer::latlon_to_pixel(
                        lat,
                        lon,
                        tex.size_vec2().x as u32,
                        tex.size_vec2().y as u32,
                    );
                    let u = px as f32 / tex.size_vec2().x;
                    let v = py as f32 / tex.size_vec2().y;
                    egui::pos2(rect.min.x + u * rect.width(), rect.min.y + v * rect.height())
                };

                let home_pos = map_to_screen(self.home.lat, self.home.lon);
                let scale = (rect.width() / tex.size_vec2().x).max(rect.height() / tex.size_vec2().y);
                let dot_radius = (marker_size as f32 * scale).max(4.0);
                let home_radius = (home_size as f32 * scale).max(8.0);
                let ttl_secs = ttl.as_secs_f64();

                // Country / city labels: drawn *before* markers and lines so they
                // sit visually underneath, with a subtle transparent text and
                // background so connection lines and markers remain prominent.
                let label_config = &cfg.map.labels;
                let zoom = self.map_zoom as f64;
                let label_text_color = egui::Color32::from_gray(230).gamma_multiply(0.75);
                let label_bg = egui::Color32::from_black_alpha(80);

                let draw_label = |
                    ui: &mut egui::Ui,
                    pos: egui::Pos2,
                    anchor: egui::Align2,
                    text: &str,
                    font_size: f32,
                | {
                    let galley = ui.painter().layout_no_wrap(
                        text.to_owned(),
                        egui::FontId::new(font_size, egui::FontFamily::Proportional),
                        egui::Color32::PLACEHOLDER,
                    );
                    let size = galley.size();
                    let label_rect = anchor.anchor_size(pos, size).expand(2.0);
                    ui.painter().rect_filled(label_rect, 2.0, label_bg);
                    ui.painter().galley(
                        label_rect.min + egui::vec2(2.0, 1.0),
                        galley,
                        label_text_color,
                    );
                };

                if label_config.show_country_labels {
                    let base_country_font = 7.0f64;
                    let country_font = (base_country_font * zoom.sqrt()).clamp(6.0, 22.0) as f32;
                    for label in self.map_renderer.as_ref().unwrap().country_labels() {
                        let pos = map_to_screen(label.lat, label.lon);
                        if !rect.expand(4.0).contains(pos) {
                            continue;
                        }
                        draw_label(ui, pos, egui::Align2::CENTER_CENTER, &label.name, country_font);
                    }
                }

                if label_config.show_city_labels && zoom >= label_config.city_label_zoom {
                    let city_font = (8.0f64 * zoom.sqrt()).clamp(7.0, 18.0) as f32;
                    for d in &dots {
                        let city = if d.city.is_empty() { &d.country } else { &d.city };
                        if city.is_empty() {
                            continue;
                        }
                        let pos = map_to_screen(d.lat, d.lon);
                        if !rect.expand(20.0).contains(pos) {
                            continue;
                        }
                        let label_pos = pos - egui::vec2(0.0, dot_radius + 6.0);
                        draw_label(ui, label_pos, egui::Align2::CENTER_BOTTOM, city, city_font);
                    }
                }

                // Connection lines first (under markers).
                if lines_enabled {
                    for d in &dots {
                        let target = map_to_screen(d.lat, d.lon);
                        ui.painter().line_segment(
                            [home_pos, target],
                            egui::Stroke::new(1.5, line_color),
                        );
                    }
                }

                // Active IP dots + hover detection.
                let mut hovered: Option<(&MapDot, egui::Pos2, f32)> = None;
                for d in &dots {
                    let pos = map_to_screen(d.lat, d.lon);
                    let age = now.duration_since(d.created_at).as_secs_f64();
                    let alpha = 1.0 - (age / ttl_secs).clamp(0.0, 1.0);
                    if alpha <= 0.0 {
                        continue;
                    }
                    let base = match d.severity {
                        Severity::Info => colors.info.to_egui(),
                        Severity::Warn => colors.warn.to_egui(),
                        Severity::Alert => colors.alert.to_egui(),
                    };
                    let faded = base.gamma_multiply(alpha as f32);
                    ui.painter().circle_filled(pos, dot_radius, faded);
                    ui.painter().circle_stroke(
                        pos,
                        dot_radius,
                        egui::Stroke::new(1.0, egui::Color32::from_black_alpha(120)),
                    );
                    if let Some(mouse) = hover_pos {
                        let dist = pos.distance(mouse);
                        if dist <= dot_radius.max(10.0) {
                            if hovered.map(|(_, _, d)| dist < d).unwrap_or(true) {
                                hovered = Some((d, pos, dist));
                            }
                        }
                    }
                }

                // Home pulse overlay.
                let t = ui.input(|i| i.time);
                let pulse = ((t * 2.0).sin() + 1.0) * 0.5;
                let home_color = colors.home.to_egui();
                ui.painter().circle_filled(home_pos, home_radius, home_color);
                ui.painter().circle_stroke(
                    home_pos,
                    home_radius + 2.0 + pulse as f32 * 6.0,
                    egui::Stroke::new(2.0, home_color.gamma_multiply(0.6)),
                );

                // Home hover.
                if let Some(mouse) = hover_pos {
                    if home_pos.distance(mouse) <= home_radius.max(12.0) {
                        hovered = None; // prefer home tooltip
                        egui::Popup::new(
                            response.id.with("home-tooltip"),
                            ui.ctx().clone(),
                            egui::PopupAnchor::from(mouse),
                            ui.layer_id(),
                        )
                        .kind(egui::PopupKind::Tooltip)
                        .open(true)
                        .show(|ui: &mut egui::Ui| {
                            ui.set_max_width(ui.spacing().tooltip_width);
                            ui.label(format!("Home: {}", self.home.ip.map(|ip| ip.to_string()).unwrap_or_default()));
                            if let Some(label) = &self.home.label {
                                ui.label(label);
                            }
                            ui.label(format!("{:.4}°{}, {:.4}°{}",
                                self.home.lat.abs(),
                                if self.home.lat >= 0.0 { "N" } else { "S" },
                                self.home.lon.abs(),
                                if self.home.lon >= 0.0 { "E" } else { "W" }));
                        });
                    }
                }

                if let Some((d, _pos, _)) = hovered {
                    egui::Popup::new(
                        response.id.with("marker-tooltip"),
                        ui.ctx().clone(),
                        egui::PopupAnchor::from(hover_pos.unwrap_or(rect.center())),
                        ui.layer_id(),
                    )
                    .kind(egui::PopupKind::Tooltip)
                    .open(true)
                    .show(|ui: &mut egui::Ui| {
                        ui.set_max_width(ui.spacing().tooltip_width);
                        ui.label(format!("IP: {}", d.src_ip));
                        if !d.country.is_empty() {
                            ui.label(format!("Country: {}", d.country));
                        }
                        if !d.city.is_empty() {
                            ui.label(format!("City: {}", d.city));
                        }
                        if let Some(proxy) = d.proxy {
                            ui.label(format!("Proxy: {}", proxy.label()));
                        }
                        ui.label(format!("{:.4}°{}, {:.4}°{}",
                            d.lat.abs(),
                            if d.lat >= 0.0 { "N" } else { "S" },
                            d.lon.abs(),
                            if d.lon >= 0.0 { "E" } else { "W" }));
                    });
                }

                // Home info label at the top-left of the map.
                let home_text = self.home.overlay_text();
                let text_pos = egui::pos2(rect.min.x + 6.0, rect.min.y + 4.0);
                ui.painter().text(
                    text_pos,
                    egui::Align2::LEFT_TOP,
                    home_text,
                    egui::FontId::new(11.0, egui::FontFamily::Proportional),
                    colors.dim.to_egui(),
                );

                // Zoom level indicator.
                if self.map_zoom > 1.01 {
                    let zoom_text = format!("zoom: {:.1}x", self.map_zoom);
                    ui.painter().text(
                        egui::pos2(rect.max.x - 6.0, rect.min.y + 4.0),
                        egui::Align2::RIGHT_TOP,
                        zoom_text,
                        egui::FontId::new(11.0, egui::FontFamily::Proportional),
                        colors.dim.to_egui(),
                    );
                }

            } else {
                ui.centered_and_justified(|ui: &mut egui::Ui| {
                    ui.label("Loading map…");
                });
            }
        });

        // --- Keyboard shortcuts ---
        ui.input(|i| {
            if i.key_pressed(egui::Key::Q) || i.key_pressed(egui::Key::Escape) {
                self.state.lock().quit();
            }
            if i.key_pressed(egui::Key::P) {
                self.state.lock().toggle_pause();
            }
            if i.key_pressed(egui::Key::C) {
                self.state.lock().request_clear();
            }
            if i.key_pressed(egui::Key::L) {
                let state = self.state.lock();
                let prev = state.connection_lines.load(Ordering::Relaxed);
                state.connection_lines.store(!prev, Ordering::Relaxed);
            }
            // Map zoom and reset (only meaningful when map is shown).
            if !self.no_map() {
                if i.key_pressed(egui::Key::Plus) || i.key_pressed(egui::Key::Equals) {
                    self.map_zoom = (self.map_zoom * 1.2).clamp(1.0, 10.0);
                }
                if i.key_pressed(egui::Key::Minus) {
                    self.map_zoom = (self.map_zoom / 1.2).clamp(1.0, 10.0);
                }
                if i.key_pressed(egui::Key::Num0) {
                    self.map_zoom = 1.0;
                    self.map_pan = egui::Vec2::ZERO;
                }
            }
        });

        // Request close if user asked to quit.
        if self.state.lock().should_quit() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn on_exit(&mut self) {
        self.state.lock().quit();
    }
}

/// Apply configured font sizes and an optional custom font file to egui.
pub fn apply_egui_style(ctx: &egui::Context, cfg: &Config) {
    let mut fonts = egui::FontDefinitions::default();
    if let Some(ref path) = cfg.fonts.gui_font_file {
        match std::fs::read(path) {
            Ok(bytes) => {
                fonts.font_data.insert(
                    "custom".to_owned(),
                    std::sync::Arc::new(egui::FontData::from_owned(bytes)),
                );
                fonts
                    .families
                    .entry(egui::FontFamily::Proportional)
                    .or_default()
                    .insert(0, "custom".to_owned());
                info!(path = %path.display(), "loaded custom GUI font");
            }
            Err(e) => warn!(path = %path.display(), error = %e, "failed to load custom GUI font"),
        }
    }
    ctx.set_fonts(fonts);

    let theme = egui::Theme::Dark;
    let mut style = (*ctx.style_of(theme)).clone();
    style.text_styles.insert(
        egui::TextStyle::Body,
        egui::FontId::new(cfg.fonts.gui_body, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Heading,
        egui::FontId::new(cfg.fonts.gui_heading, egui::FontFamily::Proportional),
    );
    ctx.set_style_of(theme, style);
}

/// Simple bar-style sparkline drawn with the egui painter.
fn sparkline(ui: &mut egui::Ui, data: &[u64], color: egui::Color32) {
    let available = ui.available_size();
    let height = available.y.max(40.0);
    let width = available.x.max(100.0);
    let (rect, _response) = ui.allocate_exact_size(
        egui::vec2(width, height),
        egui::Sense::focusable_noninteractive(),
    );

    if data.len() < 2 {
        return;
    }
    let max = *data.iter().max().unwrap_or(&1).max(&1);
    let step = width / (data.len().saturating_sub(1) as f32);

    for (i, &val) in data.iter().enumerate() {
        let h = (val as f32 / max as f32) * height;
        let x = rect.min.x + i as f32 * step;
        let bar =
            egui::Rect::from_min_size(egui::pos2(x, rect.max.y - h), egui::vec2(step.max(1.0), h));
        ui.painter().rect_filled(bar, 1.0, color);
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut val = n as f64;
    let mut i = 0;
    while val >= 1024.0 && i < UNITS.len() - 1 {
        val /= 1024.0;
        i += 1;
    }
    format!("{:.2} {}", val, UNITS[i])
}

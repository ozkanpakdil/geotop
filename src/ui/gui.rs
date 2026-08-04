//! Native GUI window mode powered by `eframe`/`egui`.
//!
//! Activated with `--gui`. Reuses `MapRenderer` for the world map, then draws
//! metrics, top-talkers, and the live connection log as egui panels on top.

use std::collections::VecDeque;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use eframe::egui;
use image::RgbaImage;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::event::{ConnectionEvent, Severity, Source};
use crate::geo::lookup::{GeoInfo, GeoLookup, ProxyKind};
use crate::ui::app::MapDot;
use crate::ui::map_renderer::{HomeLocation, MapRenderer};

const MAX_DOTS: usize = 5_000;
const MAX_LOG_ROWS: usize = 2_000;
const DOT_TTL: Duration = Duration::from_secs(8);
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
    pub should_quit: AtomicBool,
}

impl GuiState {
    pub fn new() -> Self {
        Self {
            dots: VecDeque::with_capacity(MAX_DOTS),
            log: VecDeque::with_capacity(MAX_LOG_ROWS),
            per_ip: DashMap::new(),
            per_country: DashMap::new(),
            total_events: 0,
            bytes_total: 0,
            throughput_ring: VecDeque::with_capacity(120),
            paused: AtomicBool::new(false),
            clear_requested: AtomicBool::new(false),
            should_quit: AtomicBool::new(false),
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

        let now = Instant::now();
        while let Some(front) = self.dots.front() {
            if now.duration_since(front.created_at) > DOT_TTL {
                self.dots.pop_front();
            } else {
                break;
            }
        }

        if self.throughput_ring.len() >= 120 {
            self.throughput_ring.pop_front();
        }
        self.throughput_ring.push_back(0);
    }

    /// Push a freshly ingested event into all shared structures.
    pub fn ingest(&mut self, ev: ConnectionEvent, info: &GeoInfo) {
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

        if plot {
            if let (Some(lat), Some(lon)) = (info.latitude, info.longitude) {
                if self.dots.len() >= MAX_DOTS {
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

        let key = info
            .country_code
            .clone()
            .or(info.country_name.clone())
            .unwrap_or_else(|| "??".into());
        *self.per_country.entry(key).or_insert(0) += 1;

        if self.log.len() >= MAX_LOG_ROWS {
            self.log.pop_front();
        }
        self.log.push_back(LogRow {
            timestamp: ev.timestamp,
            src_ip: ev.src_ip,
            country: info.country_name.clone().unwrap_or_default(),
            city: info.city.clone().unwrap_or_default(),
            proxy: info.proxy_kind,
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
pub struct GuiMapDot {
    pub lat: f64,
    pub lon: f64,
    pub country: String,
    pub city: String,
    pub severity: Severity,
    pub src_ip: IpAddr,
    pub proxy: Option<ProxyKind>,
    pub created_at: Instant,
}

#[derive(Debug, Clone)]
pub struct LogRow {
    pub timestamp: DateTime<Utc>,
    pub src_ip: IpAddr,
    pub country: String,
    pub city: String,
    pub proxy: Option<ProxyKind>,
    pub http_status: Option<u16>,
    pub http_method: Option<String>,
    pub http_path: Option<String>,
    pub protocol: String,
    pub bytes: u64,
    pub source: Source,
    pub severity: Severity,
}

/// The egui app state.
pub struct GuiApp {
    state: Arc<Mutex<GuiState>>,
    events: mpsc::UnboundedReceiver<ConnectionEvent>,
    geo: Arc<GeoLookup>,
    map_renderer: Arc<MapRenderer>,
    home: HomeLocation,
    map_texture: Option<egui::TextureHandle>,
    last_map: Option<RgbaImage>,
}

impl GuiApp {
    pub fn new(
        events: mpsc::UnboundedReceiver<ConnectionEvent>,
        geo: Arc<GeoLookup>,
        map_renderer: Arc<MapRenderer>,
        home: HomeLocation,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(GuiState::new())),
            events,
            geo,
            map_renderer,
            home,
            map_texture: None,
            last_map: None,
        }
    }

    pub fn state(&self) -> Arc<Mutex<GuiState>> {
        self.state.clone()
    }

    /// Drain the ingestion channel and fold events into shared state.
    fn drain_events(&mut self) {
        let mut state = self.state.lock();
        while let Ok(ev) = self.events.try_recv() {
            if let Some(info) = self.geo.lookup(ev.src_ip) {
                state.ingest(ev, &info);
            }
        }
    }

    /// Render the current map (base + dots + home pulse) to an egui texture.
    fn ensure_map_texture(&mut self, ctx: &egui::Context) -> Option<egui::TextureHandle> {
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

        let img = self.map_renderer.redraw(&dots, self.home);
        self.last_map = Some(img.clone());

        let size = [img.width() as usize, img.height() as usize];
        let pixels = img.into_raw();
        let color_image = egui::ColorImage::from_rgba_unmultiplied(size, &pixels);

        if let Some(tex) = self.map_texture.as_mut() {
            tex.set(color_image, egui::TextureOptions::LINEAR);
            self.map_texture.clone()
        } else {
            let handle = ctx.load_texture(
                "world-map",
                color_image,
                egui::TextureOptions::LINEAR,
            );
            self.map_texture = Some(handle.clone());
            Some(handle)
        }
    }
}

impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Drain incoming events and run the per-frame state tick.
        self.drain_events();
        self.state.lock().tick();

        // Update the map texture every frame so dots fade smoothly.
        let map_texture = self.ensure_map_texture(ui.ctx());

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
                    ui.label(egui::RichText::new("⏸ PAUSED").color(egui::Color32::RED));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui: &mut egui::Ui| {
                    ui.label("[p] pause  [c] clear  [q/Esc] quit");
                });
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
                            egui::Rect::from_min_size(
                                ui.cursor().min,
                                egui::vec2(bar_width, 12.0),
                            ),
                            2.0,
                            egui::Color32::from_rgb(80, 220, 120),
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
                sparkline(ui, &ring, egui::Color32::from_rgb(80, 220, 120));
            });

        // --- Bottom log panel ---
        egui::Panel::bottom("log")
            .resizable(false)
            .default_size(160.0)
            .show(ui, |ui: &mut egui::Ui| {
                ui.heading("Live Connections");
                ui.separator();

                let visible: Vec<LogRow> = {
                    let state = self.state.lock();
                    state.log.iter().rev().take(20).cloned().collect()
                };

                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .show(ui, |ui: &mut egui::Ui| {
                        for row in visible {
                            let sev_color = match row.severity {
                                Severity::Info => egui::Color32::GREEN,
                                Severity::Warn => egui::Color32::YELLOW,
                                Severity::Alert => egui::Color32::RED,
                            };
                            let proxy = row.proxy.map(|p| p.label()).unwrap_or("    ");
                            let status = row.http_status.map(|s| format!("{s}")).unwrap_or_default();
                            let path = row
                                .http_path
                                .as_deref()
                                .map(|p| if p.len() > 36 { &p[..36] } else { p })
                                .unwrap_or("");
                            ui.horizontal(|ui: &mut egui::Ui| {
                                ui.label(
                                    egui::RichText::new(row.timestamp.format("%H:%M:%S").to_string())
                                        .color(egui::Color32::GRAY),
                                );
                                ui.label(row.src_ip.to_string());
                                ui.label(egui::RichText::new(format!("[{proxy}]")).color(sev_color));
                                ui.label(format!("{:<14}", row.country));
                                ui.label(format!("{:<14}", row.city));
                                ui.label(egui::RichText::new(status).color(sev_color));
                                ui.label(path);
                            });
                        }
                    });
            });

        // --- Central map ---
        egui::CentralPanel::default().show(ui, |ui: &mut egui::Ui| {
            if let Some(tex) = map_texture {
                let available = ui.available_size();
                // Keep the ~2:1 equirectangular aspect ratio and fit inside the panel.
                let tex_size = tex.size_vec2();
                let aspect = tex_size.x / tex_size.y;
                let panel_aspect = available.x / available.y.max(1.0);
                let draw_size = if panel_aspect > aspect {
                    egui::vec2(available.y * aspect, available.y)
                } else {
                    egui::vec2(available.x, available.x / aspect)
                };

                let (rect, _response) = ui.allocate_exact_size(
                    draw_size,
                    egui::Sense::focusable_noninteractive(),
                );
                ui.painter().image(
                    tex.id(),
                    rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );

                // Home pulse overlay.
                let (hx, hy) = crate::ui::map_renderer::latlon_to_pixel(
                    self.home.lat,
                    self.home.lon,
                    tex_size.x as u32,
                    tex_size.y as u32,
                );
                let u = hx as f32 / tex_size.x;
                let v = hy as f32 / tex_size.y;
                let pulse_pos = egui::pos2(
                    rect.min.x + u * rect.width(),
                    rect.min.y + v * rect.height(),
                );
                ui.painter().circle_filled(
                    pulse_pos,
                    6.0,
                    egui::Color32::from_rgb(60, 180, 255),
                );
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
        let bar = egui::Rect::from_min_size(
            egui::pos2(x, rect.max.y - h),
            egui::vec2(step.max(1.0), h),
        );
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

//! World map renderer.
//!
//! Loads a bundled (or user-supplied) equirectangular world map (PNG,
//! JPEG, or SVG),
//! holds an owned `image::RgbaImage` buffer that we mutate each frame,
//! and renders the result through `ratatui-image` so the user gets
//! the best graphics protocol their terminal supports.
//!
//! Per frame we:
//!   1. Copy the base map into a working buffer (cheap; the base
//!      image is shared across frames via persistent state).
//!   2. Re-plot every live `MapDot`, faded by age.
//!   3. Plot a "host location" pulsing indicator for the configured
//!      home coordinates (defaults to 0°N 0°E – user can override).
//!   4. Hand the buffer to the `ratatui-image` picker, which auto-
//!      detects Kitty Graphics → Sixel → half-block fallback.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;


use anyhow::{Context, Result};
use image::{Rgba, RgbaImage};
use once_cell::sync::Lazy;
use parking_lot::{Mutex, RwLock};
use tracing::{info, warn};


/// Source map data.
///
/// - `Raster`: the procedural fallback map.
/// - `Vector`: the bundled Natural Earth GeoJSON land polygons, rendered
///   ourselves with tiny-skia to guarantee an accurate equirectangular
///   projection that matches our marker math.
enum MapSource {
    Raster(RgbaImage),
    Vector {
        /// Parsed GeoJSON polygons (lon, lat) in WGS84.
        features: Vec<geojson::Feature>,
        ocean: Rgba<u8>,
        land: Rgba<u8>,
        /// Cached full-world render at the default size.
        base: RgbaImage,
    },
}

use crate::config::{ColorConfig, Config, MarkerStyle};
use crate::event::Severity;
use crate::ui::app::{MapDot, Panel};

/// Default map: procedural fallback so the dashboard is never blank.
/// Generates a 2048×1024 dark equirectangular world backdrop with a
/// lat/lon graticule and simplified continent silhouettes.
const DEFAULT_MAP_W: u32 = 2048;
const DEFAULT_MAP_H: u32 = 1024;

/// Configure the home location (lat, lon) where connections terminate.
/// When auto-detected, the public IP and a human-readable city/country label
/// are also stored so the UI can display them.
#[derive(Debug, Clone)]
pub struct HomeLocation {
    pub lat: f64,
    pub lon: f64,
    pub ip: Option<std::net::IpAddr>,
    pub label: Option<String>,
}

impl Default for HomeLocation {
    fn default() -> Self {
        Self {
            lat: 0.0,
            lon: 0.0,
            ip: None,
            label: None,
        }
    }
}

/// A country label extracted from the SVG map.
#[derive(Debug, Clone)]
pub struct MapLabel {
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    /// Importance hint for label visibility (larger = more important).
    /// Currently used only for SVG-derived labels; vector labels use a
    /// fixed base size that scales with the GUI zoom level.
    #[allow(dead_code)]
    pub font_size: f32,
}

impl HomeLocation {
    /// One-line description for the map overlay.
    pub fn overlay_text(&self) -> String {
        let coords = format!("{:.2}, {:.2}", self.lat, self.lon);
        match (&self.ip, &self.label) {
            (Some(ip), Some(label)) => format!("🏠 {ip} — {label} ({coords})"),
            (Some(ip), None) => format!("🏠 {ip} ({coords})"),
            (None, Some(label)) => format!("🏠 {label} ({coords})"),
            (None, None) => format!("🏠 home ({coords})"),
        }
    }
}

/// Viewport for zoomed/panned map rendering.  When `None`, the full world is
/// drawn.  `zoom = 1.0` is full world; larger values zoom in.  The center
/// lat/lon stays anchored in the middle of the output image.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    pub zoom: f64,
    pub center_lat: f64,
    pub center_lon: f64,
}

impl Viewport {
    /// Convenience constructor for the full-world view.
    pub fn full_world() -> Self {
        Self {
            zoom: 1.0,
            center_lat: 0.0,
            center_lon: 0.0,
        }
    }

    /// Visible longitude range (min, max) for this viewport.
    ///
    /// The map uses an equirectangular projection with a natural 2:1 aspect
    /// ratio (360° of longitude for every 180° of latitude).  Zooming scales
    /// both axes uniformly, so the visible ranges are derived from `zoom`
    /// directly, not from the output image size.
    fn lon_range(&self, _w: u32, _h: u32) -> (f64, f64) {
        if self.zoom <= 1.0 {
            return (-180.0, 180.0);
        }
        let half_lon = (180.0 / self.zoom).min(180.0);
        (self.center_lon - half_lon, self.center_lon + half_lon)
    }

    /// Visible latitude range (min, max) for this viewport.
    fn lat_range(&self, _w: u32, _h: u32) -> (f64, f64) {
        if self.zoom <= 1.0 {
            return (-90.0, 90.0);
        }
        let half_lat = (90.0 / self.zoom).min(90.0);
        let c = self.center_lat.clamp(-90.0 + half_lat, 90.0 - half_lat);
        (c - half_lat, c + half_lat)
    }

    /// Is the given lat/lon inside the visible window?
    pub fn contains(&self, lat: f64, lon: f64, w: u32, h: u32) -> bool {
        let (lat_min, lat_max) = self.lat_range(w, h);
        let (lon_min, lon_max) = self.lon_range(w, h);
        let normalized_lon = ((lon + 180.0) % 360.0 + 360.0) % 360.0 - 180.0;
        lat >= lat_min && lat <= lat_max && normalized_lon >= lon_min && normalized_lon <= lon_max
    }

    /// Map a lat/lon to output image pixels for this viewport.
    pub fn latlon_to_pixel(&self, lat: f64, lon: f64, w: u32, h: u32) -> (i32, i32) {
        let (lat_min, lat_max) = self.lat_range(w, h);
        let (lon_min, lon_max) = self.lon_range(w, h);
        let x_norm = if self.zoom <= 1.0 {
            (lon + 180.0) / 360.0
        } else {
            ((lon - lon_min) / (lon_max - lon_min)).clamp(0.0, 1.0)
        };
        let y_norm = ((lat_max - lat) / (lat_max - lat_min)).clamp(0.0, 1.0);
        let x = ((x_norm * w as f64) as i32).clamp(0, w as i32 - 1);
        let y = ((y_norm * h as f64) as i32).clamp(0, h as i32 - 1);
        (x, y)
    }
}

/// Renderer state. Holds the working image buffer; `ratatui-image`'s
/// picker calls `image()` each frame.
pub struct MapRenderer {
    /// Persistent working buffer – re-cleared from `base` each frame.
    work: Mutex<RgbaImage>,
    /// Base layer (read-only) plus optional SVG vector source for crisp zoom.
    source: MapSource,
    /// Pulse phase (0..=2π) for the host indicator.
    pulse: Mutex<f64>,
    /// Last rendered-at – used to advance pulse.
    last_tick: Mutex<Instant>,
    /// Shared configuration; colors + TTL are read each frame so hot-reload
    /// works without rebuilding the cached base map.
    config: Arc<RwLock<Config>>,
    /// Country labels extracted from the SVG map (empty for raster-only maps).
    country_labels: Vec<MapLabel>,
    /// Cached base map (coastlines + ocean) for the current viewport.  Only the
    /// dots / arcs / home-pulse animate each frame, so re-rasterizing the world
    /// polygons on every tick is wasted work — this is the main GUI cost when
    /// zoomed in.  Invalidated when the viewport, output size or ocean color
    /// changes (the latter so config hot-reload of `colors.ocean` still applies).
    cached_base: Mutex<Option<CachedBase>>,
}

/// Cached rasterized base layer, keyed by viewport + size + ocean fill color.
struct CachedBase {
    viewport: Viewport,
    w: u32,
    h: u32,
    fill: [u8; 4],
    image: RgbaImage,
}

/// Load the bundled Natural Earth GeoJSON land polygons and build a
/// geographic vector map source.
fn load_geojson_source(
    geojson_data: &[u8],
    ocean: &Rgba<u8>,
    land: &Rgba<u8>,
) -> Result<(MapSource, Vec<MapLabel>)> {
    let collection = geojson::GeoJson::from_reader(geojson_data).context("parsing GeoJSON map")?;
    let features = match collection {
        geojson::GeoJson::FeatureCollection(fc) => fc.features,
        _ => anyhow::bail!("GeoJSON map must be a FeatureCollection"),
    };

    let base = render_geojson_world(&features, ocean, land, DEFAULT_MAP_W, DEFAULT_MAP_H)
        .context("rendering GeoJSON base map")?;

    let labels = vector_country_labels();

    Ok((
        MapSource::Vector {
            features,
            ocean: *ocean,
            land: *land,
            base,
        },
        labels,
    ))
}

/// Country labels for the vector map, taken from the hard-coded centroid table.
fn vector_country_labels() -> Vec<MapLabel> {
    let mut labels = Vec::new();
    for (&code, &(lat, lon)) in COUNTRY_CODE_CENTROIDS.iter() {
        let name = country_code_name(code)
            .map(|s| s.to_string())
            .unwrap_or_else(|| code.to_ascii_uppercase());
        labels.push(MapLabel {
            name,
            lat,
            lon,
            font_size: 12.0,
        });
    }
    labels
}

impl MapRenderer {
    /// Country labels parsed from the map, if any.
    pub fn country_labels(&self) -> &[MapLabel] {
        &self.country_labels
    }


    /// Load the bundled Natural Earth GeoJSON map and prepare the working buffer.
    pub fn load(config: Arc<RwLock<Config>>) -> Result<Arc<Self>> {
        let colors = config.read().colors.clone();
        let ocean = colors.ocean.to_rgba(255);
        let land = colors.land.to_rgba(255);

        let (source, country_labels) = {
            const BUNDLED_GEOJSON: &[u8] =
                include_bytes!("../../assets/natural-earth-110m-land.geojson");
            match load_geojson_source(BUNDLED_GEOJSON, &ocean, &land) {
                Ok(result) => result,
                Err(e) => {
                    warn!("failed to load bundled GeoJSON map, falling back to procedural map: {e:?}");
                    return Ok(Arc::new(Self {
                        work: Mutex::new(default_map_image(&colors)),
                        source: MapSource::Raster(default_map_image(&colors)),
                        pulse: Mutex::new(0.0),
                        last_tick: Mutex::new(Instant::now()),
                        config,
                        country_labels: Vec::new(),
                        cached_base: Mutex::new(None),
                    }));
                }
            }
        };

        let work = source.base_raster().clone();
        info!("map loaded: {}x{}", work.width(), work.height());

        Ok(Arc::new(Self {
            work: Mutex::new(work),
            source,
            pulse: Mutex::new(0.0),
            last_tick: Mutex::new(Instant::now()),
            config,
            country_labels,
            cached_base: Mutex::new(None),
        }))
    }

    /// Re-render the working buffer using the current active dots.
    /// `viewport` is optional; when `None` the full world is rendered (TUI).
    pub fn redraw(
        &self,
        dots: &[MapDot],
        home_dot: &HomeLocation,
        lines_enabled: bool,
        viewport: Option<Viewport>,
    ) -> RgbaImage {
        let cfg = self.config.read().clone();
        let colors = cfg.colors.clone();
        let ttl = cfg.marker_ttl();
        let vp = viewport.unwrap_or_else(Viewport::full_world);

        let mut work = self.work.lock();
        let w = work.width();
        let h = work.height();
        let ocean_fill = colors.ocean.to_rgba(255);

        // The base map (coastlines + ocean) only changes when the viewport,
        // output size or ocean fill color changes — not on every animation
        // tick.  Cache it so we avoid re-rasterizing the world polygons every
        // frame (the dominant GUI cost when zoomed in).  Each frame we just
        // blit the cached base into the working buffer, then draw the animated
        // dots / arcs / home-pulse on top.
        {
            let mut cache = self.cached_base.lock();
            let stale = match cache.as_ref() {
                None => true,
                Some(c) => c.viewport != vp || c.w != w || c.h != h || c.fill != ocean_fill.0,
            };
            if stale {
                let mut base_img = RgbaImage::new(w, h);
                render_base_viewport(&self.source, &mut base_img, &vp, &ocean_fill);
                *cache = Some(CachedBase {
                    viewport: vp,
                    w,
                    h,
                    fill: ocean_fill.0,
                    image: base_img,
                });
            }
            let base = &cache.as_ref().expect("base layer just ensured").image;
            // Same dimensions as `work` by construction; blit is a fast memcpy
            // that reuses the working buffer's allocation (no re-rasterize).
            work.copy_from_slice(base);
        }

        // Compute pulse advance
        let mut pulse = self.pulse.lock();
        let mut last = self.last_tick.lock();
        let elapsed = last.elapsed();
        *last = Instant::now();
        *pulse = (*pulse + elapsed.as_secs_f64() * 2.0) % (2.0 * std::f64::consts::PI);
        let pulse_value = *pulse;
        drop(last);
        drop(pulse);

        // Home pixel is used for both the marker and connection lines.
        let (hx, hy) = vp.latlon_to_pixel(home_dot.lat, home_dot.lon, w, h);
        let home_visible = vp.contains(home_dot.lat, home_dot.lon, w, h);

        let ttl_secs = ttl.as_secs_f64();
        let now = Instant::now();

        // Draw animated parabolic connection arcs first (under markers).
        if lines_enabled {
            let base_color = cfg.connection_lines.color;
            let glow = cfg.connection_lines.glow_size as i32;
            for d in dots {
                let age = now.duration_since(d.created_at).as_secs_f64();
                let alpha = 1.0 - (age / ttl_secs).clamp(0.0, 1.0);
                if alpha <= 0.0 {
                    continue;
                }
                let (px, py) = vp.latlon_to_pixel(d.lat, d.lon, w, h);
                // Keep the arc only if some part of it is on-screen.  We do NOT
                // cull on the target alone — an arc to an off-screen node is
                // still partly visible when you zoom in near home, and the
                // per-pixel clip in plot_line_collect truncates it to bounds.
                if !arc_intersects_viewport(hx, hy, px, py, w, h) {
                    continue;
                }
                // Fade the arc with its dot, and animate it growing from home
                // toward the node over the first LINE_DRAW_SECS of its life.
                let line_color = base_color.to_rgba((alpha * 220.0) as u8);
                let progress = line_draw_progress(age);
                draw_connection_line(&mut work, hx, hy, px, py, line_color, glow, progress);
            }
        }

        // Plot each dot, faded by age.  The outer halo keeps the marker visible
        // after the high-resolution map is downscaled to the terminal/GUI.
        for d in dots {
            if !vp.contains(d.lat, d.lon, w, h) {
                continue;
            }
            let age = now.duration_since(d.created_at).as_secs_f64();
            let alpha = 1.0 - (age / ttl_secs).clamp(0.0, 1.0);
            if alpha <= 0.0 {
                continue;
            }
            let (px, py) = vp.latlon_to_pixel(d.lat, d.lon, w, h);
            let base_color = match d.severity {
                Severity::Info => colors.info,
                Severity::Warn => colors.warn,
                Severity::Alert => colors.alert,
            };
            let color = base_color.to_rgba((alpha * 255.0) as u8);
            let halo_radius = (cfg.marker_size as i32 * 3).clamp(6, 36);
            plot_outer_halo(&mut work, px, py, color, halo_radius);
            plot_glow(&mut work, px, py, color);
            plot_marker(
                &mut work, px, py, color, cfg.marker_style, cfg.marker_size,
            );
        }

        // Draw persistent home marker last so it stays readable on top of
        // transient dots and connection lines.
        if home_visible {
            let home_color = colors.home.to_rgba(255);
            let home_halo_radius = (cfg.map.home.marker_size as i32 * 4).clamp(12, 64);
            plot_outer_halo(&mut work, hx, hy, home_color, home_halo_radius);
            plot_glow(&mut work, hx, hy, home_color);
            let pulse_glow = colors.home.to_rgba(((pulse_value.sin() + 1.0) * 80.0 + 120.0) as u8);
            plot_glow(&mut work, hx, hy, pulse_glow);
            // Solid circular glow so the home marker is visually dominant.
            let home_glow_radius = (cfg.map.home.marker_size as i32 * 2).clamp(6, 32);
            for r in -home_glow_radius..=home_glow_radius {
                for c in -home_glow_radius..=home_glow_radius {
                    if r * r + c * c > home_glow_radius * home_glow_radius {
                        continue;
                    }
                    let alpha = (1.0 - (r * r + c * c) as f32 / (home_glow_radius * home_glow_radius) as f32) * 0.45;
                    let gx = hx + c;
                    let gy = hy + r;
                    if gx >= 0 && gy >= 0 && gx < work.width() as i32 && gy < work.height() as i32 {
                        let gc = Rgba([home_color.0[0], home_color.0[1], home_color.0[2], (alpha * 255.0) as u8]);
                        blend_pixel(&mut work, gx as u32, gy as u32, gc);
                    }
                }
            }
            plot_marker(
                &mut work,
                hx,
                hy,
                home_color,
                cfg.map.home.marker_style,
                cfg.map.home.marker_size,
            );
        }

        work.clone()
    }
}

/// Convert an `image::Rgba` pixel to a `tiny_skia::Color` for filling.
fn tiny_skia_color(c: Rgba<u8>) -> resvg::tiny_skia::Color {
    resvg::tiny_skia::Color::from_rgba(c.0[0] as f32 / 255.0, c.0[1] as f32 / 255.0, c.0[2] as f32 / 255.0, c.0[3] as f32 / 255.0)
        .unwrap_or(resvg::tiny_skia::Color::BLACK)
}

/// Convert latitude/longitude (degrees) to pixel (x, y) on an
/// equirectangular projection.
pub fn latlon_to_pixel(lat: f64, lon: f64, w: u32, h: u32) -> (i32, i32) {
    let x_norm = (lon + 180.0) / 360.0;
    let y_norm = (90.0 - lat) / 180.0;
    let x = ((x_norm * w as f64) as i32).clamp(0, w as i32 - 1);
    let y = ((y_norm * h as f64) as i32).clamp(0, h as i32 - 1);
    (x, y)
}

/// Approximate geographic centroids (latitude, longitude) for country codes
/// used by the bundled world map.  The SVG's own label coordinates are wrong
/// for several small European states and some shapes are missing entirely
/// (Luxembourg has no path), so a small table gives reliable label positions.
static COUNTRY_CODE_CENTROIDS: Lazy<HashMap<&str, (f64, f64)>> = Lazy::new(|| {
    [
        // Europe
        ("ad", (42.55, 1.60)),
        ("al", (41.15, 20.17)),
        ("at", (47.52, 14.55)),
        ("ba", (44.17, 17.79)),
        ("be", (50.50, 4.47)),
        ("bg", (42.73, 25.49)),
        ("by", (53.71, 27.95)),
        ("ch", (46.82, 8.23)),
        ("cz", (49.82, 15.47)),
        ("de", (51.17, 10.45)),
        ("dk", (56.26, 9.50)),
        ("ee", (58.60, 25.01)),
        ("es", (40.46, -3.75)),
        ("fi", (61.92, 25.75)),
        ("fr", (46.23, 2.21)),
        ("gb", (54.00, -2.00)),
        ("gr", (39.07, 21.82)),
        ("hr", (45.10, 15.20)),
        ("hu", (47.16, 19.50)),
        ("ie", (53.41, -8.24)),
        ("is", (64.96, -19.02)),
        ("it", (41.87, 12.57)),
        ("li", (47.14, 9.55)),
        ("lt", (55.17, 23.88)),
        ("lu", (49.82, 6.13)),
        ("lv", (56.88, 24.60)),
        ("md", (47.41, 28.37)),
        ("me", (42.71, 19.37)),
        ("mk", (41.61, 21.75)),
        ("mt", (35.94, 14.38)),
        ("nl", (52.13, 5.29)),
        ("no", (60.47, 8.47)),
        ("pl", (51.92, 19.13)),
        ("pt", (39.40, -8.22)),
        ("ro", (45.94, 24.97)),
        ("rs", (44.02, 21.01)),
        ("se", (60.13, 18.64)),
        ("si", (46.15, 14.99)),
        ("sk", (48.67, 19.70)),
        ("ua", (48.38, 31.18)),
        ("va", (41.90, 12.45)),
        // Large / commonly labelled world countries
        ("ae", (23.42, 53.85)),
        ("af", (33.94, 67.71)),
        ("am", (40.07, 45.04)),
        ("ao", (-11.20, 17.87)),
        ("ar", (-38.42, -63.62)),
        ("au", (-25.27, 133.78)),
        ("az", (40.14, 47.58)),
        ("bd", (23.68, 90.36)),
        ("bo", (-16.29, -63.59)),
        ("br", (-14.24, -53.18)),
        ("bt", (27.51, 90.43)),
        ("bw", (-22.33, 24.68)),
        ("ca", (56.13, -106.35)),
        ("cd", (-4.04, 21.76)),
        ("cf", (6.61, 20.94)),
        ("cg", (-0.23, 15.83)),
        ("cl", (-35.68, -71.54)),
        ("cm", (7.37, 12.35)),
        ("cn", (35.86, 104.19)),
        ("co", (4.57, -74.30)),
        ("cr", (9.75, -83.75)),
        ("cu", (21.52, -77.78)),
        ("cy", (35.13, 33.43)),
        ("dj", (11.83, 42.59)),
        ("do", (18.74, -70.16)),
        ("dz", (28.03, 1.66)),
        ("ec", (-1.83, -78.18)),
        ("eg", (26.82, 30.80)),
        ("er", (15.18, 39.78)),
        ("et", (9.15, 40.49)),
        ("fj", (-17.71, 178.07)),
        ("ga", (-0.80, 11.61)),
        ("gh", (7.95, -1.02)),
        ("gl", (71.71, -42.60)),
        ("gm", (13.44, -15.31)),
        ("gn", (9.95, -9.70)),
        ("gq", (1.65, 10.27)),
        ("gt", (15.78, -90.23)),
        ("gy", (4.86, -58.93)),
        ("hn", (15.20, -86.24)),
        ("ht", (18.97, -72.29)),
        ("id", (-0.79, 113.92)),
        ("il", (31.05, 34.85)),
        ("in", (20.59, 78.96)),
        ("iq", (33.22, 43.68)),
        ("ir", (32.43, 53.69)),
        ("jm", (18.11, -77.30)),
        ("jo", (30.59, 36.24)),
        ("jp", (36.20, 138.25)),
        ("ke", (-0.02, 37.91)),
        ("kg", (41.20, 74.77)),
        ("kh", (12.57, 104.99)),
        ("kp", (40.34, 127.51)),
        ("kr", (35.91, 127.77)),
        ("kz", (48.02, 66.92)),
        ("la", (19.86, 102.50)),
        ("lb", (33.85, 35.86)),
        ("lk", (7.87, 80.77)),
        ("lr", (6.43, -9.43)),
        ("ls", (-29.61, 28.23)),
        ("ly", (26.34, 17.23)),
        ("ma", (31.79, -7.09)),
        ("mg", (-18.77, 46.87)),
        ("ml", (17.57, -3.99)),
        ("mm", (21.91, 95.96)),
        ("mn", (46.86, 103.85)),
        ("mr", (21.01, -10.94)),
        ("mw", (-13.25, 34.30)),
        ("mx", (23.63, -102.55)),
        ("my", (4.21, 101.98)),
        ("mz", (-18.67, 35.53)),
        ("na", (-22.96, 18.49)),
        ("ne", (17.61, 8.08)),
        ("ng", (9.08, 8.68)),
        ("ni", (12.87, -85.21)),
        ("np", (28.39, 84.12)),
        ("nz", (-40.90, 174.89)),
        ("om", (21.51, 55.92)),
        ("pa", (8.54, -80.78)),
        ("pe", (-9.19, -75.02)),
        ("pg", (-6.31, 143.96)),
        ("ph", (12.88, 121.77)),
        ("pk", (30.38, 69.35)),
        ("pr", (18.22, -66.59)),
        ("ps", (31.95, 35.23)),
        ("py", (-23.44, -58.44)),
        ("qa", (25.35, 51.18)),
        ("ru", (61.52, 105.32)),
        ("sa", (23.89, 45.08)),
        ("sb", (-9.65, 160.16)),
        ("sd", (12.86, 30.22)),
        ("sn", (14.50, -14.45)),
        ("so", (5.15, 46.20)),
        ("sr", (3.92, -56.02)),
        ("ss", (6.88, 31.31)),
        ("sv", (13.79, -88.90)),
        ("sy", (34.80, 38.99)),
        ("sz", (-26.52, 31.47)),
        ("td", (15.45, 18.73)),
        ("tg", (8.62, 0.82)),
        ("th", (15.87, 100.99)),
        ("tj", (38.86, 71.28)),
        ("tl", (-8.87, 125.73)),
        ("tn", (33.89, 9.56)),
        ("tr", (38.96, 35.24)),
        ("tz", (-6.37, 34.89)),
        ("ug", (1.37, 32.29)),
        ("us", (37.09, -95.71)),
        ("uy", (-32.52, -55.77)),
        ("uz", (41.38, 64.59)),
        ("ve", (6.42, -66.59)),
        ("vn", (14.06, 108.28)),
        ("ws", (-13.76, -172.10)),
        ("ye", (15.55, 48.52)),
        ("za", (-30.56, 22.94)),
        ("zm", (-13.13, 27.85)),
        ("zw", (-19.02, 29.15)),
    ]
    .iter()
    .copied()
    .collect()
});

/// Human-readable country names keyed by the same ISO country codes used in
/// `COUNTRY_CODE_CENTROIDS`.  Used by the vector map source so labels are
/// full names rather than two-letter codes.
static COUNTRY_CODE_NAMES: Lazy<HashMap<&str, &str>> = Lazy::new(|| {
    [
        ("ad", "Andorra"),
        ("ae", "United Arab Emirates"),
        ("af", "Afghanistan"),
        ("al", "Albania"),
        ("am", "Armenia"),
        ("ao", "Angola"),
        ("ar", "Argentina"),
        ("at", "Austria"),
        ("au", "Australia"),
        ("az", "Azerbaijan"),
        ("ba", "Bosnia and Herzegovina"),
        ("bd", "Bangladesh"),
        ("be", "Belgium"),
        ("bg", "Bulgaria"),
        ("bo", "Bolivia"),
        ("br", "Brazil"),
        ("bt", "Bhutan"),
        ("bw", "Botswana"),
        ("by", "Belarus"),
        ("ca", "Canada"),
        ("cd", "DR Congo"),
        ("cf", "Central African Republic"),
        ("cg", "Republic of the Congo"),
        ("ch", "Switzerland"),
        ("cl", "Chile"),
        ("cm", "Cameroon"),
        ("cn", "China"),
        ("co", "Colombia"),
        ("cr", "Costa Rica"),
        ("cu", "Cuba"),
        ("cy", "Cyprus"),
        ("cz", "Czech Republic"),
        ("de", "Germany"),
        ("dk", "Denmark"),
        ("dj", "Djibouti"),
        ("do", "Dominican Republic"),
        ("dz", "Algeria"),
        ("ec", "Ecuador"),
        ("ee", "Estonia"),
        ("eg", "Egypt"),
        ("er", "Eritrea"),
        ("es", "Spain"),
        ("et", "Ethiopia"),
        ("fi", "Finland"),
        ("fj", "Fiji"),
        ("fr", "France"),
        ("ga", "Gabon"),
        ("gb", "United Kingdom"),
        ("gh", "Ghana"),
        ("gl", "Greenland"),
        ("gm", "Gambia"),
        ("gn", "Guinea"),
        ("gq", "Equatorial Guinea"),
        ("gr", "Greece"),
        ("gt", "Guatemala"),
        ("gy", "Guyana"),
        ("hn", "Honduras"),
        ("hr", "Croatia"),
        ("ht", "Haiti"),
        ("hu", "Hungary"),
        ("id", "Indonesia"),
        ("ie", "Ireland"),
        ("il", "Israel"),
        ("in", "India"),
        ("iq", "Iraq"),
        ("ir", "Iran"),
        ("is", "Iceland"),
        ("it", "Italy"),
        ("jm", "Jamaica"),
        ("jo", "Jordan"),
        ("jp", "Japan"),
        ("ke", "Kenya"),
        ("kg", "Kyrgyzstan"),
        ("kh", "Cambodia"),
        ("kp", "North Korea"),
        ("kr", "South Korea"),
        ("kz", "Kazakhstan"),
        ("la", "Laos"),
        ("lb", "Lebanon"),
        ("li", "Liechtenstein"),
        ("lk", "Sri Lanka"),
        ("lr", "Liberia"),
        ("ls", "Lesotho"),
        ("lt", "Lithuania"),
        ("lu", "Luxembourg"),
        ("lv", "Latvia"),
        ("ly", "Libya"),
        ("ma", "Morocco"),
        ("md", "Moldova"),
        ("me", "Montenegro"),
        ("mg", "Madagascar"),
        ("mk", "North Macedonia"),
        ("ml", "Mali"),
        ("mm", "Myanmar"),
        ("mn", "Mongolia"),
        ("mr", "Mauritania"),
        ("mt", "Malta"),
        ("mw", "Malawi"),
        ("mx", "Mexico"),
        ("my", "Malaysia"),
        ("mz", "Mozambique"),
        ("na", "Namibia"),
        ("ne", "Niger"),
        ("ng", "Nigeria"),
        ("ni", "Nicaragua"),
        ("nl", "Netherlands"),
        ("no", "Norway"),
        ("np", "Nepal"),
        ("nz", "New Zealand"),
        ("om", "Oman"),
        ("pa", "Panama"),
        ("pe", "Peru"),
        ("pg", "Papua New Guinea"),
        ("ph", "Philippines"),
        ("pk", "Pakistan"),
        ("pl", "Poland"),
        ("pr", "Puerto Rico"),
        ("ps", "Palestine"),
        ("pt", "Portugal"),
        ("py", "Paraguay"),
        ("qa", "Qatar"),
        ("ro", "Romania"),
        ("rs", "Serbia"),
        ("ru", "Russia"),
        ("sa", "Saudi Arabia"),
        ("sb", "Solomon Islands"),
        ("sd", "Sudan"),
        ("se", "Sweden"),
        ("si", "Slovenia"),
        ("sk", "Slovakia"),
        ("sn", "Senegal"),
        ("so", "Somalia"),
        ("sr", "Suriname"),
        ("ss", "South Sudan"),
        ("sv", "El Salvador"),
        ("sy", "Syria"),
        ("sz", "Eswatini"),
        ("td", "Chad"),
        ("tg", "Togo"),
        ("th", "Thailand"),
        ("tj", "Tajikistan"),
        ("tl", "Timor-Leste"),
        ("tn", "Tunisia"),
        ("tr", "Turkey"),
        ("tz", "Tanzania"),
        ("ua", "Ukraine"),
        ("ug", "Uganda"),
        ("us", "United States"),
        ("uy", "Uruguay"),
        ("uz", "Uzbekistan"),
        ("va", "Vatican City"),
        ("ve", "Venezuela"),
        ("vn", "Vietnam"),
        ("ws", "Samoa"),
        ("ye", "Yemen"),
        ("za", "South Africa"),
        ("zm", "Zambia"),
        ("zw", "Zimbabwe"),
    ]
    .iter()
    .copied()
    .collect()
});

fn country_code_name(code: &str) -> Option<&str> {
    COUNTRY_CODE_NAMES.get(code).copied()
}

/// Draw a single pixel plus a soft halo (5×5 gaussian-ish glow).
/// Alpha is composited over the base map instead of overwriting it.
fn plot_glow(img: &mut RgbaImage, cx: i32, cy: i32, c: Rgba<u8>) {
    let w = img.width() as i32;
    let h = img.height() as i32;
    let ring: &[(i32, i32, f32)] = &[
        (0, 0, 1.00),
        (-1, 0, 0.60),
        (1, 0, 0.60),
        (0, -1, 0.60),
        (0, 1, 0.60),
        (-1, -1, 0.35),
        (1, -1, 0.35),
        (-1, 1, 0.35),
        (1, 1, 0.35),
        (0, -2, 0.25),
        (0, 2, 0.25),
        (-2, 0, 0.25),
        (2, 0, 0.25),
        (-2, -1, 0.15),
        (-2, 1, 0.15),
        (2, -1, 0.15),
        (2, 1, 0.15),
        (-1, -2, 0.15),
        (1, -2, 0.15),
        (-1, 2, 0.15),
        (1, 2, 0.15),
    ];
    for (dx, dy, w_alpha) in ring {
        let nx = cx + dx;
        let ny = cy + dy;
        if nx < 0 || ny < 0 || nx >= w || ny >= h {
            continue;
        }
        let p = img.get_pixel_mut(nx as u32, ny as u32);
        let src_a = (c.0[3] as f32) * *w_alpha / 255.0;
        let dst_a = p.0[3] as f32 / 255.0;
        let out_a = src_a + dst_a * (1.0 - src_a);
        if out_a > 0.0 {
            let inv_src = 1.0 - src_a;
            p.0 = [
                (((c.0[0] as f32) * src_a + (p.0[0] as f32) * dst_a * inv_src) / out_a).min(255.0)
                    as u8,
                (((c.0[1] as f32) * src_a + (p.0[1] as f32) * dst_a * inv_src) / out_a).min(255.0)
                    as u8,
                (((c.0[2] as f32) * src_a + (p.0[2] as f32) * dst_a * inv_src) / out_a).min(255.0)
                    as u8,
                (out_a * 255.0).min(255.0) as u8,
            ];
        }
    }
}

/// Draw a large, dim outer halo around a marker so it remains visible after the
/// high-resolution map is downscaled for a terminal or GUI viewport.
fn plot_outer_halo(img: &mut RgbaImage, cx: i32, cy: i32, c: Rgba<u8>, radius: i32) {
    if radius <= 0 {
        return;
    }
    let w = img.width() as i32;
    let h = img.height() as i32;
    let r2 = radius * radius;
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let d2 = dx * dx + dy * dy;
            if d2 > r2 {
                continue;
            }
            let nx = cx + dx;
            let ny = cy + dy;
            if nx < 0 || ny < 0 || nx >= w || ny >= h {
                continue;
            }
            let falloff = 1.0 - d2 as f32 / r2 as f32;
            let alpha = (c.0[3] as f32) * falloff * 0.30 / 255.0;
            if alpha <= 0.0 {
                continue;
            }
            let halo = Rgba([c.0[0], c.0[1], c.0[2], (alpha * 255.0) as u8]);
            blend_pixel(img, nx as u32, ny as u32, halo);
        }
    }
}

/// Draw a visible marker shape at the given pixel.  `size` is the radius /
/// arm length in pixels.  The color is already alpha-faded.
fn plot_marker(img: &mut RgbaImage, cx: i32, cy: i32, c: Rgba<u8>, style: MarkerStyle, size: u8) {
    let w = img.width() as i32;
    let h = img.height() as i32;
    let radius = size as i32;

    let put = |img: &mut RgbaImage, x: i32, y: i32| {
        if x < 0 || y < 0 || x >= w || y >= h {
            return;
        }
        blend_pixel(img, x as u32, y as u32, c);
    };

    match style {
        MarkerStyle::Dot => {
            // Already drawn by plot_glow; nothing extra.
        }
        MarkerStyle::Ring => {
            // Midpoint circle algorithm.
            let mut x = radius;
            let mut y = 0;
            let mut err = 0;
            while x >= y {
                for (dx, dy) in [
                    (x, y),
                    (y, x),
                    (-y, x),
                    (-x, y),
                    (-x, -y),
                    (-y, -x),
                    (y, -x),
                    (x, -y),
                ] {
                    put(img, cx + dx, cy + dy);
                }
                if err <= 0 {
                    y += 1;
                    err += 2 * y + 1;
                }
                if err > 0 {
                    x -= 1;
                    err -= 2 * x + 1;
                }
            }
        }
        MarkerStyle::Cross => {
            for d in -radius..=radius {
                put(img, cx + d, cy);
                put(img, cx, cy + d);
            }
        }
        MarkerStyle::X => {
            for d in -radius..=radius {
                put(img, cx + d, cy + d);
                put(img, cx + d, cy - d);
            }
        }
    }
}

/// Render the base map into `work`, cropped to the requested viewport.
///
/// For SVG sources we re-rasterize the vector map at the viewport's native
/// pixel size so zooming stays crisp.  For raster-only sources we fall back to
/// nearest-neighbor sampling of the cached low-resolution bitmap.
fn render_base_viewport(
    source: &MapSource,
    work: &mut RgbaImage,
    vp: &Viewport,
    fill: &Rgba<u8>,
) {
    let out_w = work.width();
    let out_h = work.height();

    // For unzoomed frames reuse the cached low-res base image.
    if vp.zoom <= 1.0 {
        let base = source.base_raster();
        let base_w = base.width();
        let base_h = base.height();
        for y in 0..out_h {
            let src_y = (y as f64 / out_h as f64 * base_h as f64) as u32;
            for x in 0..out_w {
                let src_x = (x as f64 / out_w as f64 * base_w as f64) as u32;
                work.put_pixel(x, y, *base.get_pixel(src_x.min(base_w - 1), src_y.min(base_h - 1)));
            }
        }
        return;
    }

    // Zoomed: for vector sources, render the viewport region at high resolution.
    if let MapSource::Vector { features, ocean, land, .. } = source {
        if let Some(rendered) = render_geojson_viewport(features, ocean, land, vp, out_w, out_h) {
            *work = rendered;
            return;
        }
    }

    // Fallback for non-SVG/raster sources: nearest-neighbor crop from cache.
    let base = source.base_raster();
    let base_w = base.width();
    let base_h = base.height();
    let (lat_min, lat_max) = vp.lat_range(out_w, out_h);
    let (lon_min, lon_max) = vp.lon_range(out_w, out_h);

    for y in 0..out_h {
        let lat = lat_max - (y as f64 / out_h as f64) * (lat_max - lat_min);
        let src_y = ((90.0 - lat) / 180.0 * base_h as f64) as i32;
        for x in 0..out_w {
            let lon = lon_min + (x as f64 / out_w as f64) * (lon_max - lon_min);
            let src_x = ((lon + 180.0) / 360.0 * base_w as f64) as i32;
            if src_x >= 0 && src_y >= 0 && src_x < base_w as i32 && src_y < base_h as i32 {
                work.put_pixel(x, y, *base.get_pixel(src_x as u32, src_y as u32));
            } else {
                work.put_pixel(x, y, *fill);
            }
        }
    }
}

impl MapSource {
    /// Cached low-resolution raster used for full-world frames and as the
    /// fallback for zoomed raster-only sources.
    fn base_raster(&self) -> &RgbaImage {
        match self {
            MapSource::Raster(img) => img,
            MapSource::Vector { base, .. } => base,
        }
    }
}

/// Render a zoomed viewport of the Natural Earth GeoJSON land polygons directly
/// into the output size.
fn render_geojson_viewport(
    features: &[geojson::Feature],
    ocean: &Rgba<u8>,
    land: &Rgba<u8>,
    vp: &Viewport,
    out_w: u32,
    out_h: u32,
) -> Option<RgbaImage> {
    let (lat_min, lat_max) = vp.lat_range(out_w, out_h);
    let (lon_min, lon_max) = vp.lon_range(out_w, out_h);
    let lon_range = lon_max - lon_min;
    let lat_range = lat_max - lat_min;
    if lon_range <= 0.0 || lat_range <= 0.0 {
        return None;
    }

    let mut pixmap = resvg::tiny_skia::Pixmap::new(out_w, out_h)?;
    pixmap.fill(tiny_skia_color(*ocean));

    let mut paint = resvg::tiny_skia::Paint::default();
    paint.set_color(tiny_skia_color(*land));

    for feature in features {
        if let Some(geom) = &feature.geometry {
            if let Some(path) = geometry_to_viewport_path(geom, lon_min, lat_max, lon_range, lat_range, out_w, out_h) {
                pixmap.as_mut().fill_path(
                    &path,
                    &paint,
                    resvg::tiny_skia::FillRule::Winding,
                    resvg::tiny_skia::Transform::default(),
                    None,
                );
            }
        }
    }

    RgbaImage::from_raw(out_w, out_h, pixmap.data().to_vec())
        .context("unexpected GeoJSON viewport pixmap size")
        .ok()
}

/// Convert a GeoJSON geometry into a tiny-skia path for a lat/lon viewport.
/// Points outside the viewport are clipped by tiny-skia; longitudes that wrap
/// across the antimeridian are skipped so they do not streak across the map.
fn geometry_to_viewport_path(
    geometry: &geojson::Geometry,
    lon_min: f64,
    lat_max: f64,
    lon_range: f64,
    lat_range: f64,
    out_w: u32,
    out_h: u32,
) -> Option<resvg::tiny_skia::Path> {
    match &geometry.value {
        geojson::GeometryValue::Polygon { coordinates } => {
            ring_path(coordinates, lon_min, lat_max, lon_range, lat_range, out_w, out_h)
        }
        geojson::GeometryValue::MultiPolygon { coordinates } => {
            let mut builder = resvg::tiny_skia::PathBuilder::new();
            let mut any = false;
            for polygon in coordinates {
                if let Some(p) = ring_path(polygon, lon_min, lat_max, lon_range, lat_range, out_w, out_h) {
                    // Append each sub-path to the combined path.
                    p.segments().for_each(|seg| match seg {
                        resvg::tiny_skia::PathSegment::MoveTo(p) => { builder.move_to(p.x, p.y); }
                        resvg::tiny_skia::PathSegment::LineTo(p) => { builder.line_to(p.x, p.y); }
                        resvg::tiny_skia::PathSegment::Close => { builder.close(); }
                        _ => {}
                    });
                    any = true;
                }
            }
            if any { builder.finish() } else { None }
        }
        _ => None,
    }
}

/// Build a tiny-skia path from a GeoJSON polygon's rings.
fn ring_path(
    polygon: &[Vec<geojson::Position>],
    lon_min: f64,
    lat_max: f64,
    lon_range: f64,
    lat_range: f64,
    out_w: u32,
    out_h: u32,
) -> Option<resvg::tiny_skia::Path> {
    let mut builder = resvg::tiny_skia::PathBuilder::new();
    let mut any = false;
    for ring in polygon {
        let mut first = true;
        let mut prev_x = 0.0f32;
        let mut prev_y = 0.0f32;
        for coord in ring {
            if coord.len() < 2 {
                continue;
            }
            let (lon, lat) = (coord[0], coord[1]);
            let x = ((lon - lon_min) / lon_range) * out_w as f64;
            let y = ((lat_max - lat) / lat_range) * out_h as f64;
            // Skip segments that wrap more than half the viewport; such jumps
            // usually mean the polygon crosses the antimeridian and the ring was
            // not pre-split, so drawing it would streak across the whole map.
            let dx = (x - prev_x as f64).abs();
            let dy = (y - prev_y as f64).abs();
            if !first && (dx > out_w as f64 * 0.5 || dy > out_h as f64 * 0.5) {
                first = true;
                continue;
            }
            if first {
                builder.move_to(x as f32, y as f32);
            } else {
                builder.line_to(x as f32, y as f32);
            }
            prev_x = x as f32;
            prev_y = y as f32;
            first = false;
            any = true;
        }
        if !first {
            builder.close();
        }
    }
    if any { builder.finish() } else { None }
}

/// Render the full-world Natural Earth GeoJSON land polygons into a cached
/// low-resolution raster used for unzoomed frames and as the fallback base.
fn render_geojson_world(
    features: &[geojson::Feature],
    ocean: &Rgba<u8>,
    land: &Rgba<u8>,
    width: u32,
    height: u32,
) -> Result<RgbaImage> {
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)
        .context("creating GeoJSON render target")?;
    pixmap.fill(tiny_skia_color(*ocean));

    let mut paint = resvg::tiny_skia::Paint::default();
    paint.set_color(tiny_skia_color(*land));

    for feature in features {
        if let Some(geom) = &feature.geometry {
            if let Some(path) = geometry_to_world_path(geom, width, height) {
                pixmap.as_mut().fill_path(
                    &path,
                    &paint,
                    resvg::tiny_skia::FillRule::Winding,
                    resvg::tiny_skia::Transform::default(),
                    None,
                );
            }
        }
    }

    RgbaImage::from_raw(width, height, pixmap.data().to_vec())
        .context("unexpected GeoJSON world pixmap size")
}

/// Convert a GeoJSON geometry into a tiny-skia path for the full world.
fn geometry_to_world_path(
    geometry: &geojson::Geometry,
    width: u32,
    height: u32,
) -> Option<resvg::tiny_skia::Path> {
    match &geometry.value {
        geojson::GeometryValue::Polygon { coordinates } => {
            world_ring_path(coordinates, width, height)
        }
        geojson::GeometryValue::MultiPolygon { coordinates } => {
            let mut builder = resvg::tiny_skia::PathBuilder::new();
            let mut any = false;
            for polygon in coordinates {
                if let Some(p) = world_ring_path(polygon, width, height) {
                    p.segments().for_each(|seg| match seg {
                        resvg::tiny_skia::PathSegment::MoveTo(p) => { builder.move_to(p.x, p.y); }
                        resvg::tiny_skia::PathSegment::LineTo(p) => { builder.line_to(p.x, p.y); }
                        resvg::tiny_skia::PathSegment::Close => { builder.close(); }
                        _ => {}
                    });
                    any = true;
                }
            }
            if any { builder.finish() } else { None }
        }
        _ => None,
    }
}

/// Build a tiny-skia path from a GeoJSON polygon's rings for the full world.
fn world_ring_path(
    polygon: &[Vec<geojson::Position>],
    width: u32,
    height: u32,
) -> Option<resvg::tiny_skia::Path> {
    let mut builder = resvg::tiny_skia::PathBuilder::new();
    let mut any = false;
    for ring in polygon {
        let mut first = true;
        let mut prev_x = 0.0f32;
        let mut prev_y = 0.0f32;
        for coord in ring {
            if coord.len() < 2 {
                continue;
            }
            let (lon, lat) = (coord[0], coord[1]);
            let x = ((lon + 180.0) / 360.0) * width as f64;
            let y = ((90.0 - lat) / 180.0) * height as f64;
            let dx = (x - prev_x as f64).abs();
            let dy = (y - prev_y as f64).abs();
            if !first && (dx > width as f64 * 0.5 || dy > height as f64 * 0.5) {
                first = true;
                continue;
            }
            if first {
                builder.move_to(x as f32, y as f32);
            } else {
                builder.line_to(x as f32, y as f32);
            }
            prev_x = x as f32;
            prev_y = y as f32;
            first = false;
            any = true;
        }
        if !first {
            builder.close();
        }
    }
    if any { builder.finish() } else { None }
}

/// Alpha-blend a single pixel with `c`.  Simpler than `plot_glow` because we
/// do not need the gaussian falloff.
fn blend_pixel(img: &mut RgbaImage, x: u32, y: u32, c: Rgba<u8>) {
    let p = img.get_pixel_mut(x, y);
    let src_a = c.0[3] as f32 / 255.0;
    let dst_a = p.0[3] as f32 / 255.0;
    let out_a = src_a + dst_a * (1.0 - src_a);
    if out_a <= 0.0 {
        return;
    }
    let inv_src = 1.0 - src_a;
    p.0 = [
        (((c.0[0] as f32) * src_a + (p.0[0] as f32) * dst_a * inv_src) / out_a).min(255.0) as u8,
        (((c.0[1] as f32) * src_a + (p.0[1] as f32) * dst_a * inv_src) / out_a).min(255.0) as u8,
        (((c.0[2] as f32) * src_a + (p.0[2] as f32) * dst_a * inv_src) / out_a).min(255.0) as u8,
        (out_a * 255.0).min(255.0) as u8,
    ];
}

/// Helper used by `panels.rs` to determine which panel has focus.
pub fn focus_target(prev: Panel) -> Panel {
    match prev {
        Panel::Map => Panel::Log,
        Panel::Log => Panel::Metrics,
        Panel::Metrics => Panel::Map,
    }
}

/// Build the default world map (dark ocean + graticule + simplified
/// continent silhouettes).
///
/// Equirectangular projection: `lat` ∈ [-90, 90] → y ∈ [H, 0],
/// `lon` ∈ [-180, 180] → x ∈ [0, W]. Continent polygons are coarse —
/// they're hand-rough-sketches, not cartographically accurate, but
/// they resolve to obviously-recognisable landmasses at any zoom.
fn default_map_image(colors: &ColorConfig) -> RgbaImage {
    let w = DEFAULT_MAP_W;
    let h = DEFAULT_MAP_H;
    let mut img = RgbaImage::new(w, h);

    // --- 1. Ocean: deep dark blue with a subtle horizontal vignette.
    let ocean_top = colors.ocean.to_rgba(255);
    let ocean_bot = lighten(&colors.ocean.to_rgba(255), 0.08);
    for y in 0..h {
        let t = y as f32 / h as f32;
        let r = ocean_top.0[0] as f32 * (1.0 - t) + ocean_bot.0[0] as f32 * t;
        let g = ocean_top.0[1] as f32 * (1.0 - t) + ocean_bot.0[1] as f32 * t;
        let b = ocean_top.0[2] as f32 * (1.0 - t) + ocean_bot.0[2] as f32 * t;
        let lat_factor = 1.0 - ((y as f32 / h as f32 - 0.5) * 2.0).abs();
        let v = 1.0 - 0.15 * (1.0 - lat_factor);
        let px = Rgba([(r * v) as u8, (g * v) as u8, (b * v) as u8, 255]);
        for x in 0..w {
            img.put_pixel(x, y, px);
        }
    }

    // --- 2. Sparse graticule: only prime meridian, equator, and 60° lines.
    let grid = Rgba([200, 210, 230, 22]);
    for lon in (-180..=180).step_by(60) {
        let x = (((lon + 180) as f32 / 360.0) * w as f32) as i32;
        if (0..w as i32).contains(&x) {
            for y in 0..h {
                blend(&mut img, x as u32, y, grid);
            }
        }
    }
    for lat in (-60..=60).step_by(60) {
        let y = (((90 - lat) as f32 / 180.0) * h as f32) as i32;
        if (0..h as i32).contains(&y) {
            for x in 0..w {
                blend(&mut img, x, y as u32, grid);
            }
        }
    }

    // --- 3. Simplified continent silhouettes (lat, lon polygons).
    let land = colors.land.to_rgba(255);
    let land_edge = lighten(&colors.land.to_rgba(255), 0.25);
    let continents: &[&[(f32, f32)]] = &[
        // North America
        &[
            (71.0, -156.0),
            (70.0, -141.0),
            (69.0, -120.0),
            (60.0, -94.0),
            (49.0, -94.0),
            (49.0, -123.0),
            (32.0, -117.0),
            (25.0, -110.0),
            (18.0, -97.0),
            (16.0, -88.0),
            (22.0, -82.0),
            (26.0, -81.0),
            (32.0, -80.0),
            (40.0, -74.0),
            (45.0, -67.0),
            (51.0, -56.0),
            (60.0, -64.0),
            (65.0, -78.0),
            (70.0, -90.0),
            (74.0, -95.0),
            (78.0, -120.0),
            (74.0, -140.0),
        ],
        // South America
        &[
            (12.0, -82.0),
            (10.0, -67.0),
            (5.0, -52.0),
            (-5.0, -35.0),
            (-23.0, -41.0),
            (-35.0, -57.0),
            (-55.0, -68.0),
            (-50.0, -73.0),
            (-30.0, -71.0),
            (-18.0, -75.0),
            (-5.0, -81.0),
            (2.0, -78.0),
            (8.0, -77.0),
        ],
        // Europe
        &[
            (71.0, 25.0),
            (70.0, 30.0),
            (60.0, 30.0),
            (55.0, 35.0),
            (45.0, 40.0),
            (40.0, 25.0),
            (37.0, 0.0),
            (43.0, -8.0),
            (50.0, -5.0),
            (58.0, -7.0),
            (60.0, 5.0),
            (58.0, 12.0),
            (55.0, 14.0),
            (54.0, 8.0),
            (60.0, 11.0),
            (65.0, 12.0),
        ],
        // Africa
        &[
            (37.0, -8.0),
            (32.0, 10.0),
            (30.0, 32.0),
            (12.0, 43.0),
            (12.0, 51.0),
            (-2.0, 41.0),
            (-15.0, 40.0),
            (-35.0, 20.0),
            (-34.0, 18.0),
            (-29.0, 16.0),
            (-12.0, 13.0),
            (5.0, 0.0),
            (5.0, -10.0),
            (10.0, -16.0),
            (20.0, -17.0),
            (30.0, -9.0),
        ],
        // Asia (one big blob)
        &[
            (78.0, 60.0),
            (78.0, 140.0),
            (70.0, 165.0),
            (60.0, 170.0),
            (50.0, 142.0),
            (35.0, 140.0),
            (22.0, 120.0),
            (10.0, 105.0),
            (1.0, 103.0),
            (8.0, 80.0),
            (22.0, 68.0),
            (25.0, 55.0),
            (12.0, 43.0),
            (30.0, 32.0),
            (40.0, 26.0),
            (45.0, 40.0),
            (55.0, 50.0),
            (60.0, 55.0),
            (70.0, 60.0),
        ],
        // Australia
        &[
            (-12.0, 130.0),
            (-12.0, 142.0),
            (-18.0, 146.0),
            (-25.0, 153.0),
            (-37.0, 150.0),
            (-38.0, 141.0),
            (-35.0, 135.0),
            (-32.0, 125.0),
            (-22.0, 113.0),
        ],
        // Greenland
        &[
            (83.0, -30.0),
            (80.0, -20.0),
            (70.0, -22.0),
            (60.0, -43.0),
            (70.0, -55.0),
            (78.0, -70.0),
            (82.0, -45.0),
        ],
        // Antarctica (just a strip across the bottom)
        &[
            (-65.0, -180.0),
            (-85.0, -120.0),
            (-80.0, -60.0),
            (-75.0, 0.0),
            (-72.0, 60.0),
            (-68.0, 120.0),
            (-72.0, 180.0),
        ],
    ];
    for poly in continents {
        fill_polygon(&mut img, poly, w, h, land);
        stroke_polygon(&mut img, poly, w, h, land_edge);
    }

    img
}

/// Alpha-blend `c` over `p` using premultiplied-style straight-alpha.
fn blend(img: &mut RgbaImage, x: u32, y: u32, c: Rgba<u8>) {
    let p = img.get_pixel_mut(x, y);
    let src_a = c.0[3] as f32 / 255.0;
    let dst_a = p.0[3] as f32 / 255.0;
    let out_a = src_a + dst_a * (1.0 - src_a);
    if out_a <= 0.0 {
        return;
    }
    let inv_src = 1.0 - src_a;
    p.0 = [
        (((c.0[0] as f32) * src_a + (p.0[0] as f32) * dst_a * inv_src) / out_a) as u8,
        (((c.0[1] as f32) * src_a + (p.0[1] as f32) * dst_a * inv_src) / out_a) as u8,
        (((c.0[2] as f32) * src_a + (p.0[2] as f32) * dst_a * inv_src) / out_a) as u8,
        (out_a * 255.0) as u8,
    ];
}

/// Project (lat, lon) → (x, y) on the default map.
fn project(lat: f32, lon: f32, w: u32, h: u32) -> (i32, i32) {
    let x = ((lon + 180.0) as f32 / 360.0 * w as f32) as i32;
    let y = ((90.0 - lat) as f32 / 180.0 * h as f32) as i32;
    (x.clamp(0, w as i32 - 1), y.clamp(0, h as i32 - 1))
}

/// Scanline-fill a polygon (assumed closed, simple, CCW or CW).
fn fill_polygon(img: &mut RgbaImage, pts: &[(f32, f32)], w: u32, h: u32, color: Rgba<u8>) {
    if pts.len() < 3 {
        return;
    }
    let proj: Vec<(i32, i32)> = pts.iter().map(|&(la, lo)| project(la, lo, w, h)).collect();
    let min_y = proj.iter().map(|p| p.1).min().unwrap().max(0);
    let max_y = proj.iter().map(|p| p.1).max().unwrap().min(h as i32 - 1);
    for y in min_y..=max_y {
        let mut xs: Vec<i32> = Vec::new();
        for i in 0..proj.len() {
            let (x0, y0) = proj[i];
            let (x1, y1) = proj[(i + 1) % proj.len()];
            if (y0 <= y && y1 > y) || (y1 <= y && y0 > y) {
                let t = (y - y0) as f32 / (y1 - y0) as f32;
                xs.push(x0 + ((x1 - x0) as f32 * t) as i32);
            }
        }
        xs.sort_unstable();
        for pair in xs.chunks_exact(2) {
            let (a, b) = (pair[0].max(0), pair[1].min(w as i32 - 1));
            for x in a..=b {
                img.put_pixel(x as u32, y as u32, color);
            }
        }
    }
}

/// 1-px outline of a polygon (Bresenham-style line strip).
fn stroke_polygon(img: &mut RgbaImage, pts: &[(f32, f32)], w: u32, h: u32, color: Rgba<u8>) {
    let proj: Vec<(i32, i32)> = pts.iter().map(|&(la, lo)| project(la, lo, w, h)).collect();
    for i in 0..proj.len() {
        let (x0, y0) = proj[i];
        let (x1, y1) = proj[(i + 1) % proj.len()];
        draw_line(img, x0, y0, x1, y1, w, h, color);
    }
}

/// Bresenham line.
fn draw_line(
    img: &mut RgbaImage,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    w: u32,
    h: u32,
    color: Rgba<u8>,
) {
    let (mut x, mut y) = (x0, y0);
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        if x >= 0 && y >= 0 && x < w as i32 && y < h as i32 {
            img.put_pixel(x as u32, y as u32, color);
        }
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

/// Duration (in seconds) over which a connection arc grows from the home
/// marker out to its target dot.  Short enough to feel snappy, long enough
/// that the "drawing" motion is visible on the map.
const LINE_DRAW_SECS: f64 = 0.7;

/// Ease-out cubic — the arc starts fast from home and decelerates as it
/// approaches the node, like a tracer travelling along the curve.
fn line_draw_progress(age_secs: f64) -> f32 {
    let t = (age_secs / LINE_DRAW_SECS).clamp(0.0, 1.0);
    let eased = 1.0 - (1.0 - t).powi(3);
    eased as f32
}

/// Quadratic-Bézier control point for a parabolic arc from `(x0, y0)` to
/// `(x1, y1)`.  The arc bows "upward" (toward the top of the image, like a
/// flight path on a world map) with a magnitude proportional to the distance,
/// so long connections arc visibly while short ones stay nearly straight.
fn arc_control_point(x0: f32, y0: f32, x1: f32, y1: f32) -> (f32, f32) {
    let mx = (x0 + x1) * 0.5;
    let my = (y0 + y1) * 0.5;
    let dx = x1 - x0;
    let dy = y1 - y0;
    let dist = (dx * dx + dy * dy).sqrt();
    let len = dist.max(1.0);
    // Unit perpendicular to the home→node segment.
    let perp_x = -dy / len;
    let perp_y = dx / len;
    // ~18% of the distance, with a floor so even short hops bow a little.
    let mag = (dist * 0.18).max(8.0);
    // Choose the perpendicular sign that lifts the control point up
    // (smaller y = north on the image), giving a consistent flight-arc look.
    let sign = if perp_y > 0.0 { -1.0 } else { 1.0 };
    (mx + perp_x * mag * sign, my + perp_y * mag * sign)
}

/// Cheap visibility test for a connection arc.  A quadratic Bézier stays
/// within the convex hull of its three control points (home, target, and the
/// bowed control point from [`arc_control_point`]), so if the axis-aligned
/// bounding box of those three points does not intersect the viewport, no
/// pixel of the arc would be written anyway and we can skip it for speed.
///
/// Crucially this still draws the *visible portion* of an arc whose far
/// endpoint is off-screen (e.g. you zoomed in near home and the target node
/// scrolled out of view) — the per-pixel clip in [`plot_line_collect`] then
/// truncates the arc to the image bounds.
fn arc_intersects_viewport(x0: i32, y0: i32, x1: i32, y1: i32, w: u32, h: u32) -> bool {
    let (fx0, fy0) = (x0 as f32, y0 as f32);
    let (fx1, fy1) = (x1 as f32, y1 as f32);
    let (cx, cy) = arc_control_point(fx0, fy0, fx1, fy1);
    let min_x = fx0.min(fx1).min(cx);
    let max_x = fx0.max(fx1).max(cx);
    let min_y = fy0.min(fy1).min(cy);
    let max_y = fy0.max(fy1).max(cy);
    let (wf, hf) = (w as f32, h as f32);
    // Bounding box overlaps [0, w) × [0, h).
    max_x >= 0.0 && min_x < wf && max_y >= 0.0 && min_y < hf
}

/// Bresenham line that both plots the core pixel and records the coordinate
/// so a thin glow can be applied afterwards in a single pass.
fn plot_line_collect(
    img: &mut RgbaImage,
    pixels: &mut Vec<(i32, i32)>,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    w: u32,
    h: u32,
    color: Rgba<u8>,
) {
    let (mut x, mut y) = (x0, y0);
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        if x >= 0 && y >= 0 && x < w as i32 && y < h as i32 {
            img.put_pixel(x as u32, y as u32, color);
            pixels.push((x, y));
        }
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

/// Animated parabolic (quadratic-Bézier) connection arc from the home marker
/// to a target dot.
///
/// `progress` ∈ [0, 1] controls how much of the arc is drawn — 0 is just the
/// home endpoint, 1 is the full arc reaching the dot — so freshly created
/// connections appear to "grow" out from home toward the node.  The core is a
/// single-pixel Bézier with a thin soft glow, noticeably slimmer than the old
/// straight neon line.  A bright leading head marks the travelling tip while
/// the arc is still being drawn.
fn draw_connection_line(
    img: &mut RgbaImage,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: Rgba<u8>,
    glow: i32,
    progress: f32,
) {
    let w = img.width();
    let h = img.height();

    let (fx0, fy0) = (x0 as f32, y0 as f32);
    let (fx1, fy1) = (x1 as f32, y1 as f32);
    let (cx, cy) = arc_control_point(fx0, fy0, fx1, fy1);

    // More samples for longer arcs so the curve stays smooth.
    let dist = ((fx1 - fx0).powi(2) + (fy1 - fy0).powi(2)).sqrt();
    let steps = ((dist / 3.0).ceil() as i32).clamp(12, 200) as usize;
    let drawn = ((steps as f32) * progress)
        .round()
        .clamp(1.0, steps as f32) as usize;

    let bez = |t: f32| -> (f32, f32) {
        let u = 1.0 - t;
        (
            u * u * fx0 + 2.0 * u * t * cx + t * t * fx1,
            u * u * fy0 + 2.0 * u * t * cy + t * t * fy1,
        )
    };

    let mut pixels: Vec<(i32, i32)> = Vec::with_capacity(drawn * 4);
    let mut prev = bez(0.0);
    for i in 1..=drawn {
        let t = (i as f32) / (steps as f32);
        let cur = bez(t);
        plot_line_collect(
            img,
            &mut pixels,
            prev.0 as i32,
            prev.1 as i32,
            cur.0 as i32,
            cur.1 as i32,
            w,
            h,
            color,
        );
        prev = cur;
    }

    // Bright leading head at the current tip while the arc is still growing.
    if progress < 1.0 {
        plot_glow(img, prev.0 as i32, prev.1 as i32, color);
    }

    if glow <= 0 {
        return;
    }

    // Thin soft glow around the drawn portion — a small box blur so the arc
    // reads as a neon trace even when diagonal.  Kept to a tight radius so the
    // line stays slim.
    let glow_radius = glow.min(2).max(1);
    let falloff = 0.4f32;
    for g in 1..=glow_radius {
        let alpha_mul = falloff.powi(g);
        for &(lx, ly) in &pixels {
            for (ox, oy) in [(0, g), (0, -g), (g, 0), (-g, 0)] {
                let gx = lx + ox;
                let gy = ly + oy;
                if gx >= 0 && gy >= 0 && gx < w as i32 && gy < h as i32 {
                    let gcolor = Rgba([
                        color.0[0],
                        color.0[1],
                        color.0[2],
                        (color.0[3] as f32 * alpha_mul).min(255.0) as u8,
                    ]);
                    blend_pixel(img, gx as u32, gy as u32, gcolor);
                }
            }
        }
    }
}

/// Lighten an RGBA color by mixing it with white.
fn lighten(c: &Rgba<u8>, amount: f32) -> Rgba<u8> {
    let a = c.0[3];
    let mix = |base: f32| -> u8 { ((base * (1.0 - amount) + 255.0 * amount).min(255.0)) as u8 };
    Rgba([
        mix(c.0[0] as f32),
        mix(c.0[1] as f32),
        mix(c.0[2] as f32),
        a,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bundled Natural Earth vector world map must rasterize to the
    /// documented dimensions and contain at least one land pixel.
    #[test]
    fn default_map_dimensions_and_land() {
        use crate::config::Config;
        use parking_lot::RwLock;
        let cfg = Arc::new(RwLock::new(Config::default()));
        let img = MapRenderer::load(cfg)
            .unwrap()
            .redraw(&[], &HomeLocation::default(), false, None);
        assert_eq!(img.width(), DEFAULT_MAP_W);
        assert_eq!(img.height(), DEFAULT_MAP_H);
        let land = Config::default().colors.land.to_rgba(255);
        let mut found_land = false;
        for y in (0..DEFAULT_MAP_H).step_by(32) {
            for x in (0..DEFAULT_MAP_W).step_by(32) {
                let p = img.get_pixel(x, y);
                if *p == land {
                    found_land = true;
                    break;
                }
            }
            if found_land {
                break;
            }
        }
        assert!(
            found_land,
            "bundled vector map should have at least one land pixel"
        );
    }

    /// Markers of each style should alter pixels around a known lat/lon.
    #[test]
    fn marker_styles_draw_pixels() {
        use crate::config::{Config, MarkerStyle};
        use parking_lot::RwLock;
        use std::net::IpAddr;
        use std::time::Instant;

        for style in [
            MarkerStyle::Dot,
            MarkerStyle::Ring,
            MarkerStyle::Cross,
            MarkerStyle::X,
        ] {
            let mut cfg = Config::default();
            cfg.marker_style = style;
            cfg.marker_size = 6;
            let cfg = Arc::new(RwLock::new(cfg));
            let renderer = MapRenderer::load(cfg).unwrap();
            let dot = MapDot {
                lat: 51.5,
                lon: -0.1,
                country: "UK".into(),
                city: "London".into(),
                severity: crate::event::Severity::Info,
                src_ip: "1.2.3.4".parse::<IpAddr>().unwrap(),
                proxy: None,
                created_at: Instant::now(),
            };
            let img = renderer.redraw(&[dot], &HomeLocation::default(), false, None);
            let (px, py) = latlon_to_pixel(51.5, -0.1, img.width(), img.height());
            let mut changed = 0;
            let radius = 8i32;
            for dy in -radius..=radius {
                for dx in -radius..=radius {
                    let x = (px + dx).clamp(0, img.width() as i32 - 1) as u32;
                    let y = (py + dy).clamp(0, img.height() as i32 - 1) as u32;
                    let p = img.get_pixel(x, y);
                    if p.0[0] > 80 || p.0[1] > 200 || p.0[2] > 100 {
                        changed += 1;
                    }
                }
            }
            assert!(
                changed >= 30,
                "style {:?} should draw at least 30 pixels near the marker, got {}",
                style,
                changed
            );
        }
    }

    /// Toggling connection lines on should leave green-ish pixels between
    /// the home marker (0°N, 0°E) and a target marker.
    #[test]
    fn connection_lines_draw_green_pixels() {
        use crate::config::{Config, MarkerStyle};
        use parking_lot::RwLock;
        use std::net::IpAddr;
        use std::time::Instant;

        let mut cfg = Config::default();
        cfg.marker_style = MarkerStyle::Dot;
        cfg.marker_size = 2;
        cfg.connection_lines.color = crate::config::ColorDef::from_rgb(0x00, 0xFF, 0x00);
        cfg.connection_lines.glow_size = 2;
        let cfg = Arc::new(RwLock::new(cfg));
        let renderer = MapRenderer::load(cfg).unwrap();
        // Age the dot past LINE_DRAW_SECS so the full arc is drawn (a freshly
        // created dot only draws the growing stub of the animated arc).
        let created = Instant::now()
            .checked_sub(std::time::Duration::from_secs(1))
            .unwrap();
        let dot = MapDot {
            lat: 51.5,
            lon: -0.1,
            country: "UK".into(),
            city: "London".into(),
            severity: crate::event::Severity::Info,
            src_ip: "1.2.3.4".parse::<IpAddr>().unwrap(),
            proxy: None,
            created_at: created,
        };
        let home = HomeLocation {
            lat: 0.0,
            lon: 0.0,
            ip: None,
            label: None,
        };
        let off = renderer.redraw(&[dot.clone()], &home, false, None);
        let on = renderer.redraw(&[dot], &home, true, None);

        let mut different = 0;
        for y in 0..off.height() {
            for x in 0..off.width() {
                if off.get_pixel(x, y) != on.get_pixel(x, y) {
                    different += 1;
                }
            }
        }
        assert!(
            different >= 5,
            "connection lines should change at least 5 pixels, got {}",
            different
        );

        // At least one of those changed pixels should be strongly green.
        let mut green_pixels = 0;
        for y in 0..on.height() {
            for x in 0..on.width() {
                let p = on.get_pixel(x, y);
                if p.0[1] >= 200 && p.0[0] < 50 && p.0[2] < 50 {
                    green_pixels += 1;
                }
            }
        }
        assert!(
            green_pixels >= 3,
            "connection lines should leave >= 3 green pixels, got {}",
            green_pixels
        );
    }

    /// An arc whose *target* node is scrolled off-screen by zooming must still
    /// paint its visible portion near home — the old code culled the whole arc
    /// as soon as the target left the viewport.  Regression guard for the
    /// `arc_intersects_viewport` bounding-box test in `redraw`.
    #[test]
    fn connection_lines_drawn_when_target_offscreen() {
        use crate::config::{Config, MarkerStyle};
        use parking_lot::RwLock;
        use std::net::IpAddr;
        use std::time::Instant;

        let mut cfg = Config::default();
        cfg.marker_style = MarkerStyle::Dot;
        cfg.marker_size = 2;
        cfg.connection_lines.color = crate::config::ColorDef::from_rgb(0x00, 0xFF, 0x00);
        cfg.connection_lines.glow_size = 2;
        let cfg = Arc::new(RwLock::new(cfg));
        let renderer = MapRenderer::load(cfg).unwrap();
        let created = Instant::now()
            .checked_sub(std::time::Duration::from_secs(1))
            .unwrap();
        // London is well north of the zoomed viewport below (lat 51.5 vs a
        // visible band of ±9°), so the target node is off-screen while home
        // (0°N, 0°E) stays centered and visible.
        let dot = MapDot {
            lat: 51.5,
            lon: -0.1,
            country: "UK".into(),
            city: "London".into(),
            severity: crate::event::Severity::Info,
            src_ip: "1.2.3.4".parse::<IpAddr>().unwrap(),
            proxy: None,
            created_at: created,
        };
        let home = HomeLocation {
            lat: 0.0,
            lon: 0.0,
            ip: None,
            label: None,
        };
        let vp = Viewport {
            zoom: 10.0,
            center_lat: 0.0,
            center_lon: 0.0,
        };
        // Sanity: the target really is off-screen in this viewport.
        let w = DEFAULT_MAP_W;
        let h = DEFAULT_MAP_H;
        assert!(
            !vp.contains(dot.lat, dot.lon, w, h),
            "test precondition: target should be off-screen"
        );
        assert!(
            vp.contains(home.lat, home.lon, w, h),
            "test precondition: home should be on-screen"
        );

        let off = renderer.redraw(&[dot.clone()], &home, false, Some(vp));
        let on = renderer.redraw(&[dot], &home, true, Some(vp));

        // Enabling lines must still add green pixels even though the target
        // is off-screen — the visible stub of the arc near home is drawn.
        let mut green_pixels = 0;
        for y in 0..on.height() {
            for x in 0..on.width() {
                let p = on.get_pixel(x, y);
                let q = off.get_pixel(x, y);
                if p.0[1] >= 200 && p.0[0] < 50 && p.0[2] < 50 && p != q {
                    green_pixels += 1;
                }
            }
        }
        assert!(
            green_pixels >= 3,
            "off-screen target arc should still paint >= 3 green pixels near home, got {}",
            green_pixels
        );
    }

    /// Direct unit test for the arc visibility helper.
    #[test]
    fn arc_intersects_viewport_bounds_cases() {
        // Home on-screen, target far off the right edge → still intersects.
        assert!(arc_intersects_viewport(100, 100, 5_000, 100, 256, 256));
        // Both endpoints off-screen on the same side, arc bows further left
        // (control point left of them) → does not intersect.
        assert!(!arc_intersects_viewport(-500, 100, -300, 100, 256, 256));
        // Both endpoints off-screen on opposite sides → the arc crosses → intersects.
        assert!(arc_intersects_viewport(-300, 100, 600, 100, 256, 256));
    }

    /// The bundled vector map must expose a reasonable set of country labels.
    #[test]
    fn bundled_country_labels_are_parsed() {
        use crate::config::Config;
        use parking_lot::RwLock;
        let cfg = Arc::new(RwLock::new(Config::default()));
        let renderer = MapRenderer::load(cfg).unwrap();
        let labels = renderer.country_labels();
        assert!(
            labels.len() >= 120,
            "expected at least 120 country labels, got {}",
            labels.len()
        );

        let find = |name: &str| labels.iter().find(|l| l.name == name);
        let us = find("United States").expect("USA label");
        let china = find("China").expect("China label");
        let brazil = find("Brazil").expect("Brazil label");

        // USA should be in the western hemisphere, northern hemisphere.
        assert!(us.lon < -50.0 && us.lon > -130.0, "USA lon {:?}", us.lon);
        assert!(us.lat > 20.0 && us.lat < 55.0, "USA lat {:?}", us.lat);

        // China in eastern/northern.
        assert!(china.lon > 80.0 && china.lon < 130.0, "China lon {:?}", china.lon);
        assert!(china.lat > 15.0 && china.lat < 55.0, "China lat {:?}", china.lat);

        // Brazil in western/southern.
        assert!(brazil.lon < -30.0 && brazil.lon > -75.0, "Brazil lon {:?}", brazil.lon);
        assert!(brazil.lat < 10.0 && brazil.lat > -35.0, "Brazil lat {:?}", brazil.lat);
    }

    /// Country labels should end up inside the country they name, not in a
    /// neighbouring sea.  The vector map derives label positions from a hard-
    /// coded geographic centroid table so small European states are placed
    /// correctly even when the source data does not name them.
    #[test]
    fn bundled_european_labels_are_inside_target_countries() {
        use crate::config::Config;
        use parking_lot::RwLock;
        let cfg = Arc::new(RwLock::new(Config::default()));
        let renderer = MapRenderer::load(cfg).unwrap();
        let labels = renderer.country_labels();
        let find = |name: &str| labels.iter().find(|l| l.name == name);

        let uk = find("United Kingdom").expect("UK label");
        let germany = find("Germany").expect("Germany label");
        let france = find("France").expect("France label");
        let belgium = find("Belgium").expect("Belgium label");
        let luxembourg = find("Luxembourg").expect("Luxembourg label");

        // UK label should be over the UK/Ireland landmass, not in the Atlantic.
        assert!(
            uk.lon > -15.0 && uk.lon < 3.0 && uk.lat > 50.0 && uk.lat < 65.0,
            "UK label misplaced: lon={:.2}, lat={:.2}",
            uk.lon,
            uk.lat
        );
        // These small European labels used to be drawn in the North Sea/Irish Sea.
        assert!(
            germany.lon > 5.0 && germany.lon < 15.0 && germany.lat > 47.0 && germany.lat < 55.0,
            "Germany label misplaced: lon={:.2}, lat={:.2}",
            germany.lon,
            germany.lat
        );
        assert!(
            france.lon > -6.0 && france.lon < 8.0 && france.lat > 42.0 && france.lat < 52.0,
            "France label misplaced: lon={:.2}, lat={:.2}",
            france.lon,
            france.lat
        );
        assert!(
            belgium.lon > 2.0 && belgium.lon < 7.0 && belgium.lat > 49.0 && belgium.lat < 52.0,
            "Belgium label misplaced: lon={:.2}, lat={:.2}",
            belgium.lon,
            belgium.lat
        );
        assert!(
            luxembourg.lon > 5.0 && luxembourg.lon < 7.0 && luxembourg.lat > 49.0 && luxembourg.lat < 51.0,
            "Luxembourg label misplaced: lon={:.2}, lat={:.2}",
            luxembourg.lon,
            luxembourg.lat
        );
    }

    /// Major cities must fall on land pixels in the Natural Earth vector map,
    /// proving that the projection and landmass geometry are aligned with real
    /// world coordinates.
    #[test]
    fn major_cities_land_on_vector_map() {
        use crate::config::Config;
        use parking_lot::RwLock;
        let cfg = Arc::new(RwLock::new(Config::default()));
        let renderer = MapRenderer::load(cfg).unwrap();
        let img = renderer.redraw(&[], &HomeLocation::default(), false, None);
        let land = Config::default().colors.land.to_rgba(255);

        let cities = [
            ("Edinburgh", 55.9533, -3.1883),
            ("London", 51.5074, -0.1278),
            ("New York", 40.7128, -74.0060),
            ("Tokyo", 35.6762, 139.6503),
        ];

        for (name, lat, lon) in cities {
            let (px, py) = latlon_to_pixel(lat, lon, img.width(), img.height());
            // Allow a small neighbourhood so coastal cities that fall on an
            // anti-aliased edge are still accepted.
            let mut found_land = false;
            let radius = 2i32;
            for dy in -radius..=radius {
                for dx in -radius..=radius {
                    let x = (px + dx).clamp(0, img.width() as i32 - 1) as u32;
                    let y = (py + dy).clamp(0, img.height() as i32 - 1) as u32;
                    if *img.get_pixel(x, y) == land {
                        found_land = true;
                        break;
                    }
                }
                if found_land {
                    break;
                }
            }
            assert!(
                found_land,
                "{} ({}, {}) should land on a land pixel near ({}, {})",
                name, lat, lon, px, py
            );
        }
    }

    /// A well-known city should map to the correct side of the full-world map.
    #[test]
    fn san_francisco_latlon_projection_sanity() {
        // San Francisco, CA.
        let lat = 37.7749;
        let lon = -122.4194;
        let (px, py) = latlon_to_pixel(lat, lon, DEFAULT_MAP_W, DEFAULT_MAP_H);
        // Pacific coast: left half of the map (lon < 0) and northern half.
        assert!(
            (px as u32) < DEFAULT_MAP_W / 2,
            "San Francisco should be on the left half of the map, got x={px}"
        );
        assert!(
            (py as u32) < DEFAULT_MAP_H / 2,
            "San Francisco should be in the northern half of the map, got y={py}"
        );
    }

    /// A marker must remain visible on the dark procedural fallback map (the
    /// one used when the bundled SVG cannot be rasterized).  Regression test
    /// for markers disappearing against dark land/ocean.
    #[test]
    fn marker_visible_on_dark_procedural_map() {
        use crate::config::Config;
        use std::net::IpAddr;
        use std::time::Instant;

        let cfg = Config::default();
        let colors = cfg.colors.clone();
        let mut img = default_map_image(&colors);
        let dot = MapDot {
            lat: 51.5,
            lon: -0.1,
            country: "UK".into(),
            city: "London".into(),
            severity: crate::event::Severity::Info,
            src_ip: "1.2.3.4".parse::<IpAddr>().unwrap(),
            proxy: None,
            created_at: Instant::now(),
        };
        let (px, py) = latlon_to_pixel(dot.lat, dot.lon, img.width(), img.height());
        let color = colors.info.to_rgba(255);
        plot_outer_halo(&mut img, px, py, color, 24);
        plot_glow(&mut img, px, py, color);
        plot_marker(&mut img, px, py, color, Config::default().marker_style, 8);

        let mut bright = 0;
        let radius = 12i32;
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                let x = (px + dx).clamp(0, img.width() as i32 - 1) as u32;
                let y = (py + dy).clamp(0, img.height() as i32 - 1) as u32;
                let p = img.get_pixel(x, y);
                // The dark procedural land is ~#233E5F; a bright info marker
                // should push green well above the background.
                if p.0[1] > 120 {
                    bright += 1;
                }
            }
        }
        assert!(
            bright >= 10,
            "marker should leave >= 10 bright green pixels on the dark procedural map, got {}",
            bright
        );
    }
}

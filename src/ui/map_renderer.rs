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

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use image::{Rgba, RgbaImage};
use parking_lot::Mutex;
use tracing::{info, warn};

use crate::event::Severity;
use crate::ui::app::{MapDot, Panel};

/// Default map: procedural fallback so the dashboard is never blank.
/// Generates a 2048×1024 dark equirectangular world backdrop with a
/// lat/lon graticule and simplified continent silhouettes.
const DEFAULT_MAP_W: u32 = 2048;
const DEFAULT_MAP_H: u32 = 1024;

/// Configure the home location (lat, lon) where connections terminate.
#[derive(Debug, Clone, Copy)]
pub struct HomeLocation {
    pub lat: f64,
    pub lon: f64,
}

impl Default for HomeLocation {
    fn default() -> Self {
        Self { lat: 0.0, lon: 0.0 }
    }
}

/// Renderer state. Holds the working image buffer; `ratatui-image`'s
/// picker calls `image()` each frame.
pub struct MapRenderer {
    /// Persistent working buffer – re-cleared from `base` each frame.
    work: Mutex<RgbaImage>,
    /// Base layer (read-only).
    base: RgbaImage,
    /// Pulse phase (0..=2π) for the host indicator.
    pulse: Mutex<f64>,
    /// Last rendered-at – used to advance pulse.
    last_tick: Mutex<Instant>,
}

impl MapRenderer {
    /// Load the map image (PNG/JPEG/SVG) and prepare the working buffer.
    pub fn load(map_path: Option<&Path>) -> Result<Arc<Self>> {
        let base: RgbaImage = match map_path {
            Some(p) => {
                if is_svg(p) {
                    let data = std::fs::read(p)
                        .with_context(|| format!("reading SVG map {}", p.display()))?;
                    render_svg_to_rgba(&data, DEFAULT_MAP_W, DEFAULT_MAP_H)
                        .with_context(|| format!("rendering SVG map {}", p.display()))?
                } else {
                    image::open(p)
                        .with_context(|| format!("loading map image {}", p.display()))?
                        .to_rgba8()
                }
            }
            None => {
                const BUNDLED_SVG: &[u8] = include_bytes!("../../assets/world-map.svg");
                match render_svg_to_rgba(BUNDLED_SVG, DEFAULT_MAP_W, DEFAULT_MAP_H) {
                    Ok(img) => img,
                    Err(e) => {
                        warn!("failed to render bundled SVG map, falling back to procedural map: {e:?}");
                        default_map_image()
                    }
                }
            }
        };
        if let Some(p) = map_path {
            info!(path = %p.display(), "loaded user map");
        } else {
            info!("using bundled SVG world map (pass --map-path to override)");
        }

        let w = base.width();
        let h = base.height();
        info!("map loaded: {w}x{h}");

        let work = base.clone();

        Ok(Arc::new(Self {
            work: Mutex::new(work),
            base,
            pulse: Mutex::new(0.0),
            last_tick: Mutex::new(Instant::now()),
        }))
    }

    /// Re-render the working buffer using the current active dots.
    pub fn redraw(&self, dots: &[MapDot], home_dot: HomeLocation) -> RgbaImage {
        // Refresh base layer into the working buffer.
        let mut work = self.work.lock();
        for (x, y, p) in self.base.enumerate_pixels() {
            work.put_pixel(x, y, *p);
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

        // Plot each dot, faded by age.
        let now = Instant::now();
        for d in dots {
            let age = now.duration_since(d.created_at).as_secs_f64();
            let alpha = 1.0 - (age / 8.0).clamp(0.0, 1.0);
            if alpha <= 0.0 {
                continue;
            }
            let (px, py) = latlon_to_pixel(d.lat, d.lon, work.width(), work.height());
            let color = match d.severity {
                Severity::Info => Rgba([80, 220, 120, (alpha * 255.0) as u8]),
                Severity::Warn => Rgba([250, 200, 50, (alpha * 255.0) as u8]),
                Severity::Alert => Rgba([240, 60, 60, (alpha * 255.0) as u8]),
            };
            plot_glow(&mut work, px, py, color);
        }

        // Draw host pulse.
        let (hx, hy) = latlon_to_pixel(
            home_dot.lat,
            home_dot.lon,
            work.width(),
            work.height(),
        );
        let pulse_alpha = ((pulse_value.sin() + 1.0) * 0.5 * 255.0) as u8;
        plot_glow(
            &mut work,
            hx,
            hy,
            Rgba([60, 180, 255, pulse_alpha.max(180)]),
        );

        work.clone()
    }

}

/// True when the path's extension is `.svg` (case-insensitive).
fn is_svg(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("svg"))
        .unwrap_or(false)
}

/// Rasterize an SVG document to an RGBA image of the requested size.
///
/// The SVG viewBox is scaled to fill the output; the bundled world map is
/// equirectangular and the target size is 2:1, so the stretch is minimal.
fn render_svg_to_rgba(data: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
    let opt = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_data(data, &opt)
        .map_err(|e| anyhow::anyhow!("SVG parse failed: {e}"))?;

    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)
        .context("creating SVG render target")?;

    let scale_x = width as f32 / tree.size().width();
    let scale_y = height as f32 / tree.size().height();
    let transform = resvg::tiny_skia::Transform::from_scale(scale_x, scale_y);

    resvg::render(&tree, transform, &mut pixmap.as_mut());

    RgbaImage::from_raw(width, height, pixmap.data().to_vec())
        .context("unexpected SVG pixmap size")
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

/// Draw a single pixel plus a soft halo (5×5 gaussian-ish glow).
/// Alpha is composited over the base map instead of overwriting it.
fn plot_glow(img: &mut RgbaImage, cx: i32, cy: i32, c: Rgba<u8>) {
    let w = img.width() as i32;
    let h = img.height() as i32;
    let ring: &[(i32, i32, f32)] = &[
        (0, 0, 1.00),
        (-1, 0, 0.60), (1, 0, 0.60), (0, -1, 0.60), (0, 1, 0.60),
        (-1, -1, 0.35), (1, -1, 0.35), (-1, 1, 0.35), (1, 1, 0.35),
        (0, -2, 0.25), (0, 2, 0.25), (-2, 0, 0.25), (2, 0, 0.25),
        (-2, -1, 0.15), (-2, 1, 0.15), (2, -1, 0.15), (2, 1, 0.15),
        (-1, -2, 0.15), (1, -2, 0.15), (-1, 2, 0.15), (1, 2, 0.15),
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
                (((c.0[0] as f32) * src_a + (p.0[0] as f32) * dst_a * inv_src) / out_a)
                    .min(255.0) as u8,
                (((c.0[1] as f32) * src_a + (p.0[1] as f32) * dst_a * inv_src) / out_a)
                    .min(255.0) as u8,
                (((c.0[2] as f32) * src_a + (p.0[2] as f32) * dst_a * inv_src) / out_a)
                    .min(255.0) as u8,
                (out_a * 255.0).min(255.0) as u8,
            ];
        }
    }
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
/// continent silhouettes). Good enough to make `--map-path` optional.
///
/// Equirectangular projection: `lat` ∈ [-90, 90] → y ∈ [H, 0],
/// `lon` ∈ [-180, 180] → x ∈ [0, W]. Continent polygons are coarse —
/// they're hand-rough-sketches, not cartographically accurate, but
/// they resolve to obviously-recognisable landmasses at any zoom.
fn default_map_image() -> RgbaImage {
    let w = DEFAULT_MAP_W;
    let h = DEFAULT_MAP_H;
    let mut img = RgbaImage::new(w, h);

    // --- 1. Ocean: deep dark blue with a subtle horizontal vignette.
    let ocean_top = Rgba([10, 18, 30, 255]);
    let ocean_bot = Rgba([13, 24, 40, 255]);
    for y in 0..h {
        let t = y as f32 / h as f32;
        let r = ocean_top.0[0] as f32 * (1.0 - t) + ocean_bot.0[0] as f32 * t;
        let g = ocean_top.0[1] as f32 * (1.0 - t) + ocean_bot.0[1] as f32 * t;
        let b = ocean_top.0[2] as f32 * (1.0 - t) + ocean_bot.0[2] as f32 * t;
        let lat_factor = 1.0 - ((y as f32 / h as f32 - 0.5) * 2.0).abs();
        let v = 1.0 - 0.15 * (1.0 - lat_factor);
        let px = Rgba([
            (r * v) as u8,
            (g * v) as u8,
            (b * v) as u8,
            255,
        ]);
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
    let land = Rgba([35, 62, 95, 255]); // muted slate-blue landmass
    let land_edge = Rgba([70, 120, 170, 255]);
    let continents: &[&[(f32, f32)]] = &[
        // North America
        &[
            (71.0, -156.0), (70.0, -141.0), (69.0, -120.0), (60.0, -94.0),
            (49.0, -94.0), (49.0, -123.0), (32.0, -117.0), (25.0, -110.0),
            (18.0, -97.0), (16.0, -88.0), (22.0, -82.0), (26.0, -81.0),
            (32.0, -80.0), (40.0, -74.0), (45.0, -67.0), (51.0, -56.0),
            (60.0, -64.0), (65.0, -78.0), (70.0, -90.0), (74.0, -95.0),
            (78.0, -120.0), (74.0, -140.0),
        ],
        // South America
        &[
            (12.0, -82.0), (10.0, -67.0), (5.0, -52.0), (-5.0, -35.0),
            (-23.0, -41.0), (-35.0, -57.0), (-55.0, -68.0), (-50.0, -73.0),
            (-30.0, -71.0), (-18.0, -75.0), (-5.0, -81.0), (2.0, -78.0),
            (8.0, -77.0),
        ],
        // Europe
        &[
            (71.0, 25.0), (70.0, 30.0), (60.0, 30.0), (55.0, 35.0),
            (45.0, 40.0), (40.0, 25.0), (37.0, 0.0), (43.0, -8.0),
            (50.0, -5.0), (58.0, -7.0), (60.0, 5.0), (58.0, 12.0),
            (55.0, 14.0), (54.0, 8.0), (60.0, 11.0), (65.0, 12.0),
        ],
        // Africa
        &[
            (37.0, -8.0), (32.0, 10.0), (30.0, 32.0), (12.0, 43.0),
            (12.0, 51.0), (-2.0, 41.0), (-15.0, 40.0), (-35.0, 20.0),
            (-34.0, 18.0), (-29.0, 16.0), (-12.0, 13.0), (5.0, 0.0),
            (5.0, -10.0), (10.0, -16.0), (20.0, -17.0), (30.0, -9.0),
        ],
        // Asia (one big blob)
        &[
            (78.0, 60.0), (78.0, 140.0), (70.0, 165.0), (60.0, 170.0),
            (50.0, 142.0), (35.0, 140.0), (22.0, 120.0), (10.0, 105.0),
            (1.0, 103.0), (8.0, 80.0), (22.0, 68.0), (25.0, 55.0),
            (12.0, 43.0), (30.0, 32.0), (40.0, 26.0), (45.0, 40.0),
            (55.0, 50.0), (60.0, 55.0), (70.0, 60.0),
        ],
        // Australia
        &[
            (-12.0, 130.0), (-12.0, 142.0), (-18.0, 146.0), (-25.0, 153.0),
            (-37.0, 150.0), (-38.0, 141.0), (-35.0, 135.0), (-32.0, 125.0),
            (-22.0, 113.0),
        ],
        // Greenland
        &[
            (83.0, -30.0), (80.0, -20.0), (70.0, -22.0), (60.0, -43.0),
            (70.0, -55.0), (78.0, -70.0), (82.0, -45.0),
        ],
        // Antarctica (just a strip across the bottom)
        &[
            (-65.0, -180.0), (-85.0, -120.0), (-80.0, -60.0),
            (-75.0, 0.0), (-72.0, 60.0), (-68.0, 120.0), (-72.0, 180.0),
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
fn draw_line(img: &mut RgbaImage, x0: i32, y0: i32, x1: i32, y1: i32, w: u32, h: u32, color: Rgba<u8>) {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The bundled SVG world map must rasterize to the documented
    /// dimensions and contain at least one land pixel.
    #[test]
    fn default_map_dimensions_and_land() {
        let img = MapRenderer::load(None).unwrap().redraw(&[], HomeLocation::default());
        assert_eq!(img.width(), DEFAULT_MAP_W);
        assert_eq!(img.height(), DEFAULT_MAP_H);
        let mut found_land = false;
        for y in (0..DEFAULT_MAP_H).step_by(32) {
            for x in (0..DEFAULT_MAP_W).step_by(32) {
                let p = img.get_pixel(x, y);
                // Bundled SVG land is #FFF8DC (R=255, G=248, B=220).
                // Ocean is #87CEEB (R=135, G=206, B=235).
                if p.0[0] > 240 && p.0[1] > 230 && p.0[2] > 180 {
                    found_land = true;
                    break;
                }
            }
            if found_land {
                break;
            }
        }
        assert!(found_land, "bundled SVG map should have at least one land pixel");
    }
}

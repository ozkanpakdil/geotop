//! User-facing configuration loaded from `~/.config/geotop/config.json`
//! (or any path passed with `--config`).
//!
//! The file is JSON.  All fields are optional; missing values fall back to
//! the same hard-coded defaults the program used before configuration existed.
//!
//! A `notify`-based watcher can hot-reload the file while `geotop` runs.
//! Some changes (colors, marker TTL) apply immediately; others (SVG render
//! size, map asset path) require a restart because they affect cached state.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{de::Visitor, Deserialize, Deserializer, Serialize};
use tracing::info;

use crate::ui::map_renderer::HomeLocation;

/// Visual style of a marker on the world map.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MarkerStyle {
    /// Small soft glow (legacy look).
    Dot,
    /// Hollow ring outline.
    Ring,
    /// Plus / cross shape.
    Cross,
    /// Diagonal X shape.
    X,
}

impl Default for MarkerStyle {
    fn default() -> Self {
        DEFAULT_MARKER_STYLE
    }
}

impl MarkerStyle {
    #[allow(dead_code)]
    pub fn label(&self) -> &'static str {
        match self {
            MarkerStyle::Dot => "dot",
            MarkerStyle::Ring => "ring",
            MarkerStyle::Cross => "cross",
            MarkerStyle::X => "x",
        }
    }
}

/// Default marker lifetime, in seconds.
const DEFAULT_MARKER_TTL_SECONDS: u64 = 8;

/// Default maximum number of live markers on the map.
const DEFAULT_MAX_MARKERS: usize = 5_000;

/// Default marker visual style.
const DEFAULT_MARKER_STYLE: MarkerStyle = MarkerStyle::Ring;

/// Default marker size in pixels.  Large enough to remain visible after the
/// 2048×1024 map is downscaled to a typical terminal/GUI viewport.
const DEFAULT_MARKER_SIZE: u8 = 8;

/// Default home location (roughly Washington, DC).
const DEFAULT_HOME_LAT: f64 = 39.0;
const DEFAULT_HOME_LON: f64 = -77.0;

/// Default home marker visual style.
const DEFAULT_HOME_MARKER_STYLE: MarkerStyle = MarkerStyle::Ring;

/// Default home marker size in pixels.  Larger than regular markers so the
/// "home" pulse is always easy to spot.
const DEFAULT_HOME_MARKER_SIZE: u8 = 14;

/// Default connection line startup state.
/// Connection lines are shown on startup; pressing `l` toggles them off.
const DEFAULT_CONNECTION_LINES_ENABLED: bool = true;

/// Default connection line glow radius in pixels.
const DEFAULT_CONNECTION_LINE_GLOW: u8 = 1;

/// Top-level configuration.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Config {
    /// How long a packet/log marker stays visible on the map.
    #[serde(default = "default_marker_ttl_seconds")]
    pub marker_ttl_seconds: u64,

    /// Maximum number of markers retained on the map.
    #[serde(default = "default_max_markers")]
    pub max_markers: usize,

    /// Visual style of each marker on the map.
    #[serde(default = "default_marker_style")]
    pub marker_style: MarkerStyle,

    /// Size/thickness of each marker (1–20 pixels).
    #[serde(default = "default_marker_size")]
    pub marker_size: u8,

    /// Map asset + projection settings.
    #[serde(default)]
    pub map: MapConfig,

    /// Toggleable glowing connection lines from home to each active marker.
    #[serde(default)]
    pub connection_lines: ConnectionLinesConfig,

    /// UI color theme.
    #[serde(default)]
    pub colors: ColorConfig,

    /// Font settings.
    #[serde(default)]
    pub fonts: FontConfig,

    /// GUI window size.
    #[serde(default)]
    pub window: WindowConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            marker_ttl_seconds: DEFAULT_MARKER_TTL_SECONDS,
            max_markers: DEFAULT_MAX_MARKERS,
            marker_style: default_marker_style(),
            marker_size: default_marker_size(),
            map: MapConfig::default(),
            connection_lines: ConnectionLinesConfig::default(),
            colors: ColorConfig::default(),
            fonts: FontConfig::default(),
            window: WindowConfig::default(),
        }
    }
}

impl Config {
    /// Resolve and load the configuration file.
    ///
    /// * If `explicit_path` is given, that file is used.
    /// * Otherwise `~/.config/geotop/config.json` is tried.
    /// * If neither exists, defaults are returned.
    ///
    /// The returned path is the file that should be watched for hot-reload.
    pub fn load(explicit_path: Option<&Path>) -> Result<(Arc<Self>, Option<PathBuf>)> {
        if let Some(p) = explicit_path {
            let cfg = Self::read_file(p)
                .with_context(|| format!("loading config from {}", p.display()))?;
            return Ok((Arc::new(cfg), Some(p.to_path_buf())));
        }

        if let Some(dirs) = ProjectDirs::from("dev", "geotop", "geotop") {
            let p = dirs.config_dir().join("config.json");
            if p.exists() {
                let cfg = Self::read_file(&p)
                    .with_context(|| format!("loading config from {}", p.display()))?;
                info!(path = %p.display(), "loaded user config");
                return Ok((Arc::new(cfg), Some(p)));
            }
        }

        info!("no config file found, using defaults");
        Ok((Arc::new(Self::default()), None))
    }

    /// Read and parse a single JSON config file.
    pub(crate) fn read_file(path: &Path) -> Result<Self> {
        let data =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::read_str(&data)
    }

    /// Parse and validate a JSON config from an in-memory string.
    pub(crate) fn read_str(data: &str) -> Result<Self> {
        let cfg: Config =
            serde_json::from_str(data).with_context(|| "parsing JSON config")?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Sanity-check user-provided values.
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.marker_ttl_seconds > 0 && self.marker_ttl_seconds <= 3600,
            "marker_ttl_seconds must be between 1 and 3600"
        );
        anyhow::ensure!(self.max_markers > 0, "max_markers must be > 0");
        anyhow::ensure!(
            self.marker_size > 0 && self.marker_size <= 20,
            "marker_size must be between 1 and 20"
        );
        self.map.validate()?;
        self.connection_lines.validate()?;
        self.window.validate()?;
        self.fonts.validate()?;
        Ok(())
    }

    /// Convert the map home coordinates to the renderer's `HomeLocation`.
    pub fn home_location(&self) -> HomeLocation {
        HomeLocation {
            lat: self.map.home.lat,
            lon: self.map.home.lon,
            ip: None,
            label: None,
        }
    }

    /// Marker TTL as a `std::time::Duration`.
    pub fn marker_ttl(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.marker_ttl_seconds)
    }

    /// Spawn a file-system watcher for the config file and apply changes
    /// atomically to the shared `RwLock`.  The returned watcher must be kept
    /// alive; dropping it stops watching.
    pub fn spawn_watcher(
        path: PathBuf,
        config: std::sync::Arc<parking_lot::RwLock<Config>>,
    ) -> anyhow::Result<notify::RecommendedWatcher> {
        use notify::{RecursiveMode, Watcher};
        use tracing::error;

        let path_for_cb = path.clone();
        let mut watcher = notify::recommended_watcher(
            move |res: std::result::Result<notify::Event, notify::Error>| {
                let Ok(event) = res else { return };
                if !event.kind.is_modify() && !event.kind.is_create() {
                    return;
                }
                match Config::read_file(&path_for_cb) {
                    Ok(new) => {
                        let diff = {
                            let old = config.read();
                            old.diff(&new)
                        };
                        *config.write() = new;
                        diff.log();
                    }
                    Err(e) => error!(error = %e, "failed to hot-reload config"),
                }
            },
        )?;
        watcher.watch(&path, RecursiveMode::NonRecursive)?;
        Ok(watcher)
    }

    /// Compare two configs and return which hot-reload fields changed.
    pub fn diff(&self, other: &Config) -> ConfigDiff {
        ConfigDiff {
            marker_ttl: self.marker_ttl_seconds != other.marker_ttl_seconds,
            max_markers: self.max_markers != other.max_markers,
            marker_style: self.marker_style != other.marker_style
                || self.marker_size != other.marker_size,
            home: self.map.home != other.map.home,
            labels: self.map.labels != other.map.labels,
            connection_lines: self.connection_lines != other.connection_lines,
            colors: self.colors != other.colors,
            tui_font: self.fonts.tui_font_width != other.fonts.tui_font_width
                || self.fonts.tui_font_height != other.fonts.tui_font_height,
            gui_font: self.fonts.gui_body != other.fonts.gui_body
                || self.fonts.gui_heading != other.fonts.gui_heading
                || self.fonts.gui_font_file != other.fonts.gui_font_file,
            window: self.window != other.window,
        }
    }
}

fn default_marker_ttl_seconds() -> u64 {
    DEFAULT_MARKER_TTL_SECONDS
}

fn default_max_markers() -> usize {
    DEFAULT_MAX_MARKERS
}

fn default_marker_style() -> MarkerStyle {
    DEFAULT_MARKER_STYLE
}

fn default_marker_size() -> u8 {
    DEFAULT_MARKER_SIZE
}

/// Map asset and projection configuration.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct MapConfig {
    /// Home location and its persistent marker style.
    #[serde(default)]
    pub home: HomeConfig,

    /// Country/city label overlay settings.
    #[serde(default)]
    pub labels: LabelConfig,
}

impl Default for MapConfig {
    fn default() -> Self {
        Self {
            home: HomeConfig::default(),
            labels: LabelConfig::default(),
        }
    }
}

impl MapConfig {
    fn validate(&self) -> Result<()> {
        self.home.validate()?;
        self.labels.validate()?;
        Ok(())
    }
}

/// Home location and its persistent marker appearance.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
pub struct HomeConfig {
    #[serde(default = "default_home_lat")]
    pub lat: f64,

    #[serde(default = "default_home_lon")]
    pub lon: f64,

    /// Marker style for the persistent home indicator.
    #[serde(default = "default_home_marker_style")]
    pub marker_style: MarkerStyle,

    /// Marker size for the persistent home indicator (1–20).
    #[serde(default = "default_home_marker_size")]
    pub marker_size: u8,
}

impl HomeConfig {
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.marker_size > 0 && self.marker_size <= 20,
            "home.marker_size must be between 1 and 20"
        );
        Ok(())
    }
}

impl Default for HomeConfig {
    fn default() -> Self {
        Self {
            lat: DEFAULT_HOME_LAT,
            lon: DEFAULT_HOME_LON,
            marker_style: DEFAULT_HOME_MARKER_STYLE,
            marker_size: DEFAULT_HOME_MARKER_SIZE,
        }
    }
}

fn default_home_lat() -> f64 {
    DEFAULT_HOME_LAT
}

fn default_home_lon() -> f64 {
    DEFAULT_HOME_LON
}

fn default_home_marker_style() -> MarkerStyle {
    DEFAULT_HOME_MARKER_STYLE
}

fn default_home_marker_size() -> u8 {
    DEFAULT_HOME_MARKER_SIZE
}

/// Default label settings.
const DEFAULT_SHOW_COUNTRY_LABELS: bool = true;
const DEFAULT_SHOW_CITY_LABELS: bool = true;
const DEFAULT_CITY_LABEL_ZOOM: f64 = 2.5;

/// Map label overlay configuration.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct LabelConfig {
    /// Show country names on the map.
    #[serde(default = "default_show_country_labels")]
    pub show_country_labels: bool,

    /// Show city names next to markers when zoomed in.
    #[serde(default = "default_show_city_labels")]
    pub show_city_labels: bool,

    /// Minimum GUI zoom level before city labels appear.
    #[serde(default = "default_city_label_zoom")]
    pub city_label_zoom: f64,
}

impl Default for LabelConfig {
    fn default() -> Self {
        Self {
            show_country_labels: DEFAULT_SHOW_COUNTRY_LABELS,
            show_city_labels: DEFAULT_SHOW_CITY_LABELS,
            city_label_zoom: DEFAULT_CITY_LABEL_ZOOM,
        }
    }
}

impl LabelConfig {
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.city_label_zoom >= 1.0 && self.city_label_zoom <= 10.0,
            "map.labels.city_label_zoom must be between 1.0 and 10.0"
        );
        Ok(())
    }
}

fn default_show_country_labels() -> bool {
    DEFAULT_SHOW_COUNTRY_LABELS
}

fn default_show_city_labels() -> bool {
    DEFAULT_SHOW_CITY_LABELS
}

fn default_city_label_zoom() -> f64 {
    DEFAULT_CITY_LABEL_ZOOM
}

/// Glowing connection lines from home to each active marker.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ConnectionLinesConfig {
    /// Whether lines are drawn on startup.
    #[serde(default = "default_connection_lines_enabled")]
    pub enabled: bool,

    /// Color of the lines (Matrix green by default).
    #[serde(default = "default_connection_lines_color")]
    pub color: ColorDef,

    /// Extra pixel glow radius around each line (0–10).
    #[serde(default = "default_connection_line_glow")]
    pub glow_size: u8,
}

impl Default for ConnectionLinesConfig {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_CONNECTION_LINES_ENABLED,
            color: default_connection_lines_color(),
            glow_size: DEFAULT_CONNECTION_LINE_GLOW,
        }
    }
}

impl ConnectionLinesConfig {
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.glow_size <= 10,
            "connection_lines.glow_size must be between 0 and 10"
        );
        Ok(())
    }
}

fn default_connection_lines_enabled() -> bool {
    DEFAULT_CONNECTION_LINES_ENABLED
}

fn default_connection_lines_color() -> ColorDef {
    ColorDef::from_rgb(0x00, 0xFF, 0x00)
}

fn default_connection_line_glow() -> u8 {
    DEFAULT_CONNECTION_LINE_GLOW
}

/// Color theme.  All values are stored as `ColorDef` (hex #RRGGBB[AA]).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ColorConfig {
    #[serde(default = "default_info")]
    pub info: ColorDef,
    #[serde(default = "default_warn")]
    pub warn: ColorDef,
    #[serde(default = "default_alert")]
    pub alert: ColorDef,
    #[serde(default = "default_focus")]
    pub focus: ColorDef,
    #[serde(default = "default_dim")]
    pub dim: ColorDef,
    #[serde(default = "default_home")]
    pub home: ColorDef,
    #[serde(default = "default_ocean")]
    pub ocean: ColorDef,
    #[serde(default = "default_land")]
    pub land: ColorDef,
}

impl Default for ColorConfig {
    fn default() -> Self {
        Self {
            info: default_info(),
            warn: default_warn(),
            alert: default_alert(),
            focus: default_focus(),
            dim: default_dim(),
            home: default_home(),
            ocean: default_ocean(),
            land: default_land(),
        }
    }
}

fn default_info() -> ColorDef {
    ColorDef::from_rgb(0x50, 0xDC, 0x78)
}
fn default_warn() -> ColorDef {
    ColorDef::from_rgb(0xFA, 0xC8, 0x32)
}
fn default_alert() -> ColorDef {
    ColorDef::from_rgb(0xF0, 0x3C, 0x3C)
}
fn default_focus() -> ColorDef {
    ColorDef::from_rgb(0x00, 0xFF, 0xFF)
}
fn default_dim() -> ColorDef {
    ColorDef::from_rgb(0x55, 0x55, 0x55)
}
fn default_home() -> ColorDef {
    ColorDef::from_rgb(0x3C, 0xB4, 0xFF)
}
fn default_ocean() -> ColorDef {
    ColorDef::from_rgb(0x0A, 0x12, 0x1E)
}
fn default_land() -> ColorDef {
    ColorDef::from_rgb(0x23, 0x3E, 0x5F)
}

/// Font settings.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct FontConfig {
    /// Optional TUI terminal font width override.
    #[serde(default)]
    pub tui_font_width: Option<u16>,
    /// Optional TUI terminal font height override.
    #[serde(default)]
    pub tui_font_height: Option<u16>,
    /// GUI body text size.
    #[serde(default = "default_gui_body")]
    pub gui_body: f32,
    /// GUI heading text size.
    #[serde(default = "default_gui_heading")]
    pub gui_heading: f32,
    /// Optional custom font file for the GUI.
    #[serde(default)]
    pub gui_font_file: Option<PathBuf>,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            tui_font_width: None,
            tui_font_height: None,
            gui_body: default_gui_body(),
            gui_heading: default_gui_heading(),
            gui_font_file: None,
        }
    }
}

impl FontConfig {
    fn validate(&self) -> Result<()> {
        if let (Some(w), Some(h)) = (self.tui_font_width, self.tui_font_height) {
            anyhow::ensure!(w > 0 && h > 0, "tui_font_width/height must be > 0");
        }
        anyhow::ensure!(self.gui_body > 0.0, "fonts.gui_body must be > 0");
        anyhow::ensure!(self.gui_heading > 0.0, "fonts.gui_heading must be > 0");
        if let Some(ref p) = self.gui_font_file {
            anyhow::ensure!(
                p.exists(),
                "fonts.gui_font_file does not exist: {}",
                p.display()
            );
        }
        Ok(())
    }
}

fn default_gui_body() -> f32 {
    14.0
}
fn default_gui_heading() -> f32 {
    20.0
}

/// GUI window geometry.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
pub struct WindowConfig {
    #[serde(default = "default_window_width")]
    pub width: u32,
    #[serde(default = "default_window_height")]
    pub height: u32,
    #[serde(default = "default_window_min_width")]
    pub min_width: u32,
    #[serde(default = "default_window_min_height")]
    pub min_height: u32,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            width: default_window_width(),
            height: default_window_height(),
            min_width: default_window_min_width(),
            min_height: default_window_min_height(),
        }
    }
}

impl WindowConfig {
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.width >= 100, "window.width must be >= 100");
        anyhow::ensure!(self.height >= 100, "window.height must be >= 100");
        anyhow::ensure!(
            self.min_width <= self.width && self.min_height <= self.height,
            "window min dimensions must not exceed the default dimensions"
        );
        Ok(())
    }
}

fn default_window_width() -> u32 {
    1280
}
fn default_window_height() -> u32 {
    720
}
fn default_window_min_width() -> u32 {
    640
}
fn default_window_min_height() -> u32 {
    360
}

/// Hot-reload diff report.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ConfigDiff {
    pub marker_ttl: bool,
    pub max_markers: bool,
    pub marker_style: bool,
    pub home: bool,
    pub connection_lines: bool,
    pub labels: bool,
    pub colors: bool,
    pub tui_font: bool,
    pub gui_font: bool,
    pub window: bool,
}

impl ConfigDiff {
    /// True if any changed field requires an application restart to take
    /// full effect.
    /// True if anything at all changed.
    pub fn any(&self) -> bool {
        self.marker_ttl
            || self.max_markers
            || self.marker_style
            || self.home
            || self.connection_lines
            || self.labels
            || self.colors
            || self.tui_font
            || self.gui_font
            || self.window
    }

    /// Log a human-readable summary of what changed and whether a restart is
    /// recommended.
    pub fn log(&self) {
        if !self.any() {
            info!("config reloaded: no changes detected");
            return;
        }

        let mut parts = Vec::new();
        if self.marker_ttl {
            parts.push("marker_ttl");
        }
        if self.max_markers {
            parts.push("max_markers");
        }
        if self.marker_style {
            parts.push("marker_style");
        }
        if self.home {
            parts.push("home");
        }
        if self.connection_lines {
            parts.push("connection_lines");
        }
        if self.labels {
            parts.push("labels");
        }
        if self.colors {
            parts.push("colors");
        }
        if self.tui_font {
            parts.push("tui_font");
        }
        if self.gui_font {
            parts.push("gui_font");
        }
        if self.window {
            parts.push("window");
        }

        info!(
            changed = parts.join(", "),
            "config reloaded; changes are live"
        );
    }
}

/// A color value parsed from a hex string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorDef {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl ColorDef {
    pub const fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub fn to_ratatui(self) -> ratatui::style::Color {
        ratatui::style::Color::Rgb(self.r, self.g, self.b)
    }

    pub fn to_rgba(self, alpha: u8) -> image::Rgba<u8> {
        image::Rgba([self.r, self.g, self.b, alpha])
    }

    pub fn to_egui(self) -> eframe::egui::Color32 {
        eframe::egui::Color32::from_rgb(self.r, self.g, self.b)
    }
}

impl Serialize for ColorDef {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&format!("#{:02X}{:02X}{:02X}", self.r, self.g, self.b))
    }
}

impl<'de> Deserialize<'de> for ColorDef {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(ColorVisitor)
    }
}

struct ColorVisitor;

impl<'de> Visitor<'de> for ColorVisitor {
    type Value = ColorDef;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a hex color string like #RRGGBB or #RGB")
    }

    fn visit_str<E>(self, v: &str) -> std::result::Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        parse_hex_color(v).map_err(|e| E::custom(e))
    }
}

fn parse_hex_color(s: &str) -> Result<ColorDef> {
    let s = s.trim();
    if !s.starts_with('#') {
        anyhow::bail!("hex color must start with '#': {s}");
    }
    let hex = &s[1..];
    match hex.len() {
        3 => {
            let r = u8::from_str_radix(&hex[0..1].repeat(2), 16)?;
            let g = u8::from_str_radix(&hex[1..2].repeat(2), 16)?;
            let b = u8::from_str_radix(&hex[2..3].repeat(2), 16)?;
            Ok(ColorDef { r, g, b })
        }
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16)?;
            let g = u8::from_str_radix(&hex[2..4], 16)?;
            let b = u8::from_str_radix(&hex[4..6], 16)?;
            Ok(ColorDef { r, g, b })
        }
        4 | 8 => {
            // Ignore alpha for now; we compute our own alpha per-frame.
            let rgb = if hex.len() == 4 {
                &hex[0..3]
            } else {
                &hex[0..6]
            };
            parse_hex_color(&format!("#{rgb}"))
        }
        _ => anyhow::bail!("invalid hex color length: {s}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_parsing() {
        assert_eq!(
            ColorDef::from_rgb(0x50, 0xDC, 0x78),
            parse_hex_color("#50DC78").unwrap()
        );
        assert_eq!(
            ColorDef::from_rgb(0xFF, 0x00, 0xAA),
            parse_hex_color("#F0A").unwrap()
        );
        assert_eq!(
            ColorDef::from_rgb(0x50, 0xDC, 0x78),
            parse_hex_color("#50DC78FF").unwrap()
        );
    }

    #[test]
    fn defaults_round_trip() {
        let json = serde_json::to_string(&Config::default()).unwrap();
        let parsed: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.marker_ttl_seconds, 8);
        assert_eq!(parsed.colors.info, ColorDef::from_rgb(0x50, 0xDC, 0x78));
    }

    #[test]
    fn example_config_loads() {
        // Embed the example config at compile time so the test does not depend
        // on the source tree / CARGO_MANIFEST_DIR being present (works for
        // `cargo test` on a published crate too).
        const EXAMPLE: &str = include_str!("../assets/config.example.json");
        let cfg = Config::read_str(EXAMPLE).expect("example config should parse and validate");
        assert_eq!(cfg.marker_ttl_seconds, 8);
        assert_eq!(cfg.colors.info, ColorDef::from_rgb(0x50, 0xDC, 0x78));
    }
}

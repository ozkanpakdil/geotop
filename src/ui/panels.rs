//! Concrete panel renderers: map, live log, top-talkers/metrics, footer.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use parking_lot::RwLock;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Sparkline,
};
use ratatui::Frame;
use ratatui_image::protocol::Protocol;

use crate::config::Config;
use crate::ui::app::{AppState, LogRow, Panel};
use crate::ui::layout::DashboardAreas;

/// Render the entire dashboard into the frame.
pub fn render(
    f: &mut Frame<'_>,
    state: &AppState,
    areas: DashboardAreas,
    map_protocol: &Protocol,
    map_inner: Rect,
    cfg: Arc<RwLock<Config>>,
    home: &crate::ui::map_renderer::HomeLocation,
    no_map: bool,
) {
    let colors = cfg.read().colors.clone();
    if !no_map {
        render_map(f, state, areas.map, map_protocol, map_inner, &colors, home);
    }
    render_metrics(f, state, areas, &colors);
    render_log(f, state, areas.log, &colors);
    render_footer(f, state, areas.footer, &colors);
}

// ---------------------------------------------------------------------------
// Map panel
// ---------------------------------------------------------------------------
fn render_map(
    f: &mut Frame<'_>,
    state: &AppState,
    area: Rect,
    map_protocol: &Protocol,
    inner: Rect,
    colors: &crate::config::ColorConfig,
    home: &crate::ui::map_renderer::HomeLocation,
) {
    let color_info = colors.info.to_ratatui();
    let color_alert = colors.alert.to_ratatui();
    let color_focus = colors.focus.to_ratatui();
    let color_dim = colors.dim.to_ratatui();

    let focused = *state.focused.lock();
    let title_style = if focused == Panel::Map {
        Style::default()
            .fg(color_focus)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(vec![Span::styled(
            " 🌍 World Map ",
            title_style,
        )]));
    f.render_widget(block, area);

    // Stateless Image widget fed by the pre-encoded protocol. Re-encoding
    // is done in `main.rs` once per frame. `allow_clipping` prevents the
    // widget from silently dropping the image if the protocol size differs
    // from the inner area by a cell.
    f.render_widget(
        ratatui_image::Image::new(map_protocol).allow_clipping(true),
        inner,
    );

    // Overlay: connection count and home info.
    let dots = state.dots.lock();
    let total = dots.len();
    drop(dots);
    let counts = state.stats.lock();
    let home_text = home.overlay_text();
    let overlay = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(" ● ", Style::default().fg(color_info)),
            Span::styled(
                format!("{total} active "),
                Style::default().fg(Color::White),
            ),
            Span::styled(
                format!("{:.1} evt/s ", counts.events_per_sec),
                Style::default().fg(Color::White),
            ),
            Span::styled(
                format!("Proxy: {:.0}% ", counts.proxy_pct),
                Style::default().fg(color_alert),
            ),
        ]),
        Line::from(vec![Span::styled(home_text, Style::default().fg(color_dim))]),
    ]);
    let overlay_area = Rect {
        x: inner.x + 1,
        y: inner.y,
        width: inner.width.saturating_sub(2),
        height: 2,
    };
    if overlay_area.width > 0 {
        f.render_widget(Clear, overlay_area);
        f.render_widget(overlay, overlay_area);
    }
}

// ---------------------------------------------------------------------------
// Metrics panel (right column)
// ---------------------------------------------------------------------------
fn render_metrics(
    f: &mut Frame<'_>,
    state: &AppState,
    areas: DashboardAreas,
    colors: &crate::config::ColorConfig,
) {
    let color_info = colors.info.to_ratatui();
    let color_focus = colors.focus.to_ratatui();
    let color_dim = colors.dim.to_ratatui();

    let focused = *state.focused.lock();
    let border_color = if focused == Panel::Metrics {
        color_focus
    } else {
        Color::White
    };

    let mut country_counts: Vec<(String, u64)> = state
        .per_country
        .iter()
        .map(|kv| (kv.key().clone(), *kv.value()))
        .collect();
    country_counts.sort_by(|a, b| b.1.cmp(&a.1));
    country_counts.truncate(10);

    let max = country_counts.first().map(|x| x.1).unwrap_or(0).max(1);

    let items: Vec<ListItem> = if country_counts.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            " waiting for events…",
            Style::default().fg(color_dim),
        )))]
    } else {
        country_counts
            .into_iter()
            .map(|(cc, n)| {
                let bar = "█".repeat((n * 18 / max.max(1)) as usize);
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {cc:<3} "), Style::default().fg(Color::White)),
                    Span::styled(bar, Style::default().fg(color_info)),
                    Span::styled(format!(" {n}"), Style::default().fg(color_dim)),
                ]))
            })
            .collect()
    };

    let top = List::new(items).block(Block::default().borders(Borders::ALL).title(Span::styled(
        " 📊 Top Talkers (country) ",
        Style::default().fg(border_color),
    )));
    f.render_widget(top, areas.metrics_top);

    let ring = state.throughput_ring.lock();
    let spark_data: Vec<u64> = ring.iter().copied().collect();
    drop(ring);

    let sparkline = Sparkline::default()
        .block(Block::default().borders(Borders::ALL).title(Span::styled(
            " 📈 Packets / sec (60s) ",
            Style::default().fg(Color::White),
        )))
        .data(&spark_data)
        .style(Style::default().fg(color_info));
    f.render_widget(sparkline, areas.metrics_bottom);

    let counts = state.stats.lock();
    let proxy_label = format!(
        "Proxy {p:.0}%  Datacenter {d:.0}%  Tor {t:.0}%",
        p = counts.proxy_pct,
        d = counts.datacenter_pct,
        t = counts.tor_pct,
    );
    drop(counts);
    let color_alert = colors.alert.to_ratatui();
    let gauge = Gauge::default()
        .gauge_style(Style::default().fg(color_alert).bg(Color::Black))
        .ratio((proxy_pct(state) / 100.0).clamp(0.0, 1.0))
        .label(Span::styled(proxy_label, Style::default().fg(Color::White)));
    let gauge_area = Rect {
        x: areas.metrics_bottom.x,
        y: areas.metrics_bottom.y + areas.metrics_bottom.height.saturating_sub(1),
        width: areas.metrics_bottom.width,
        height: 1,
    };
    if gauge_area.y < f.area().height && gauge_area.width > 0 {
        f.render_widget(Clear, gauge_area);
        f.render_widget(gauge, gauge_area);
    }
}

fn proxy_pct(state: &AppState) -> f64 {
    state.stats.lock().proxy_pct
}

// ---------------------------------------------------------------------------
// Live log panel (bottom-left)
// ---------------------------------------------------------------------------
fn render_log(
    f: &mut Frame<'_>,
    state: &AppState,
    area: Rect,
    colors: &crate::config::ColorConfig,
) {
    let color_focus = colors.focus.to_ratatui();
    let focused = *state.focused.lock();
    let border_color = if focused == Panel::Log {
        color_focus
    } else {
        Color::White
    };

    let log = state.log.lock();
    let per_row_inner = area.height.saturating_sub(2) as usize;
    let total = log.len();
    let start = total.saturating_sub(per_row_inner);

    let mut state_log = state.log_state.lock();
    let visible: Vec<ListItem> = log
        .iter()
        .skip(start)
        .map(|r| log_row_item(r, colors))
        .collect();

    let visible_len = visible.len();
    let list = List::new(visible)
        .block(Block::default().borders(Borders::ALL).title(Span::styled(
            " 📜 Live Connections (Tab=focus  ↑/↓=scroll) ",
            Style::default().fg(border_color),
        )))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    let mut s: ListState = state_log.clone();
    f.render_stateful_widget(list, area, &mut s);
    s.select(Some(visible_len.saturating_sub(1)));
    *state_log = s;
    drop(state_log);
    drop(log);
}

fn log_row_item<'a>(r: &'a LogRow, colors: &crate::config::ColorConfig) -> ListItem<'a> {
    let color_info = colors.info.to_ratatui();
    let color_warn = colors.warn.to_ratatui();
    let color_alert = colors.alert.to_ratatui();
    let color_dim = colors.dim.to_ratatui();

    let proxy = r.proxy.map(|p| p.label()).unwrap_or("    ");
    let city = if r.city.is_empty() { "—" } else { &r.city };
    let cc = if r.country.is_empty() {
        "—"
    } else {
        &r.country
    };
    let sev_color = match r.severity {
        crate::event::Severity::Info => color_info,
        crate::event::Severity::Warn => color_warn,
        crate::event::Severity::Alert => color_alert,
    };
    let status = r
        .http_status
        .map(|s| format!("{s}"))
        .unwrap_or_else(|| "   ".into());
    let path = r
        .http_path
        .as_deref()
        .map(|p| if p.len() > 30 { &p[..30] } else { p })
        .unwrap_or("");
    let line = Line::from(vec![
        Span::styled(
            format!(" {} ", r.timestamp.format("%H:%M:%S")),
            Style::default().fg(color_dim),
        ),
        Span::styled(
            format!("{:<15}", r.src_ip),
            Style::default().fg(Color::White),
        ),
        Span::styled(
            format!(" [{proxy}] "),
            Style::default().fg(sev_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {cc:<14}"), Style::default().fg(Color::White)),
        Span::styled(format!(" {city:<14}"), Style::default().fg(color_dim)),
        Span::styled(format!(" {status:<3} "), Style::default().fg(sev_color)),
        Span::styled(format!(" {path}"), Style::default().fg(Color::White)),
    ]);
    ListItem::new(line)
}

// ---------------------------------------------------------------------------
// Footer
// ---------------------------------------------------------------------------
fn render_footer(
    f: &mut Frame<'_>,
    state: &AppState,
    area: Rect,
    colors: &crate::config::ColorConfig,
) {
    let color_dim = colors.dim.to_ratatui();
    let color_focus = colors.focus.to_ratatui();
    let color_alert = colors.alert.to_ratatui();

    let counts = state.stats.lock();
    let total = counts.total_events;
    let bytes = counts.bytes_total;
    let events_per_sec = counts.events_per_sec;
    let paused = state.is_paused();
    drop(counts);

    let human = human_bytes(bytes);
    let mut spans = vec![
        Span::styled(" ⌨  ", Style::default().fg(color_dim)),
        Span::styled("[Tab]", Style::default().fg(color_focus)),
        Span::styled(" focus  ", Style::default().fg(color_dim)),
        Span::styled("[p]", Style::default().fg(color_focus)),
        Span::styled(" pause  ", Style::default().fg(color_dim)),
        Span::styled("[c]", Style::default().fg(color_focus)),
        Span::styled(" clear  ", Style::default().fg(color_dim)),
        Span::styled("[l]", Style::default().fg(color_focus)),
        Span::styled(
            format!(
                " lines:{} ",
                if state.connection_lines.load(Ordering::Relaxed) {
                    "on"
                } else {
                    "off"
                }
            ),
            Style::default().fg(color_dim),
        ),
        Span::styled("[m]", Style::default().fg(color_focus)),
        Span::styled(
            format!(
                " map:{} ",
                if state.map_hidden.load(Ordering::Relaxed) {
                    "off"
                } else {
                    "on"
                }
            ),
            Style::default().fg(color_dim),
        ),
        Span::styled("[q/Esc]", Style::default().fg(color_focus)),
        Span::styled(" quit  ", Style::default().fg(color_dim)),
        Span::styled(" │ ", Style::default().fg(color_dim)),
        Span::styled(
            format!("events: {total} | {human} | {events_per_sec:.1}/s"),
            Style::default().fg(Color::White),
        ),
    ];
    if paused {
        spans.insert(
            11,
            Span::styled(
                " ⏸ PAUSED ",
                Style::default()
                    .fg(color_alert)
                    .add_modifier(Modifier::BOLD),
            ),
        );
    }
    let p = Paragraph::new(Line::from(spans));
    f.render_widget(p, area);
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

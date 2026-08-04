//! Concrete panel renderers: map, live log, top-talkers/metrics, footer.

use image::RgbaImage;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Sparkline,
};
use ratatui::Frame;
use ratatui_image::protocol::Protocol;

use crate::ui::app::{AppState, LogRow, Panel};
use crate::ui::layout::DashboardAreas;

/// Highlight colors for severities.
const COLOR_INFO: Color = Color::Green;
const COLOR_WARN: Color = Color::Yellow;
const COLOR_ALERT: Color = Color::Red;
const COLOR_FOCUS: Color = Color::Cyan;
const COLOR_DIM: Color = Color::DarkGray;

/// Render the entire dashboard into the frame.
pub fn render(
    f: &mut Frame<'_>,
    state: &AppState,
    areas: DashboardAreas,
    _img: RgbaImage,
    map_protocol: &Protocol,
    map_inner: Rect,
) {
    render_map(f, state, areas.map, map_protocol, map_inner);
    render_metrics(f, state, areas);
    render_log(f, state, areas.log);
    render_footer(f, state, areas.footer);
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
) {
    let focused = *state.focused.lock();
    let title_style = if focused == Panel::Map {
        Style::default().fg(COLOR_FOCUS).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(vec![Span::styled(" 🌍 World Map ", title_style)]));
    f.render_widget(block, area);

    // Stateless Image widget fed by the pre-encoded protocol. Re-encoding
    // is done in `main.rs` once per frame. `allow_clipping` prevents the
    // widget from silently dropping the image if the protocol size differs
    // from the inner area by a cell.
    f.render_widget(ratatui_image::Image::new(map_protocol).allow_clipping(true), inner);

    // Overlay: connection count.
    let dots = state.dots.lock();
    let total = dots.len();
    drop(dots);
    let counts = state.stats.lock();
    let overlay = Paragraph::new(Line::from(vec![
        Span::styled(" ● ", Style::default().fg(COLOR_INFO)),
        Span::styled(format!("{total} active "), Style::default().fg(Color::White)),
        Span::styled(
            format!("{:.1} evt/s ", counts.events_per_sec),
            Style::default().fg(Color::White),
        ),
        Span::styled(
            format!("Proxy: {:.0}% ", counts.proxy_pct),
            Style::default().fg(COLOR_ALERT),
        ),
    ]));
    let overlay_area = Rect {
        x: inner.x + 1,
        y: inner.y,
        width: inner.width.saturating_sub(2),
        height: 1,
    };
    if overlay_area.width > 0 {
        f.render_widget(Clear, overlay_area);
        f.render_widget(overlay, overlay_area);
    }
}

// ---------------------------------------------------------------------------
// Metrics panel (right column)
// ---------------------------------------------------------------------------
fn render_metrics(f: &mut Frame<'_>, state: &AppState, areas: DashboardAreas) {
    let focused = *state.focused.lock();
    let border_color = if focused == Panel::Metrics {
        COLOR_FOCUS
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
            Style::default().fg(COLOR_DIM),
        )))]
    } else {
        country_counts
            .into_iter()
            .map(|(cc, n)| {
                let bar = "█".repeat((n * 18 / max.max(1)) as usize);
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {cc:<3} "), Style::default().fg(Color::White)),
                    Span::styled(bar, Style::default().fg(COLOR_INFO)),
                    Span::styled(format!(" {n}"), Style::default().fg(COLOR_DIM)),
                ]))
            })
            .collect()
    };

    let top = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(
                " 📊 Top Talkers (country) ",
                Style::default().fg(border_color),
            )),
    );
    f.render_widget(top, areas.metrics_top);

    let ring = state.throughput_ring.lock();
    let spark_data: Vec<u64> = ring.iter().copied().collect();
    drop(ring);

    let sparkline = Sparkline::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(
                    " 📈 Packets / sec (60s) ",
                    Style::default().fg(Color::White),
                )),
        )
        .data(&spark_data)
        .style(Style::default().fg(COLOR_INFO));
    f.render_widget(sparkline, areas.metrics_bottom);

    let counts = state.stats.lock();
    let proxy_label = format!(
        "Proxy {p:.0}%  Datacenter {d:.0}%  Tor {t:.0}%",
        p = counts.proxy_pct,
        d = counts.datacenter_pct,
        t = counts.tor_pct,
    );
    drop(counts);
    let gauge = Gauge::default()
        .gauge_style(Style::default().fg(COLOR_ALERT).bg(Color::Black))
        .ratio((proxy_pct(state) / 100.0).clamp(0.0, 1.0))
        .label(Span::styled(
            proxy_label,
            Style::default().fg(Color::White),
        ));
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
fn render_log(f: &mut Frame<'_>, state: &AppState, area: Rect) {
    let focused = *state.focused.lock();
    let border_color = if focused == Panel::Log {
        COLOR_FOCUS
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
        .map(|r| log_row_item(r))
        .collect();

    let visible_len = visible.len();
    let list = List::new(visible)
        .block(
            Block::default().borders(Borders::ALL).title(Span::styled(
                " 📜 Live Connections (Tab=focus  ↑/↓=scroll) ",
                Style::default().fg(border_color),
            )),
        )
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

fn log_row_item(r: &LogRow) -> ListItem<'_> {
    let proxy = r.proxy.map(|p| p.label()).unwrap_or("    ");
    let city = if r.city.is_empty() { "—" } else { &r.city };
    let cc = if r.country.is_empty() { "—" } else { &r.country };
    let sev_color = match r.severity {
        crate::event::Severity::Info => COLOR_INFO,
        crate::event::Severity::Warn => COLOR_WARN,
        crate::event::Severity::Alert => COLOR_ALERT,
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
            Style::default().fg(COLOR_DIM),
        ),
        Span::styled(
            format!("{:<15}", r.src_ip),
            Style::default().fg(Color::White),
        ),
        Span::styled(
            format!(" [{proxy}] "),
            Style::default().fg(sev_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {cc:<14}"),
            Style::default().fg(Color::White),
        ),
        Span::styled(
            format!(" {city:<14}"),
            Style::default().fg(COLOR_DIM),
        ),
        Span::styled(
            format!(" {status:<3} "),
            Style::default().fg(sev_color),
        ),
        Span::styled(format!(" {path}"), Style::default().fg(Color::White)),
    ]);
    ListItem::new(line)
}

// ---------------------------------------------------------------------------
// Footer
// ---------------------------------------------------------------------------
fn render_footer(f: &mut Frame<'_>, state: &AppState, area: Rect) {
    let counts = state.stats.lock();
    let total = counts.total_events;
    let bytes = counts.bytes_total;
    let events_per_sec = counts.events_per_sec;
    let paused = state.is_paused();
    drop(counts);

    let human = human_bytes(bytes);
    let mut spans = vec![
        Span::styled(" ⌨  ", Style::default().fg(Color::DarkGray)),
        Span::styled("[Tab]", Style::default().fg(Color::Cyan)),
        Span::styled(" focus  ", Style::default().fg(Color::DarkGray)),
        Span::styled("[p]", Style::default().fg(Color::Cyan)),
        Span::styled(" pause  ", Style::default().fg(Color::DarkGray)),
        Span::styled("[c]", Style::default().fg(Color::Cyan)),
        Span::styled(" clear  ", Style::default().fg(Color::DarkGray)),
        Span::styled("[q/Esc]", Style::default().fg(Color::Cyan)),
        Span::styled(" quit  ", Style::default().fg(Color::DarkGray)),
        Span::styled(" │ ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("events: {total} | {human} | {events_per_sec:.1}/s"),
            Style::default().fg(Color::White),
        ),
    ];
    if paused {
        spans.insert(
            9,
            Span::styled(
                " ⏸ PAUSED ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
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

//! Grid layout for the dashboard:
//!
//! ```text
//!   ┌──────────────────────────┬─────────────────────┐
//!   │                          │   Top Talkers       │
//!   │       World Map          │   + Metrics         │
//!   │                          ├─────────────────────┤
//!   │                          │   Proxy Breakdown   │
//!   ├──────────────────────────┴─────────────────────┤
//!   │       Live Connection Log                      │
//!   └────────────────────────────────────────────────┘
//! ```
//!
//! When `no_map` is true the map panel is removed and the log panel fills the
//! entire left column.

use ratatui::layout::{Constraint, Direction, Layout, Rect};

#[derive(Debug, Clone, Copy)]
pub struct DashboardAreas {
    pub map: Rect,
    pub metrics_top: Rect,
    pub metrics_bottom: Rect,
    pub log: Rect,
    pub footer: Rect,
}

pub fn dashboard(frame_size: Rect, no_map: bool) -> DashboardAreas {
    // Outer: vertical split – main body, footer.
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(8),    // main dashboard
            Constraint::Length(1), // footer / status
        ])
        .split(frame_size);

    // Body: horizontal split – main column (map + log), metrics column.
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(72), Constraint::Percentage(28)])
        .split(outer[0]);

    // Main column: map (top) + log (bottom), or just log when no_map is set.
    let main_col = if no_map {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(100)])
            .split(body[0])
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(8), Constraint::Length(10)])
            .split(body[0])
    };

    // Metrics column: stacked – top talkers, then proxy breakdown.
    let metrics_col = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(body[1]);

    DashboardAreas {
        map: if no_map { Rect::default() } else { main_col[0] },
        metrics_top: metrics_col[0],
        metrics_bottom: metrics_col[1],
        log: main_col[if no_map { 0 } else { 1 }],
        footer: outer[1],
    }
}

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

use ratatui::layout::{Constraint, Direction, Layout, Rect};

#[derive(Debug, Clone, Copy)]
pub struct DashboardAreas {
    pub map: Rect,
    pub metrics_top: Rect,
    pub metrics_bottom: Rect,
    pub log: Rect,
    pub footer: Rect,
}

pub fn dashboard(frame_size: Rect) -> DashboardAreas {
    // Outer: vertical split – main body, footer.
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(8),     // main dashboard
            Constraint::Length(1),  // footer / status
        ])
        .split(frame_size);

    // Body: horizontal split – main column (map + log), metrics column.
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(72), Constraint::Percentage(28)])
        .split(outer[0]);

    // Main column: map (top) + log (bottom).
    let main_col = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(8), Constraint::Length(10)])
        .split(body[0]);

    // Metrics column: stacked – top talkers, then proxy breakdown.
    let metrics_col = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(body[1]);

    DashboardAreas {
        map: main_col[0],
        metrics_top: metrics_col[0],
        metrics_bottom: metrics_col[1],
        log: main_col[1],
        footer: outer[1],
    }
}

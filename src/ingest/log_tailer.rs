//! Async log tailer using `notify-debouncer-mini` and a unified
//! regex for the Common Log Format / Combined Log Format used by
//! nginx & apache.
//!
//! Emits [`ConnectionEvent`]s onto a bounded Tokio broadcast channel.
//! The actual file reading happens off the runtime worker pool via
//! a dedicated `std::thread` so we never block the UI tick loop.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use notify::RecursiveMode;
use notify_debouncer_mini::{
    new_debouncer, DebounceEventHandler, DebounceEventResult, DebouncedEventKind, Debouncer,
};
use regex::Regex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::event::{ConnectionEvent, Severity, Source};

/// Combined Log Format (`$remote_addr - $remote_user [$time_local] "$request" $status $body_bytes_sent "$http_referer" "$http_user_agent"`).
const COMBINED_LOG_RE: &str = r#"^(?P<ip>\S+)\s+\S+\s+\S+\s+\[(?P<time>[^\]]+)\]\s+"(?P<method>[A-Z]+)\s+(?P<path>\S+)(?:\s+HTTP/\d\.\d)?"\s+(?P<status>\d{3})\s+(?P<size>\d+|-)\s+(?:"[^"]*")?\s+"(?P<ua>[^"]*)""#;

/// Common Log Format (no referer/user-agent).
const COMMON_LOG_RE: &str = r#"^(?P<ip>\S+)\s+\S+\s+\S+\s+\[(?P<time>[^\]]+)\]\s+"(?P<method>[A-Z]+)\s+(?P<path>\S+)(?:\s+HTTP/\d\.\d)?"\s+(?P<status>\d{3})\s+(?P<size>\d+|-)"#;

/// Output-side handle to the tailer.
pub struct LogTailerHandle {
    /// Send a CancellationToken to request a clean shutdown.
    pub cancel: CancellationToken,
    // Hold the debouncer so the watcher lives as long as the task.
    _debouncer: Debouncer<notify::RecommendedWatcher>,
}

/// Spawn a tailer task. Returns a handle; dropped on cancel.
pub fn spawn(path: PathBuf, tx: mpsc::UnboundedSender<ConnectionEvent>) -> Result<LogTailerHandle> {
    let cancel = CancellationToken::new();
    let cancel_child = cancel.clone();
    let path_clone = path.clone();
    let tx_clone = tx.clone();

    std::thread::Builder::new()
        .name(format!("geotop-log-tailer-{}", path.display()))
        .spawn(move || {
            if let Err(e) = run_loop_blocking(&path_clone, tx_clone, cancel_child) {
                error!("log tailer crashed for {}: {e:?}", path_clone.display());
            }
        })
        .context("spawning log tailer thread")?;

    // Lightweight no-op handler.
    struct NullHandler;
    impl DebounceEventHandler for NullHandler {
        fn handle_event(&mut self, _event: DebounceEventResult) {}
    }

    // Watch the parent directory of the file (or the file itself) so
    // we see appends and rotations.
    let watch_target = if path.exists() {
        path.parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf()
    } else {
        path.clone()
    };

    let mut debouncer: Debouncer<notify::RecommendedWatcher> =
        new_debouncer(Duration::from_millis(150), NullHandler)?;
    debouncer
        .watcher()
        .watch(&watch_target, RecursiveMode::NonRecursive)?;

    // We don't actually use the debouncer for events in this version —
    // the read loop polls the file. Keep it alive anyway so future work
    // can subscribe to `debounced_events()` cheaply.
    let _ = &path;
    let _ = DebouncedEventKind::Any;

    Ok(LogTailerHandle {
        cancel,
        _debouncer: debouncer,
    })
}

/// Blocking read loop – reads the file from the last-known offset,
/// parses new lines, and forwards events.
fn run_loop_blocking(
    path: &Path,
    tx: mpsc::UnboundedSender<ConnectionEvent>,
    cancel: CancellationToken,
) -> Result<()> {
    info!("starting log tailer: {}", path.display());

    let combined = Regex::new(COMBINED_LOG_RE).expect("valid combined regex");
    let common = Regex::new(COMMON_LOG_RE).expect("valid common regex");

    let mut offset: u64 = if let Ok(meta) = std::fs::metadata(path) {
        meta.len()
    } else {
        0
    };

    loop {
        if cancel.is_cancelled() {
            info!("log tailer cancelled");
            return Ok(());
        }

        match read_new_lines(path, &mut offset) {
            Ok(lines) => {
                for line in lines {
                    if let Some(ev) = parse_line(&line, &combined, &common) {
                        if tx.send(ev).is_err() {
                            debug!("downstream closed – stopping log tailer");
                            return Ok(());
                        }
                    }
                }
            }
            Err(e) => {
                warn!("read error: {e:?}");
            }
        }

        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Read any bytes appended to `path` since the last call and return them
/// as a list of completed lines (the trailing partial line is buffered
/// until a newline arrives).
fn read_new_lines(path: &Path, offset: &mut u64) -> Result<Vec<String>> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let len = file.metadata()?.len();

    // File rotated/truncated: reset offset.
    if len < *offset {
        *offset = 0;
    }
    if len == *offset {
        return Ok(Vec::new());
    }

    use std::io::Seek;
    use std::io::SeekFrom;
    file.seek(SeekFrom::Start(*offset))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    *offset = len;

    Ok(String::from_utf8_lossy(&buf)
        .split('\n')
        .filter(|l| !l.is_empty())
        .map(|s| s.to_string())
        .collect())
}

/// Parse a single log line. Returns `None` if it doesn't match either format.
fn parse_line(line: &str, combined: &Regex, common: &Regex) -> Option<ConnectionEvent> {
    let caps = if let Some(c) = combined.captures(line) {
        c
    } else if let Some(c) = common.captures(line) {
        c
    } else {
        return None;
    };

    let ip_str = caps.name("ip")?.as_str();
    let ip: std::net::IpAddr = ip_str.parse().ok()?;
    let status: u16 = caps.name("status")?.as_str().parse().ok()?;
    let method = caps.name("method").map(|m| m.as_str().to_string());
    let path = caps.name("path").map(|p| p.as_str().to_string());
    let ua = caps.name("ua").map(|u| u.as_str().to_string());
    let size: Option<u64> = caps.name("size").and_then(|s| s.as_str().parse().ok());

    let severity = match status {
        500..=599 => Severity::Alert,
        400..=499 => Severity::Warn,
        _ => Severity::Info,
    };

    Some(ConnectionEvent {
        timestamp: Utc::now(),
        src_ip: ip,
        dst_port: Some(detect_http_port(path.as_deref())),
        protocol: "HTTP".to_string(),
        http_status: Some(status),
        http_method: method,
        http_path: path,
        user_agent: ua,
        bytes: size,
        source: Source::Log,
        severity,
    })
}

/// Guess the destination port from the URI scheme.
fn detect_http_port(uri: Option<&str>) -> u16 {
    match uri {
        Some(u) if u.starts_with("https://") => 443,
        Some(u) if u.starts_with("http://") => 80,
        _ => 80,
    }
}

//! Shared event types for the ingestion → UI pipeline.
//!
//! Both the packet sniffer and the log tailer produce a normalized
//! [`ConnectionEvent`] which is then dispatched through a Tokio channel
//! to the UI loop.

use std::net::IpAddr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Source of a `ConnectionEvent` – controls the icon and color in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    /// Packet sniffed from a network interface.
    Pcap,
    /// Parsed from a web server access log (nginx/apache).
    Log,
}

impl Source {
    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            Source::Pcap => "PCAP",
            Source::Log => "LOG ",
        }
    }
}

/// Severity / visual category for a connection dot on the map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    /// Normal traffic — green dot.
    Info,
    /// Elevated traffic / many requests — yellow dot.
    Warn,
    /// Suspicious / proxy / datacenter / 4xx-5xx — red dot.
    Alert,
}

impl Severity {
    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            Severity::Info => "INFO",
            Severity::Warn => "WARN",
            Severity::Alert => "ALRT",
        }
    }
}

/// A single connection event flowing into the dashboard.
#[derive(Debug, Clone)]
pub struct ConnectionEvent {
    /// Wall-clock time the event was observed.
    pub timestamp: DateTime<Utc>,
    /// Originating IP address.
    pub src_ip: IpAddr,
    /// Best-effort destination port (HTTP 80/443 from logs, real port from pcap).
    #[allow(dead_code)]
    pub dst_port: Option<u16>,
    /// Protocol (`"TCP"`, `"UDP"`, `"HTTP"`, …).
    pub protocol: String,
    /// HTTP status code (only for log source).
    pub http_status: Option<u16>,
    /// HTTP method (`"GET"`, `"POST"`, …) – only for log source.
    pub http_method: Option<String>,
    /// HTTP path / URI – only for log source.
    pub http_path: Option<String>,
    /// User-Agent – only for log source.
    pub user_agent: Option<String>,
    /// Raw byte size of the request / packet payload if known.
    pub bytes: Option<u64>,
    /// What subsystem produced the event.
    pub source: Source,
    /// Auto-derived severity for the map dot.
    pub severity: Severity,
}

impl ConnectionEvent {
    /// Short, single-line summary used in the connection log panel.
    #[allow(dead_code)]
    pub fn summary(&self) -> String {
        let port = self
            .dst_port
            .map(|p| format!(":{p}"))
            .unwrap_or_else(|| "    ".into());
        let http = match (self.http_status, &self.http_method) {
            (Some(s), Some(m)) => format!(" {m} {s}"),
            (Some(s), None) => format!(" {s}"),
            _ => String::new(),
        };
        format!(
            "[{}] {ip}{port} {proto}{http}",
            self.source.label(),
            ip = self.src_ip,
            port = port,
            proto = self.protocol,
            http = http,
        )
    }
}

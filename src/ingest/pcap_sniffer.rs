//! Packet sniffer built on top of `pnet`'s cross-platform datalink channel.
//!
//! We only inspect IPv4 / IPv6 + TCP / UDP headers (no payload), so this
//! has minimal overhead and works with raw sockets on Linux/macOS or
//! the BPF device on Windows. Requires `CAP_NET_RAW` / `sudo` on most
//! systems.

use std::net::IpAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use pnet::datalink::Channel::Ethernet;
use pnet::datalink::{self, NetworkInterface};
use pnet::ipnetwork::IpNetwork;
use pnet::packet::ethernet::EthernetPacket;
use pnet::packet::ip::{IpNextHeaderProtocol, IpNextHeaderProtocols};
use pnet::packet::ipv4::Ipv4Packet;
use pnet::packet::ipv6::Ipv6Packet;
use pnet::packet::tcp::TcpPacket;
use pnet::packet::udp::UdpPacket;
use pnet::packet::Packet;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::event::{ConnectionEvent, Severity, Source};

/// A handle to the running sniffer. Drop / cancel to stop.
pub struct SnifferHandle {
    pub cancel: CancellationToken,
}

/// Spawn a synchronous thread that owns `pnet`'s blocking datalink
/// channel and emits events on the unbounded Tokio channel.
pub fn spawn(
    interface: String,
    tx: mpsc::UnboundedSender<ConnectionEvent>,
) -> Result<SnifferHandle> {
    let cancel = CancellationToken::new();
    let cancel_child = cancel.clone();
    let tx_child = tx.clone();

    std::thread::Builder::new()
        .name(format!("geotop-pcap-{interface}"))
        .spawn(move || {
            if let Err(e) = run_blocking(&interface, tx_child, cancel_child) {
                error!("pcap sniffer crashed ({interface}): {e:?}");
            }
        })
        .context("spawning pcap sniffer thread")?;

    Ok(SnifferHandle { cancel })
}

/// Enumerate all non-loopback datalink interfaces (for `--list` mode).
pub fn list_interfaces() -> Vec<(String, Vec<IpNetwork>)> {
    datalink::interfaces()
        .into_iter()
        .filter(|i| !i.is_loopback() && !i.name.is_empty())
        .map(|i| (i.name, i.ips))
        .collect()
}

fn run_blocking(
    iface: &str,
    tx: mpsc::UnboundedSender<ConnectionEvent>,
    cancel: CancellationToken,
) -> Result<()> {
    let interfaces = datalink::interfaces();
    let target: NetworkInterface = interfaces
        .into_iter()
        .find(|i| i.name == iface)
        .with_context(|| format!("interface '{iface}' not found"))?;

    info!("opening datalink channel on {}", target.name);
    let config = datalink::Config::default();
    let (tx_link, mut rx_link) = match datalink::channel(&target, config) {
        Ok(Ethernet(tx, rx)) => (tx, rx),
        Ok(_) => anyhow::bail!("unsupported channel type"),
        Err(e) => anyhow::bail!("opening channel: {e}"),
    };

    // Hold tx so the OS doesn't disable promisc mode at GC time.
    let _keep_tx = Arc::new(tx_link);

    info!("listening on {} (idx {})", target.name, target.index);

    loop {
        if cancel.is_cancelled() {
            info!("pcap sniffer cancelled ({iface})");
            return Ok(());
        }

        match rx_link.next() {
            Ok(frame) => {
                if let Some(ev) = parse_frame(frame) {
                    if tx.send(ev).is_err() {
                        warn!("downstream closed – stopping pcap sniffer");
                        return Ok(());
                    }
                }
            }
            Err(e) => {
                warn!("recv error: {e:?}");
            }
        }
    }
}

/// Decode one ethernet frame into a `ConnectionEvent`. Returns `None`
/// for non-IP frames, malformed packets, or packets we don't care about.
fn parse_frame(frame: &[u8]) -> Option<ConnectionEvent> {
    let eth = EthernetPacket::new(frame)?;
    let src_ip: IpAddr = match eth.get_ethertype() {
        pnet::packet::ethernet::EtherTypes::Ipv4 => {
            let v4 = Ipv4Packet::new(eth.payload())?;
            let src = v4.get_source();
            IpAddr::V4(src.into())
        }
        pnet::packet::ethernet::EtherTypes::Ipv6 => {
            let v6 = Ipv6Packet::new(eth.payload())?;
            let src = v6.get_source();
            IpAddr::V6(src.into())
        }
        _ => return None,
    };

    // Re-decode to grab protocol + ports. We avoid building a giant
    // match tree on IpNextHeaderProtocol – just sniff the well-known
    // subset we care about.
    let (protocol, dst_port, severity) = match eth.get_ethertype() {
        pnet::packet::ethernet::EtherTypes::Ipv4 => {
            let v4 = Ipv4Packet::new(eth.payload())?;
            tcp_udp_info(
                IpNextHeaderProtocols::Tcp,
                v4.get_next_level_protocol(),
                v4.payload(),
            )
        }
        pnet::packet::ethernet::EtherTypes::Ipv6 => {
            let v6 = Ipv6Packet::new(eth.payload())?;
            tcp_udp_info(
                IpNextHeaderProtocols::Tcp,
                v6.get_next_header(),
                v6.payload(),
            )
        }
        _ => return None,
    };

    // Resolve "TCP"/"UDP" + port.
    let (proto_str, port, sev) = (protocol, dst_port, severity);

    Some(ConnectionEvent {
        timestamp: Utc::now(),
        src_ip,
        dst_port: port,
        protocol: proto_str,
        http_status: None,
        http_method: None,
        http_path: None,
        user_agent: None,
        bytes: Some(frame.len() as u64),
        source: Source::Pcap,
        severity: sev,
    })
}

fn tcp_udp_info(
    _ip_proto_any: IpNextHeaderProtocol,
    next: IpNextHeaderProtocol,
    payload: &[u8],
) -> (String, Option<u16>, Severity) {
    match next {
        IpNextHeaderProtocols::Tcp => {
            if let Some(tcp) = TcpPacket::new(payload) {
                let dst = tcp.get_destination();
                let sev = match dst {
                    22 | 3389 | 5900 => Severity::Warn, // common admin ports
                    23 | 445 | 1433 => Severity::Alert, // telnet / smb / mssql
                    _ => Severity::Info,
                };
                ("TCP".to_string(), Some(dst), sev)
            } else {
                ("TCP".to_string(), None, Severity::Info)
            }
        }
        IpNextHeaderProtocols::Udp => {
            if let Some(udp) = UdpPacket::new(payload) {
                (
                    "UDP".to_string(),
                    Some(udp.get_destination()),
                    Severity::Info,
                )
            } else {
                ("UDP".to_string(), None, Severity::Info)
            }
        }
        IpNextHeaderProtocols::Icmp | IpNextHeaderProtocols::Icmpv6 => {
            ("ICMP".to_string(), None, Severity::Info)
        }
        other => (format!("IP/{other}"), None, Severity::Info),
    }
}

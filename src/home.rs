//! Detect the machine's public IP address at startup and geolocate it so
//! the "home" marker sits where *this* machine actually is.
//!
//! If detection fails (offline, no DB yet, no public IP endpoint reachable),
//! callers fall back to the configured `map.home.lat` / `map.home.lon`.

use std::net::IpAddr;
use std::time::Duration;

use reqwest;
use tracing::{info, warn};

use std::sync::Arc;

use crate::geo::lookup::GeoLookup;
use crate::ui::map_renderer::HomeLocation;

/// IPv4-only public-IP endpoints.  IP2Location LITE covers IPv4 far better
/// than IPv6, so we try these first to maximize the chance of a successful
/// geolocation.
const IPV4_IP_ENDPOINTS: &[&str] = &[
    "https://ipv4.icanhazip.com",
    "https://api4.ipify.org",
];

/// Dual-stack public-IP endpoints used as a fallback when IPv4-only
/// endpoints are unreachable.
const PUBLIC_IP_ENDPOINTS: &[&str] = &[
    "https://icanhazip.com",
    "https://api.ipify.org",
    "https://ifconfig.me/ip",
];

/// Try to figure out where on the map the local machine belongs.
///
/// 1. Ask a public "what is my IP" endpoint.
/// 2. Look that IP up in the IP2Location database.
/// 3. Return the lat/lon if found.
///
/// This is best-effort: being behind NAT, offline, or lacking a DB all cause
/// a graceful fallback to the configured `fallback` coordinates.  If we
/// managed to discover the public IP but could not geolocate it, the IP is
/// still returned so the UI can display it.
pub async fn detect(geo: &Arc<GeoLookup>, fallback: HomeLocation) -> HomeLocation {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .user_agent("geotop/0.1 home-location detection")
        .build();

    let client = match client {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "failed to build public IP HTTP client");
            return fallback;
        }
    };

    // Try IPv4-only endpoints first, then any-address endpoints.
    let mut ip: Option<IpAddr> = None;
    for url in IPV4_IP_ENDPOINTS.iter().chain(PUBLIC_IP_ENDPOINTS.iter()) {
        match client.get(*url).send().await {
            Ok(resp) if resp.status().is_success() => {
                match resp.text().await {
                    Ok(text) => {
                        let candidate = text.trim();
                        if let Ok(parsed) = candidate.parse::<IpAddr>() {
                            info!(endpoint = url, ip = %parsed, "detected public IP");
                            ip = Some(parsed);
                            break;
                        }
                    }
                    Err(e) => warn!(endpoint = url, error = %e, "failed to read public IP response"),
                }
            }
            Ok(resp) => warn!(endpoint = url, status = %resp.status(), "public IP endpoint returned non-success"),
            Err(e) => warn!(endpoint = url, error = %e, "public IP endpoint unreachable"),
        }
    }

    let Some(ip) = ip else {
        warn!("could not detect public IP; using configured home location");
        return fallback;
    };

    let Some(info) = geo.lookup(ip) else {
        warn!(ip = %ip, "public IP geolocation returned no lat/lon; using configured coordinates");
        return HomeLocation {
            ip: Some(ip),
            ..fallback
        };
    };

    let (lat, lon) = match (info.latitude, info.longitude) {
        (Some(lat), Some(lon)) => (lat, lon),
        _ => {
            warn!(ip = %ip, "public IP geolocation returned incomplete lat/lon; using configured coordinates");
            return HomeLocation {
                ip: Some(ip),
                label: info.country_name.clone().or(info.city.clone()).map(|s| s),
                ..fallback
            };
        }
    };

    info!(
        ip = %ip,
        country = info.country_name.as_deref().unwrap_or("?"),
        city = info.city.as_deref().unwrap_or("?"),
        lat, lon,
        "located home marker from public IP"
    );

    let label = match (&info.country_name, &info.city) {
        (Some(c), Some(ci)) if !c.is_empty() && !ci.is_empty() => format!("{c}, {ci}"),
        (Some(c), _) if !c.is_empty() => c.clone(),
        (_, Some(ci)) if !ci.is_empty() => ci.clone(),
        _ => "Unknown".to_string(),
    };

    HomeLocation {
        lat,
        lon,
        ip: Some(ip),
        label: Some(label),
    }
}

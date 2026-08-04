//! IP geolocation wrapper around the IP2Location + IP2Proxy binaries.
//!
//! Both databases are loaded via `DB::from_file` (which mmaps the file
//! inside the crate and auto-detects whether it's a Location or Proxy
//! bin) — see `ip2location 0.6`. Lookups are O(log N) and incur no
//! syscalls per call.
//!
//! Results are cached in an LRU keyed on `IpAddr`. The renderer and
//! the metrics panel both call `lookup()` once per ingest event; the
//! LRU keeps the hot path off the OS.

use std::net::IpAddr;
use std::num::NonZeroUsize;
use std::sync::Arc;

use ip2location::Record;
use lru::LruCache;
use parking_lot::Mutex;
use tracing::debug;

use crate::db_downloader::DatabaseManager;

/// Lightweight geolocation record used by the renderer. We don't pass
/// `Record<'_>` itself across thread boundaries (it borrows from the DB)
/// so we copy out only the fields the UI consumes.
#[derive(Debug, Clone)]
pub struct GeoInfo {
    #[allow(dead_code)]
    pub ip: IpAddr,
    pub country_code: Option<String>,
    pub country_name: Option<String>,
    #[allow(dead_code)]
    pub region: Option<String>,
    pub city: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    /// Proxy / VPN / TOR / datacenter flags (aggregated).
    pub proxy_kind: Option<ProxyKind>,
    #[allow(dead_code)]
    pub isp: Option<String>,
}

/// Proxy detection categories returned by IP2PROXY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyKind {
    #[allow(dead_code)]
    Anonymous,
    AnonymousVpn,
    Hosting,
    PublicProxy,
    ResidentialProxy,
    Tor,
    Vpn,
    SearchEngineBot,
}

impl ProxyKind {
    pub fn label(self) -> &'static str {
        match self {
            ProxyKind::Anonymous => "ANON",
            ProxyKind::AnonymousVpn => "AVPN",
            ProxyKind::Hosting => "DC ",
            ProxyKind::PublicProxy => "PROX",
            ProxyKind::ResidentialProxy => "RESI",
            ProxyKind::Tor => "TOR ",
            ProxyKind::Vpn => "VPN ",
            ProxyKind::SearchEngineBot => "BOT ",
        }
    }
}

/// Combined LRU cache + handle to the [`DatabaseManager`]. The DB access
/// is lock-free (`arc_swap::load_full`).
pub struct GeoLookup {
    mgr: Arc<DatabaseManager>,
    cache: Mutex<LruCache<IpAddr, Option<GeoInfo>>>,
}

impl GeoLookup {
    pub fn new(mgr: Arc<DatabaseManager>) -> Arc<Self> {
        let cap = NonZeroUsize::new(20_000).expect("20_000 > 0");
        Arc::new(Self {
            mgr,
            cache: Mutex::new(LruCache::new(cap)),
        })
    }

    /// Lookup geolocation + proxy info for an IP. Result is cached.
    pub fn lookup(self: &Arc<Self>, ip: IpAddr) -> Option<GeoInfo> {
        // Fast path: cache hit.
        {
            let mut cache = self.cache.lock();
            if let Some(hit) = cache.get(&ip).cloned() {
                return hit;
            }
        }

        let info = self.lookup_uncached(ip);
        let mut cache = self.cache.lock();
        cache.put(ip, info.clone());
        info
    }

    fn lookup_uncached(&self, ip: IpAddr) -> Option<GeoInfo> {
        // --- IP2Location record -----------------------------------------
        let geo_db = self.mgr.geo()?;
        let record = match geo_db.ip_lookup(ip) {
            Ok(r) => r,
            Err(e) => {
                debug!("IP2Location lookup failed for {ip}: {e}");
                return None;
            }
        };

        // `ip2location 0.6` returns a `Record<'a>` enum. For an IP2Location
        // DB the only populated variant is `Record::LocationDb`.
        let loc = match record {
            Record::LocationDb(rec) => rec,
            _ => return None,
        };

        let lat = loc.latitude.map(|f| f as f64);
        let lon = loc.longitude.map(|f| f as f64);
        if lat.is_none() || lon.is_none() {
            return None;
        }

        // --- IP2Proxy record --------------------------------------------
        let mut proxy_kind: Option<ProxyKind> = None;
        if let Some(proxy_db) = self.mgr.proxy() {
            if let Ok(rec) = proxy_db.ip_lookup(ip) {
                if let Record::ProxyDb(p) = rec {
                    proxy_kind = Some(classify_proxy(p.proxy_type.as_deref()));
                }
            }
        }

        // ip2location 0.6 stores string fields as `Cow<'_, str>` — collapse
        // them into owned `String`s so GeoInfo outlives the lookup.
        let own = |c: Option<std::borrow::Cow<'_, str>>| c.map(|s| s.into_owned());

        Some(GeoInfo {
            ip,
            country_code: loc
                .country
                .as_ref()
                .and_then(|c| own(Some(c.short_name.clone()))),
            country_name: loc
                .country
                .as_ref()
                .and_then(|c| own(Some(c.long_name.clone()))),
            region: own(loc.region),
            city: own(loc.city),
            latitude: lat,
            longitude: lon,
            proxy_kind,
            isp: own(loc.isp),
        })
    }
}

fn classify_proxy(pt: Option<&str>) -> ProxyKind {
    let lc = match pt {
        Some(s) => s.to_ascii_lowercase(),
        None => return ProxyKind::PublicProxy,
    };
    if lc.contains("tor") {
        ProxyKind::Tor
    } else if lc.contains("vpn") && lc.contains("anon") {
        ProxyKind::AnonymousVpn
    } else if lc.contains("vpn") {
        ProxyKind::Vpn
    } else if lc.contains("host") || lc.contains("datacenter") || lc.contains("data center") {
        ProxyKind::Hosting
    } else if lc.contains("residential") {
        ProxyKind::ResidentialProxy
    } else if lc.contains("search") || lc.contains("bot") {
        ProxyKind::SearchEngineBot
    } else {
        ProxyKind::PublicProxy
    }
}

// Resolve default DB filenames on the `db_downloader` side; re-exported
// here for callers that still import them from `geo::lookup`.
#[allow(unused_imports)]
pub use crate::db_downloader::{LOCATION_DB_NAME, PROXY_DB_NAME};

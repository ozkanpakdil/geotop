//! IP2Location / IP2Proxy database downloader + hot-reload manager.
//!
//! Modelled directly on the `DatabaseManager` from
//! [`GeoSentinel-Ingress`](../GeoSentinel-Ingress/src/db_manager.rs):
//!
//! - `reqwest` streaming download with the IP2Location `token=` query
//!   parameter (the same auth mechanism GeoSentinel uses; `token`
//!   comes from `--download-token` / `GEOTOP_DOWNLOAD_TOKEN`).
//! - `.BIN` extraction done in pure Rust via the `zip` crate
//!   (no `unzip` shell-out like a typical CLI downloader).
//! - `arc_swap::ArcSwapOption<DB>` for lock-free hot-reload: the lookup
//!   closure stays on the old DB until the new one is loaded, then
//!   swaps atomically.
//! - max-age refresh policy (default 30 days) + on-disk mtime watch
//!   so `geotop --reload` / external tooling updating the `.BIN` is
//!   picked up without restart.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use arc_swap::ArcSwapOption;
use futures_util::StreamExt;
use ip2location::DB;
use tokio::io::AsyncWriteExt;
use tracing::{debug, info, warn};

/// LITE-DB URLs (the same default GeoSentinel-Ingress uses).
pub const DEFAULT_GEO_URL: &str = "https://www.ip2location.com/download?file=DB11LITEBIN";
pub const DEFAULT_PROXY_URL: &str = "https://www.ip2location.com/download?file=PX11LITEBIN";

/// DB filenames resolved under `data_dir`.
pub const LOCATION_DB_NAME: &str = "IP2LOCATION-LITE-DB11.BIN";
pub const PROXY_DB_NAME: &str = "IP2PROXY-LITE-PX11.BIN";

/// Configuration for the manager. All fields have sensible defaults.
#[derive(Debug, Clone)]
pub struct DbConfig {
    pub data_dir: PathBuf,
    pub geo_db: String,
    pub proxy_db: String,
    pub max_age_days: u64,
    pub geo_url: String,
    pub proxy_url: String,
    pub token: String,
    pub proxy_enabled: bool,
}

impl DbConfig {
    pub fn geo_path(&self) -> PathBuf {
        self.data_dir.join(&self.geo_db)
    }

    pub fn proxy_path(&self) -> PathBuf {
        self.data_dir.join(&self.proxy_db)
    }
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("./data"),
            geo_db: LOCATION_DB_NAME.into(),
            proxy_db: PROXY_DB_NAME.into(),
            max_age_days: 30,
            geo_url: DEFAULT_GEO_URL.into(),
            proxy_url: DEFAULT_PROXY_URL.into(),
            token: String::new(),
            proxy_enabled: true,
        }
    }
}

/// Threadsafe handle to the downloaded databases — cloned cheaply, hot
/// reload swaps the inner `Arc<DB>` atomically.
pub struct DatabaseManager {
    cfg: Arc<DbConfig>,
    geo: ArcSwapOption<DB>,
    proxy: ArcSwapOption<DB>,
    geo_mtime: AtomicU64,
    proxy_mtime: AtomicU64,
}

impl Clone for DatabaseManager {
    fn clone(&self) -> Self {
        Self {
            cfg: Arc::clone(&self.cfg),
            geo: ArcSwapOption::new(self.geo.load_full()),
            proxy: ArcSwapOption::new(self.proxy.load_full()),
            geo_mtime: AtomicU64::new(self.geo_mtime.load(Ordering::Relaxed)),
            proxy_mtime: AtomicU64::new(self.proxy_mtime.load(Ordering::Relaxed)),
        }
    }
}

impl DatabaseManager {
    pub fn new(cfg: DbConfig) -> Arc<Self> {
        Arc::new(Self {
            cfg: Arc::new(cfg),
            geo: ArcSwapOption::new(None),
            proxy: ArcSwapOption::new(None),
            geo_mtime: AtomicU64::new(0),
            proxy_mtime: AtomicU64::new(0),
        })
    }

    /// Borrow the current location DB (lock-free).
    pub fn geo(&self) -> Option<Arc<DB>> {
        self.geo.load_full()
    }

    /// Borrow the current proxy DB (lock-free).
    pub fn proxy(&self) -> Option<Arc<DB>> {
        self.proxy.load_full()
    }

    /// Ensure both DBs exist on disk and are loaded into memory. Downloads
    /// anything that's missing or older than `cfg.max_age_days`.
    pub async fn ensure_databases(self: &Arc<Self>) -> Result<()> {
        tokio::fs::create_dir_all(&self.cfg.data_dir).await?;

        let geo_path = self.cfg.geo_path();
        if self.needs_refresh(&geo_path).await? {
            self.download_and_extract(
                &self.cfg.geo_url,
                &geo_path,
                &self.cfg.geo_db,
            )
            .await?;
        }

        let proxy_path = self.cfg.proxy_path();
        if self.cfg.proxy_enabled && self.needs_refresh(&proxy_path).await? {
            self.download_and_extract(
                &self.cfg.proxy_url,
                &proxy_path,
                &self.cfg.proxy_db,
            )
            .await?;
        }

        self.load_geo(&geo_path).await?;
        if self.cfg.proxy_enabled {
            // Failing to load proxy is non-fatal — GeoSentinel does the same.
            if let Err(e) = self.load_proxy(&proxy_path).await {
                warn!(error = %e, "proxy DB load failed; continuing without proxy detection");
            }
        }

        Ok(())
    }

    /// Re-scan disk and atomically swap the in-memory `DB` if the file
    /// has changed on disk since the last load.
    pub async fn hot_reload(self: &Arc<Self>) -> Result<()> {
        let geo_path = self.cfg.geo_path();
        if self
            .file_changed(&geo_path, &self.geo_mtime)
            .await?
        {
            info!(path = %geo_path.display(), "geo database changed; reloading");
            self.load_geo(&geo_path).await?;
        }

        let proxy_path = self.cfg.proxy_path();
        if self.cfg.proxy_enabled
            && self
                .file_changed(&proxy_path, &self.proxy_mtime)
                .await?
        {
            info!(path = %proxy_path.display(), "proxy database changed; reloading");
            self.load_proxy(&proxy_path).await?;
        }

        Ok(())
    }

    /// Read-only helper exposed to tests / callers (matches GeoSentinel).
    pub async fn needs_refresh(&self, path: &Path) -> Result<bool> {
        let meta = match tokio::fs::metadata(path).await {
            Ok(m) => m,
            Err(_) => return Ok(true),
        };

        let modified = meta.modified().unwrap_or(UNIX_EPOCH);
        let age = SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default();
        let threshold = Duration::from_secs(self.cfg.max_age_days * 24 * 60 * 60);
        Ok(age > threshold)
    }

    async fn file_changed(&self, path: &Path, last: &AtomicU64) -> Result<bool> {
        if !path.exists() {
            return Ok(false);
        }
        let meta = tokio::fs::metadata(path).await?;
        let mtime = meta
            .modified()
            .unwrap_or(UNIX_EPOCH)
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let prev = last.load(Ordering::Relaxed);
        if mtime != prev && prev != 0 {
            last.store(mtime, Ordering::Relaxed);
            Ok(true)
        } else {
            if prev == 0 {
                last.store(mtime, Ordering::Relaxed);
            }
            Ok(false)
        }
    }

    async fn load_geo(self: &Arc<Self>, path: &Path) -> Result<()> {
        let path = path.to_path_buf();
        let path_for_task = path.clone();
        let db = tokio::task::spawn_blocking(move || DB::from_file(&path_for_task))
            .await
            .with_context(|| "spawn_blocking load_geo")?
            .with_context(|| format!("opening IP2Location DB {}", path.display()))?;
        self.geo.store(Some(Arc::new(db)));
        info!(path = %path.display(), "loaded geo database");
        self.update_mtime(path, &self.geo_mtime).await?;
        Ok(())
    }

    async fn load_proxy(self: &Arc<Self>, path: &Path) -> Result<()> {
        let path = path.to_path_buf();
        let path_for_task = path.clone();
        let db = tokio::task::spawn_blocking(move || DB::from_file(&path_for_task))
            .await
            .with_context(|| "spawn_blocking load_proxy")?
            .with_context(|| format!("opening IP2Proxy DB {}", path.display()))?;
        self.proxy.store(Some(Arc::new(db)));
        info!(path = %path.display(), "loaded proxy database");
        self.update_mtime(path, &self.proxy_mtime).await?;
        Ok(())
    }

    async fn update_mtime(&self, path: PathBuf, atom: &AtomicU64) -> Result<()> {
        let meta = tokio::fs::metadata(&path).await?;
        let mtime = meta
            .modified()
            .unwrap_or(UNIX_EPOCH)
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        atom.store(mtime, Ordering::Relaxed);
        Ok(())
    }

    /// Download the zip at `base_url` (appending `&token=<token>` when one
    /// is configured, exactly like GeoSentinel does) and extract the first
    /// `.BIN` entry as `target`.
    async fn download_and_extract(
        self: &Arc<Self>,
        base_url: &str,
        target: &Path,
        expected_name: &str,
    ) -> Result<()> {
        let url = if self.cfg.token.is_empty() {
            base_url.to_string()
        } else {
            format!("{}&token={}", base_url, self.cfg.token)
        };

        info!(url = %url, file = %expected_name, "downloading IP database");
        let zip_path = self.cfg.data_dir.join(format!("{expected_name}.zip"));

        self.download_file(&url, &zip_path).await?;
        self.extract_bin(&zip_path, target).await?;

        // Verify the .BIN opens — same self-check GeoSentinel performs.
        let target_clone = target.to_path_buf();
        tokio::task::spawn_blocking(move || DB::from_file(&target_clone))
            .await?
            .map(|db| db.print_db_info())
            .map_err(|e| anyhow!("verification failed for {}: {e}", target.display()))?;

        Ok(())
    }

    async fn download_file(&self, url: &str, dest: &Path) -> Result<()> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .user_agent("geotop/0.1")
            .build()
            .context("building reqwest client")?;

        let response = client
            .get(url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        if !response.status().is_success() {
            return Err(anyhow!("download failed: HTTP {}", response.status()));
        }

        let mut file = tokio::fs::File::create(dest)
            .await
            .with_context(|| format!("creating {}", dest.display()))?;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        debug!(bytes = %file.metadata().await?.len(), "download complete");
        Ok(())
    }

    async fn extract_bin(&self, zip_path: &Path, target: &Path) -> Result<()> {
        let zip_path = zip_path.to_path_buf();
        let target = target.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let file = std::fs::File::open(&zip_path)
                .with_context(|| format!("opening {}", zip_path.display()))?;
            let mut archive = zip::ZipArchive::new(file)
                .with_context(|| format!("reading zip {}", zip_path.display()))?;
            for i in 0..archive.len() {
                let mut entry = archive.by_index(i)?;
                let name = entry.name().to_string();
                if name.ends_with(".BIN") {
                    let mut out = std::fs::File::create(&target)
                        .with_context(|| format!("creating {}", target.display()))?;
                    let mut buf = vec![0u8; 65_536];
                    loop {
                        let n = entry.read(&mut buf)?;
                        if n == 0 {
                            break;
                        }
                        out.write_all(&buf[..n])?;
                    }
                    out.flush()?;
                    info!(file = %name, "extracted database");
                    return Ok::<(), anyhow::Error>(());
                }
            }
            Err(anyhow!("no .BIN file found in archive"))
        })
        .await?
    }
}

/// Convenience helper for the `update-dbs` subcommand.
pub async fn ensure_dbs(dir: &Path) -> Result<ResolvedDbs> {
    let mut cfg = DbConfig::default();
    cfg.data_dir = dir.to_path_buf();
    let mgr = DatabaseManager::new(cfg);
    mgr.ensure_databases().await?;
    Ok(ResolvedDbs {
        location_db: mgr.cfg.geo_path(),
        proxy_db: if mgr.cfg.proxy_enabled {
            Some(mgr.cfg.proxy_path())
        } else {
            None
        },
    })
}

/// Result of the auto-resolution step.
#[allow(dead_code)]
pub struct ResolvedDbs {
    pub location_db: PathBuf,
    pub proxy_db: Option<PathBuf>,
}

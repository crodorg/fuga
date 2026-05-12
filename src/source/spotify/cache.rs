//! Disk + in-memory cache for `SpotifySource::browse` results.
//!
//! Browse views (Saved Albums, Liked Songs, artist pages, etc.) are
//! near-static catalog data. Hitting the Web API on every tab switch was the
//! main first-open slowness — pagination walks for hundreds of albums block
//! the UI thread until the stream completes.
//!
//! Strategy: serialize each browse result to `<dir>/<sha256(path)>.json`,
//! mirror the most-recent entries in a small in-memory LRU. On `get`, return
//! `Fresh` if the entry is < 5 minutes old, `Stale` if older (caller decides
//! to refetch synchronously and replace), `Miss` if the path was never
//! cached.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use lru::LruCache;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::types::Entry;

const FRESH_TTL: Duration = Duration::from_secs(300);

#[derive(Serialize, Deserialize, Clone)]
struct CachedEntry {
    fetched_at: SystemTime,
    entries: Vec<Entry>,
}

pub enum CacheHit {
    Fresh(Vec<Entry>),
    Stale(Vec<Entry>),
    Miss,
}

pub struct BrowseCache {
    dir: PathBuf,
    mem: Mutex<LruCache<String, CachedEntry>>,
}

impl BrowseCache {
    pub fn new(dir: PathBuf, capacity: usize) -> Self {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!("spotify cache dir create failed at {}: {e}", dir.display());
        }
        let cap = std::num::NonZeroUsize::new(capacity).unwrap_or(
            std::num::NonZeroUsize::new(64).unwrap(),
        );
        Self {
            dir,
            mem: Mutex::new(LruCache::new(cap)),
        }
    }

    fn path_for(&self, key: &str) -> PathBuf {
        let mut h = Sha256::new();
        h.update(key.as_bytes());
        let hex = hex_lower(&h.finalize());
        self.dir.join(format!("{hex}.json"))
    }

    pub async fn get(&self, key: &str) -> CacheHit {
        // Memory hit first (zero I/O).
        if let Some(entry) = self.mem.lock().ok().and_then(|mut m| m.get(key).cloned()) {
            return classify(entry);
        }
        // Disk fallback.
        let path = self.path_for(key);
        match tokio::fs::read(&path).await {
            Ok(bytes) => match serde_json::from_slice::<CachedEntry>(&bytes) {
                Ok(entry) => {
                    if let Ok(mut m) = self.mem.lock() {
                        m.put(key.to_string(), entry.clone());
                    }
                    classify(entry)
                }
                Err(e) => {
                    tracing::debug!("spotify cache decode {key}: {e}");
                    CacheHit::Miss
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => CacheHit::Miss,
            Err(e) => {
                tracing::debug!("spotify cache read {key}: {e}");
                CacheHit::Miss
            }
        }
    }

    pub async fn put(&self, key: &str, entries: Vec<Entry>) -> Result<()> {
        let entry = CachedEntry {
            fetched_at: SystemTime::now(),
            entries,
        };
        if let Ok(mut m) = self.mem.lock() {
            m.put(key.to_string(), entry.clone());
        }
        let path = self.path_for(key);
        let bytes = serde_json::to_vec(&entry)?;
        if let Err(e) = tokio::fs::write(&path, bytes).await {
            tracing::warn!("spotify cache write {}: {e}", path.display());
        }
        Ok(())
    }
}

fn classify(entry: CachedEntry) -> CacheHit {
    let age = SystemTime::now()
        .duration_since(entry.fetched_at)
        .unwrap_or(Duration::from_secs(0));
    if age < FRESH_TTL {
        CacheHit::Fresh(entry.entries)
    } else {
        CacheHit::Stale(entry.entries)
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Paths whose results are NOT safe to cache: anything tied to current
/// playback state or session-bound endpoints. These are rare in browse() but
/// the gate is here so we can't accidentally cache one if it gets added.
pub fn is_cacheable(path: &str) -> bool {
    !path.is_empty()
        && !path.contains("current_playback")
        && !path.contains("devices")
        && path.starts_with("spotify:")
        // Artist sub-views (top tracks / albums) come from mercury and are
        // small + fast. Skipping cache here means rebuilds + sort changes
        // take effect immediately instead of waiting for the 5 min TTL.
        && !path.starts_with("spotify:artistview:")
}

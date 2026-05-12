use std::fmt::Write as _;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use image::DynamicImage;
use lru::LruCache;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

pub struct ArtCache {
    decoded: Mutex<LruCache<String, Arc<DynamicImage>>>,
    disk_dir: PathBuf,
    fetch_sem: Arc<Semaphore>,
}

impl ArtCache {
    pub fn new(disk_dir: PathBuf, max_concurrent: usize, decoded_capacity: usize) -> Self {
        let cap = NonZeroUsize::new(decoded_capacity.max(1)).unwrap();
        Self {
            decoded: Mutex::new(LruCache::new(cap)),
            disk_dir,
            fetch_sem: Arc::new(Semaphore::new(max_concurrent.max(1))),
        }
    }

    pub fn peek(&self, uri: &str) -> Option<Arc<DynamicImage>> {
        let mut g = self.decoded.lock().unwrap();
        g.get(uri).cloned()
    }

    pub async fn get<F, Fut>(&self, uri: &str, fetcher: F) -> Result<Arc<DynamicImage>>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Vec<u8>>>,
    {
        if let Some(img) = self.peek(uri) {
            return Ok(img);
        }

        let key = sha256_hex(uri);
        let path = self.disk_dir.join(format!("{key}.bin"));

        if path.exists() {
            match tokio::fs::read(&path).await {
                Ok(bytes) => match decode_off_runtime(bytes).await {
                    Ok(img) => {
                        let arc = Arc::new(img);
                        self.put(uri, arc.clone());
                        return Ok(arc);
                    }
                    Err(e) => {
                        tracing::warn!(?path, "art disk decode failed: {e}; refetching");
                        let _ = tokio::fs::remove_file(&path).await;
                    }
                },
                Err(e) => tracing::warn!(?path, "art disk read failed: {e}; refetching"),
            }
        }

        let _permit = self.fetch_sem.acquire().await.context("art semaphore")?;
        let bytes = fetcher().await.context("art fetch")?;

        tokio::fs::create_dir_all(&self.disk_dir).await.ok();
        if let Err(e) = tokio::fs::write(&path, &bytes).await {
            tracing::warn!(?path, "art disk write failed: {e}");
        }

        let img = decode_off_runtime(bytes).await.context("art decode")?;
        let arc = Arc::new(img);
        self.put(uri, arc.clone());
        Ok(arc)
    }

    fn put(&self, uri: &str, img: Arc<DynamicImage>) {
        let mut g = self.decoded.lock().unwrap();
        g.put(uri.to_string(), img);
    }
}

/// Decode image bytes off the tokio runtime so a slow PNG decode can't
/// stall the event loop. Many cached images decode in <10ms but a 4k JPEG
/// can take 100+ms, which would freeze the UI for that frame if run
/// inline on the runtime.
async fn decode_off_runtime(bytes: Vec<u8>) -> Result<DynamicImage> {
    tokio::task::spawn_blocking(move || image::load_from_memory(&bytes))
        .await
        .context("decode join")?
        .context("decoding image bytes")
}

fn sha256_hex(s: &str) -> String {
    let h = Sha256::digest(s.as_bytes());
    let mut out = String::with_capacity(64);
    for b in h.iter() {
        write!(out, "{b:02x}").unwrap();
    }
    out
}

pub fn art_dir(cache_root: &Path) -> PathBuf {
    cache_root.join("art")
}

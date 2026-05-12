#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};

use crate::queue::{Queue, QueuedItem, RepeatMode};
use crate::source::MusicSource;

pub struct Dispatcher {
    sources: HashMap<&'static str, Arc<dyn MusicSource>>,
    active_scheme: Option<&'static str>,
}

impl Dispatcher {
    pub fn new() -> Self {
        Self {
            sources: HashMap::new(),
            active_scheme: None,
        }
    }

    pub fn register(&mut self, source: Arc<dyn MusicSource>) {
        self.sources.insert(source.scheme(), source);
    }

    pub fn get(&self, scheme: &str) -> Option<&Arc<dyn MusicSource>> {
        self.sources.get(scheme)
    }

    pub fn schemes(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.sources.keys().copied()
    }

    pub fn active_scheme(&self) -> Option<&'static str> {
        self.active_scheme
    }

    /// Play the given queued item. Stops any prior source if scheme changed.
    /// Stop must complete (or time-out internally) before next.play to avoid
    /// double audio when sources share an output device.
    pub async fn play(&mut self, item: &QueuedItem, volume: u8) -> Result<()> {
        let next = self
            .sources
            .get(item.source_scheme)
            .ok_or_else(|| anyhow!("unknown source scheme: {}", item.source_scheme))?
            .clone();

        if let Some(prev_scheme) = self.active_scheme {
            if prev_scheme != item.source_scheme {
                if let Some(prev) = self.sources.get(prev_scheme) {
                    prev.stop().await.context("stop prev source")?;
                }
            }
        }

        let playable = next.resolve(&item.uri).await?;
        next.play(&playable).await?;
        // Cross-source volume consistency: re-assert the master volume on each play.
        let _ = next.set_volume(volume).await;
        self.active_scheme = Some(item.source_scheme);
        Ok(())
    }

    pub async fn pause(&self) -> Result<()> {
        if let Some(s) = self.active_scheme.and_then(|s| self.sources.get(s)) {
            s.pause().await?;
        }
        Ok(())
    }

    pub async fn resume(&self) -> Result<()> {
        if let Some(s) = self.active_scheme.and_then(|s| self.sources.get(s)) {
            s.resume().await?;
        }
        Ok(())
    }

    pub async fn stop(&mut self) -> Result<()> {
        if let Some(s) = self.active_scheme.and_then(|s| self.sources.get(s)) {
            s.stop().await?;
        }
        self.active_scheme = None;
        Ok(())
    }

    pub async fn advance(&mut self, queue: &mut Queue, volume: u8) -> Result<()> {
        if let Some(item) = queue.advance().cloned() {
            self.play(&item, volume).await?;
        }
        Ok(())
    }

    /// Advance honoring shuffle + repeat settings.
    pub async fn advance_with(
        &mut self,
        queue: &mut Queue,
        shuffle: bool,
        repeat: RepeatMode,
        volume: u8,
    ) -> Result<()> {
        if let Some(item) = queue.advance_with(shuffle, repeat).cloned() {
            self.play(&item, volume).await?;
        }
        Ok(())
    }

    pub async fn previous(&mut self, queue: &mut Queue, volume: u8) -> Result<()> {
        if let Some(item) = queue.back().cloned() {
            self.play(&item, volume).await?;
        }
        Ok(())
    }
}

impl Default for Dispatcher {
    fn default() -> Self {
        Self::new()
    }
}

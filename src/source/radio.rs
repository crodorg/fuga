#![allow(dead_code)]

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use mpd_client::{client::Client, commands};
use reqwest::Url;
use serde::Deserialize;

use crate::source::mpd_shared::{mpd_set_volume, mpd_status};
use crate::source::MusicSource;
use crate::types::{ArtSize, Entry, EntryKind, Item, ItemDisplay, Playable, PlaybackStatus};

#[derive(Debug, Clone, Deserialize)]
pub struct RadioStation {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub art_url: Option<String>,
}

pub struct RadioSource {
    stations: Vec<RadioStation>,
    mpd: Client,
    http: reqwest::Client,
}

impl RadioSource {
    pub fn new(stations: Vec<RadioStation>, mpd: Client, http: reqwest::Client) -> Self {
        Self {
            stations,
            mpd,
            http,
        }
    }

    fn station_for_uri(&self, uri: &str) -> Option<&RadioStation> {
        let key = uri.strip_prefix("radio:").unwrap_or(uri);
        self.stations.iter().find(|s| s.name == key)
    }

    async fn resolve_stream_url(&self, station: &RadioStation) -> Result<String> {
        resolve_playlist(&self.http, &station.url).await
    }
}

#[async_trait]
impl MusicSource for RadioSource {
    fn scheme(&self) -> &'static str {
        "radio"
    }

    fn display_name(&self) -> &'static str {
        "Radio"
    }

    async fn search(&self, query: &str) -> Result<Vec<Item>> {
        let q = query.to_ascii_lowercase();
        Ok(self
            .stations
            .iter()
            .filter(|s| s.name.to_ascii_lowercase().contains(&q))
            .map(station_to_item)
            .collect())
    }

    async fn browse(&self, _path: &str) -> Result<Vec<Entry>> {
        Ok(self
            .stations
            .iter()
            .map(|s| Entry {
                uri: format!("radio:{}", s.name),
                label: s.name.clone(),
                kind: EntryKind::Track,
                display: Some(ItemDisplay {
                    title: s.name.clone(),
                    artist: None,
                    album: None,
                    art_uri: s.art_url.clone(),
                    art_uri_full: None,
                    duration: None,
                    sort_hint: None,
                    track_no: None,
                }),
            })
            .collect())
    }

    async fn resolve(&self, uri: &str) -> Result<Playable> {
        let station = self
            .station_for_uri(uri)
            .ok_or_else(|| anyhow!("unknown radio station: {uri}"))?
            .clone();
        let stream = self.resolve_stream_url(&station).await?;
        Ok(Playable::Url(stream))
    }

    async fn play(&self, playable: &Playable) -> Result<()> {
        let url = match playable {
            Playable::Url(u) | Playable::LibraryUri(u) => u.as_str(),
        };
        self.mpd
            .command(commands::ClearQueue)
            .await
            .context("MPD clear")?;
        let _ = self
            .mpd
            .command(commands::Add::uri(url))
            .await
            .context("MPD addid")?;
        self.mpd
            .command(commands::Play::current())
            .await
            .context("MPD play")?;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.mpd.command(commands::Stop).await.context("MPD stop")?;
        Ok(())
    }

    async fn pause(&self) -> Result<()> {
        self.mpd
            .command(commands::SetPause(true))
            .await
            .context("MPD pause")?;
        Ok(())
    }

    async fn resume(&self) -> Result<()> {
        self.mpd
            .command(commands::SetPause(false))
            .await
            .context("MPD resume")?;
        Ok(())
    }

    async fn playback_status(&self) -> Result<Option<PlaybackStatus>> {
        Ok(Some(mpd_status(&self.mpd).await?))
    }

    async fn set_volume(&self, vol: u8) -> Result<()> {
        mpd_set_volume(&self.mpd, vol).await
    }

    async fn art(&self, uri: &str, _size: ArtSize) -> Result<Vec<u8>> {
        let station = self
            .station_for_uri(uri)
            .ok_or_else(|| anyhow!("unknown radio: {uri}"))?;
        let url = station
            .art_url
            .as_deref()
            .ok_or_else(|| anyhow!("no art_url for {}", station.name))?;
        let bytes = self
            .http
            .get(url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()?
            .bytes()
            .await
            .context("read art body")?;
        Ok(bytes.to_vec())
    }
}

fn station_to_item(s: &RadioStation) -> Item {
    Item {
        uri: format!("radio:{}", s.name),
        display: ItemDisplay {
            title: s.name.clone(),
            artist: None,
            album: None,
            art_uri: s.art_url.clone(),
            art_uri_full: None,
            duration: None,
            sort_hint: None,
            track_no: None,
        },
    }
}

/// Inspect URL extension or content; return the actual stream URL.
pub async fn resolve_playlist(http: &reqwest::Client, url: &str) -> Result<String> {
    let parsed = Url::parse(url).with_context(|| format!("parse url {url}"))?;
    let lower = parsed.path().to_ascii_lowercase();

    if lower.ends_with(".pls") || lower.ends_with(".m3u") || lower.ends_with(".m3u8") {
        let body = http
            .get(url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()?
            .text()
            .await
            .context("read playlist body")?;
        if lower.ends_with(".pls") {
            parse_pls(&body).ok_or_else(|| anyhow!("no File entries in .pls"))
        } else {
            parse_m3u(&body).ok_or_else(|| anyhow!("no URL entries in m3u"))
        }
    } else {
        // Direct stream URL.
        Ok(url.to_string())
    }
}

fn parse_pls(text: &str) -> Option<String> {
    // INI-ish: `File1=https://...`, `File2=...`
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("File") {
            if let Some(eq) = rest.find('=') {
                let val = rest[eq + 1..].trim();
                if !val.is_empty() {
                    return Some(val.to_string());
                }
            }
        }
    }
    None
}

fn parse_m3u(text: &str) -> Option<String> {
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        return Some(l.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pls_first_file_entry() {
        let body = "[playlist]\nFile1=https://stream.example/1.mp3\nFile2=https://stream.example/2.mp3\nNumberOfEntries=2\n";
        assert_eq!(
            parse_pls(body).as_deref(),
            Some("https://stream.example/1.mp3")
        );
    }

    #[test]
    fn parses_m3u_skipping_comments() {
        let body = "#EXTM3U\n#EXTINF:-1,Live\nhttps://stream.example/live\n";
        assert_eq!(
            parse_m3u(body).as_deref(),
            Some("https://stream.example/live")
        );
    }
}

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::BytesMut;
use mpd_client::{
    client::{Client, ConnectionEvents},
    commands::{self, SongId},
    filter::{Filter, Operator},
    tag::Tag,
};
use tokio::net::TcpStream;

use crate::source::MusicSource;
use crate::source::mpd_shared::{mpd_set_volume, mpd_status};
use crate::types::{ArtSize, Entry, EntryKind, Item, ItemDisplay, Playable, PlaybackStatus};

pub struct LocalSource {
    client: Client,
    /// Filesystem root for the MPD library, mirroring `music_directory` in
    /// mpd.conf. Used by `art()` to locate sidecar covers when MPD's
    /// `albumart`/`readpicture` come back empty. `None` disables the
    /// fallback.
    music_directory: Option<std::path::PathBuf>,
}

pub struct LocalConnection {
    pub source: LocalSource,
    pub events: ConnectionEvents,
    /// Cheap clone of the underlying MPD client for sharing with other MPD-backed sources.
    pub client: Client,
}

impl LocalSource {
    pub async fn connect(
        host: &str,
        port: u16,
        password: Option<&str>,
        music_directory: Option<std::path::PathBuf>,
    ) -> Result<LocalConnection> {
        let stream = TcpStream::connect((host, port))
            .await
            .with_context(|| format!("connecting to MPD at {host}:{port}"))?;

        let (client, events) = Client::connect_with_password_opt(stream, password)
            .await
            .map_err(|e| anyhow::anyhow!("MPD handshake failed: {e:?}"))?;

        Ok(LocalConnection {
            source: Self {
                client: client.clone(),
                music_directory,
            },
            events,
            client,
        })
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    fn song_to_item(song: &mpd_client::responses::Song) -> Item {
        let title = song
            .title()
            .map(str::to_owned)
            .unwrap_or_else(|| song.url.clone());
        let artist = song.artists().first().cloned();
        let album = song.album().map(str::to_owned);
        // (disc, track) -> u32 for sort. Disc * 1000 + track keeps multi-disc
        // albums grouped: disc1 tracks first, then disc2. 0/0 = unknown.
        let (disc, track) = song.number();
        let track_no = if disc == 0 && track == 0 {
            None
        } else {
            Some((disc.max(1) as u32) * 1000 + track as u32)
        };
        Item {
            uri: song.url.clone(),
            display: ItemDisplay {
                title,
                artist,
                album,
                art_uri: Some(song.url.clone()),
                art_uri_full: None,
                duration: song.duration,
                sort_hint: None,
                track_no,
                year_hint: None,
            },
        }
    }
}

#[async_trait]
impl MusicSource for LocalSource {
    fn scheme(&self) -> &'static str {
        "local"
    }

    fn display_name(&self) -> &'static str {
        "Local Library"
    }

    async fn search(&self, query: &str) -> Result<Vec<Item>> {
        if query.is_empty() {
            return Ok(Vec::new());
        }
        // MPD's `find` operator `contains` is case-sensitive; `search` would
        // be case-insensitive but mpd_client 1.4 doesn't expose it. Fall back
        // to `find` with Operator::Match + a case-insensitive PCRE pattern
        // `(?i)<escaped>`. Filter has no OR, so fan out one query per tag
        // (Title / Artist / Album) and dedupe by URL. Each tag is capped at
        // 200 hits so a pathological pattern can't blow memory.
        let pattern = format!("(?i){}", regex_escape(query));
        let mut seen = std::collections::HashSet::new();
        let mut out: Vec<Item> = Vec::new();
        for tag in [Tag::Title, Tag::Artist, Tag::Album] {
            let tag_label = format!("{tag:?}");
            let filter = Filter::new(tag, Operator::Match, pattern.clone());
            let songs = match self
                .client
                .command(commands::Find::new(filter).window(0..200))
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("MPD find {tag_label}: {e:?}");
                    continue;
                }
            };
            for s in songs.iter() {
                if seen.insert(s.url.clone()) {
                    out.push(Self::song_to_item(s));
                }
            }
        }
        Ok(out)
    }

    async fn browse(&self, path: &str) -> Result<Vec<Entry>> {
        match path {
            "local:dir:" | "local:dir" => return self.lsinfo("").await,
            p if p.starts_with("local:dir:") => {
                let dir = p.trim_start_matches("local:dir:");
                return self.lsinfo(dir).await;
            }
            _ => {}
        }
        match path {
            "local:playlists" => {
                let playlists = self
                    .client
                    .command(commands::GetPlaylists)
                    .await
                    .context("MPD listplaylists")?;
                Ok(playlists
                    .into_iter()
                    .map(|p| Entry {
                        uri: format!("local:playlist:{}", p.name),
                        label: p.name.clone(),
                        kind: EntryKind::Playlist,
                        display: None,
                    })
                    .collect())
            }
            p if p.starts_with("local:album:") => {
                let name = p.trim_start_matches("local:album:");
                let songs = self.songs_in_album(name).await?;
                Ok(songs
                    .into_iter()
                    .map(|item| Entry {
                        uri: item.uri.clone(),
                        label: format!(
                            "{} — {}",
                            item.display.artist.as_deref().unwrap_or(""),
                            item.display.title
                        ),
                        kind: EntryKind::Track,
                        display: Some(item.display),
                    })
                    .collect())
            }
            p if p.starts_with("local:playlist:") => {
                let name = p.trim_start_matches("local:playlist:");
                let songs = self
                    .client
                    .command(commands::GetPlaylist(name))
                    .await
                    .context("MPD listplaylistinfo")?;
                Ok(songs
                    .iter()
                    .map(|s| {
                        let item = Self::song_to_item(s);
                        Entry {
                            uri: item.uri.clone(),
                            label: format!(
                                "{} — {}",
                                item.display.artist.as_deref().unwrap_or(""),
                                item.display.title
                            ),
                            kind: EntryKind::Track,
                            display: Some(item.display),
                        }
                    })
                    .collect())
            }
            _ => {
                // Default browse: album list. We pull all songs in one MPD
                // round-trip and group by album name so each Album entry can
                // carry an `art_uri` pointing at one of its tracks. The art
                // fetcher then routes that song URI through `LocalSource::art`
                // (MPD `albumart` / `readpicture`) so albums show their cover
                // as the row icon. Cap is generous; libraries past 50k songs
                // need a smarter incremental approach.
                let songs = self
                    .client
                    .command(commands::Find::new(Filter::tag_exists(Tag::Title)).window(0..50_000))
                    .await
                    .context("MPD find all (album list)")?;
                use std::collections::BTreeMap;
                let mut by_album: BTreeMap<String, String> = BTreeMap::new();
                for s in songs.iter() {
                    let Some(album) = s.album() else { continue };
                    if album.is_empty() {
                        continue;
                    }
                    by_album
                        .entry(album.to_string())
                        .or_insert_with(|| s.url.clone());
                }
                let mut out = Vec::with_capacity(by_album.len());
                for (album, song_uri) in by_album {
                    let display = ItemDisplay {
                        title: album.clone(),
                        artist: None,
                        album: Some(album.clone()),
                        art_uri: Some(song_uri.clone()),
                        art_uri_full: Some(song_uri),
                        duration: None,
                        sort_hint: None,
                        track_no: None,
                        year_hint: None,
                    };
                    out.push(Entry {
                        uri: format!("local:album:{album}"),
                        label: album,
                        kind: EntryKind::Album,
                        display: Some(display),
                    });
                }
                Ok(out)
            }
        }
    }

    async fn resolve(&self, uri: &str) -> Result<Playable> {
        Ok(Playable::LibraryUri(uri.to_string()))
    }

    async fn play(&self, playable: &Playable) -> Result<()> {
        let uri = match playable {
            Playable::Url(u) | Playable::LibraryUri(u) => u.as_str(),
        };
        self.client
            .command(commands::ClearQueue)
            .await
            .context("MPD clear")?;
        let _id: SongId = self
            .client
            .command(commands::Add::uri(uri))
            .await
            .context("MPD addid")?;
        self.client
            .command(commands::Play::current())
            .await
            .context("MPD play")?;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.client
            .command(commands::Stop)
            .await
            .context("MPD stop")?;
        Ok(())
    }

    async fn pause(&self) -> Result<()> {
        self.client
            .command(commands::SetPause(true))
            .await
            .context("MPD pause")?;
        Ok(())
    }

    async fn resume(&self) -> Result<()> {
        self.client
            .command(commands::SetPause(false))
            .await
            .context("MPD resume")?;
        Ok(())
    }

    async fn playback_status(&self) -> Result<Option<PlaybackStatus>> {
        Ok(Some(mpd_status(&self.client).await?))
    }

    async fn seek(&self, position: std::time::Duration) -> Result<()> {
        self.client
            .command(commands::Seek(commands::SeekMode::Absolute(position)))
            .await
            .context("MPD seek")?;
        Ok(())
    }

    async fn set_volume(&self, vol: u8) -> Result<()> {
        mpd_set_volume(&self.client, vol).await
    }

    async fn art(&self, uri: &str, _size: ArtSize) -> Result<Vec<u8>> {
        let t0 = std::time::Instant::now();
        match self.client.album_art(uri).await {
            Ok(Some((bytes, _mime))) => {
                let bm: BytesMut = bytes;
                tracing::debug!(uri = %uri, ms = t0.elapsed().as_millis() as u64, "local art: mpd hit");
                return Ok(bm.to_vec());
            }
            Ok(None) => {}
            Err(e) => {
                // Don't bail yet — try the sidecar fallback below before
                // giving up. Many MPD libraries store covers as files
                // rather than embed them.
                tracing::debug!("album_art {uri}: {e:?}; trying sidecar");
            }
        }
        let mpd_ms = t0.elapsed().as_millis();
        if let Some(bytes) = self.read_sidecar_cover(uri).await {
            tracing::debug!(
                uri = %uri,
                mpd_ms = mpd_ms as u64,
                total_ms = t0.elapsed().as_millis() as u64,
                "local art: sidecar hit"
            );
            return Ok(bytes);
        }
        tracing::debug!(
            uri = %uri,
            mpd_ms = mpd_ms as u64,
            total_ms = t0.elapsed().as_millis() as u64,
            "local art: miss"
        );
        Err(anyhow::anyhow!("no album art for {uri}"))
    }

    async fn embedded_lyrics(&self, uri: &str) -> Result<Option<String>> {
        use mpd_client::protocol::command::Command as RawCommand;
        let mut cmd = RawCommand::new("readcomments");
        cmd.add_argument(uri)
            .map_err(|e| anyhow::anyhow!("readcomments arg: {e:?}"))?;
        let frame = match self.client.raw_command(cmd).await {
            Ok(f) => f,
            Err(e) => {
                // Not fatal — caller falls back to lrclib.
                tracing::debug!("readcomments {uri}: {e:?}");
                return Ok(None);
            }
        };
        // Lyrics live under varied comment keys; prefer synced. Some taggers
        // split the lyric across one comment per line, so accumulate same-key
        // values rather than taking the first. Keys come back verbatim — match
        // case-insensitively.
        let mut synced: Vec<String> = Vec::new();
        let mut plain: Vec<String> = Vec::new();
        for (k, v) in frame.fields() {
            match k.to_ascii_lowercase().as_str() {
                "syncedlyrics" => synced.push(v.to_string()),
                "lyrics" | "unsyncedlyrics" | "uslt" => plain.push(v.to_string()),
                _ => {}
            }
        }
        let blob = if synced.is_empty() { plain } else { synced }.join("\n");
        Ok((!blob.trim().is_empty()).then_some(blob))
    }
}

impl LocalSource {
    /// Look for `cover.jpg` / `folder.jpg` / `front.jpg` / `AlbumArt.jpg`
    /// (and `.png` variants) next to the given track. Requires
    /// `music_directory` to be configured so we can resolve the song's
    /// library URI to a filesystem path. Returns `None` on any miss; never
    /// errors so the caller falls cleanly through to "no art".
    async fn read_sidecar_cover(&self, uri: &str) -> Option<Vec<u8>> {
        let root = self.music_directory.as_ref()?;
        let song_path = root.join(uri);
        let dir = song_path.parent()?;
        const CANDIDATES: &[&str] = &[
            "cover.jpg",
            "cover.jpeg",
            "cover.png",
            "folder.jpg",
            "folder.jpeg",
            "folder.png",
            "front.jpg",
            "front.png",
            "AlbumArt.jpg",
            "AlbumArtSmall.jpg",
            "albumart.jpg",
        ];
        for name in CANDIDATES {
            let p = dir.join(name);
            if let Ok(bytes) = tokio::fs::read(&p).await {
                tracing::debug!("sidecar art hit: {}", p.display());
                return Some(bytes);
            }
        }
        None
    }
}

/// Leaf name (last `/`-segment). Used as the display label for `lsinfo`
/// entries so directories show as "Some Album" rather than the full
/// "Artist/Some Album" relative path.
fn leaf_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// Escape PCRE regex metacharacters so a user's plain-text query can be
/// embedded inside a case-insensitive regex (`(?i)…`) without surprise.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if matches!(
            c,
            '.' | '*'
                | '+'
                | '?'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '|'
                | '^'
                | '$'
                | '\\'
                | '/'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

impl LocalSource {
    /// `lsinfo <dir>` (raw protocol). Returns directories, files (tracks),
    /// and saved playlists at one level. Directory entries surface as
    /// `local:dir:<path>` URIs so `browse` can descend; file entries use
    /// MPD's library path directly (the same form `Add::uri` accepts).
    /// Track titles/artists/albums attach to the just-pushed file entry as
    /// the field stream walks past them.
    async fn lsinfo(&self, dir: &str) -> Result<Vec<Entry>> {
        use mpd_client::protocol::command::Command as RawCommand;
        let t0 = std::time::Instant::now();
        let mut cmd = RawCommand::new("lsinfo");
        if !dir.is_empty() {
            cmd.add_argument(dir)
                .map_err(|e| anyhow::anyhow!("lsinfo arg: {e:?}"))?;
        }
        let frame = self
            .client
            .raw_command(cmd)
            .await
            .map_err(|e| anyhow::anyhow!("lsinfo {dir:?}: {e:?}"))?;
        let rpc_ms = t0.elapsed().as_millis();
        let mut out: Vec<Entry> = Vec::new();
        let mut cur_is_file = false;
        for (k, v) in frame.fields() {
            match k {
                "directory" => {
                    out.push(Entry {
                        uri: format!("local:dir:{v}"),
                        label: leaf_name(v),
                        kind: EntryKind::Directory,
                        display: None,
                    });
                    cur_is_file = false;
                }
                "file" => {
                    let leaf = leaf_name(v);
                    out.push(Entry {
                        uri: v.to_string(),
                        label: leaf.clone(),
                        kind: EntryKind::Track,
                        display: Some(ItemDisplay {
                            title: leaf,
                            artist: None,
                            album: None,
                            // Use the file path as the art lookup key; MPD's
                            // `albumart`/`readpicture` resolve the cover from
                            // the song URL.
                            art_uri: Some(v.to_string()),
                            art_uri_full: Some(v.to_string()),
                            duration: None,
                            sort_hint: None,
                            track_no: None,
                            year_hint: None,
                        }),
                    });
                    cur_is_file = true;
                }
                "playlist" => {
                    out.push(Entry {
                        uri: format!("local:playlist:{v}"),
                        label: leaf_name(v),
                        kind: EntryKind::Playlist,
                        display: None,
                    });
                    cur_is_file = false;
                }
                "Title" | "Artist" | "Album" | "duration" | "Time" if cur_is_file => {
                    if let Some(last) = out.last_mut() {
                        if let Some(d) = last.display.as_mut() {
                            match k {
                                "Title" => {
                                    d.title = v.to_string();
                                    last.label = format!(
                                        "{} — {}",
                                        d.artist.as_deref().unwrap_or(""),
                                        d.title
                                    );
                                }
                                "Artist" => {
                                    d.artist = Some(v.to_string());
                                    last.label = format!("{} — {}", v, d.title);
                                }
                                "Album" => d.album = Some(v.to_string()),
                                // `duration` (fractional seconds) is preferred;
                                // `Time` (integer seconds) is the fallback when
                                // it's absent. Without this, dir-browsed tracks
                                // carried no duration and lrclib lyrics lookups
                                // (which match on it) silently failed.
                                "duration" => {
                                    if let Ok(secs) = v.parse::<f64>() {
                                        d.duration = Some(std::time::Duration::from_secs_f64(secs));
                                    }
                                }
                                "Time" if d.duration.is_none() => {
                                    if let Ok(secs) = v.parse::<u64>() {
                                        d.duration = Some(std::time::Duration::from_secs(secs));
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        // Sort: directories first, then playlists, then files (tracks). Within
        // each kind, case-insensitive alpha. Mirrors ranger / mc style so the
        // user can descend without scrolling past inline tracks.
        out.sort_by(|a, b| {
            let rank = |k: &EntryKind| match k {
                EntryKind::Directory => 0,
                EntryKind::Playlist => 1,
                _ => 2,
            };
            rank(&a.kind).cmp(&rank(&b.kind)).then_with(|| {
                a.label
                    .to_ascii_lowercase()
                    .cmp(&b.label.to_ascii_lowercase())
            })
        });
        tracing::info!(
            dir = %dir,
            entries = out.len(),
            rpc_ms = rpc_ms as u64,
            total_ms = t0.elapsed().as_millis() as u64,
            "lsinfo done"
        );
        Ok(out)
    }

    pub async fn next(&self) -> Result<()> {
        self.client
            .command(commands::Next)
            .await
            .context("MPD next")?;
        Ok(())
    }

    pub async fn previous(&self) -> Result<()> {
        self.client
            .command(commands::Previous)
            .await
            .context("MPD previous")?;
        Ok(())
    }

    pub async fn current_song(&self) -> Result<Option<Item>> {
        let song = self
            .client
            .command(commands::CurrentSong)
            .await
            .context("MPD currentsong")?;
        Ok(song.map(|s| Self::song_to_item(&s.song)))
    }

    pub async fn add_to_queue(&self, uri: &str) -> Result<()> {
        let _: SongId = self
            .client
            .command(commands::Add::uri(uri))
            .await
            .context("MPD addid")?;
        Ok(())
    }

    /// Pull all songs from the library (Title-exists filter). Capped via window.
    pub async fn all_songs(&self, limit: usize) -> Result<Vec<Item>> {
        let filter = Filter::tag_exists(Tag::Title);
        let songs = self
            .client
            .command(commands::Find::new(filter).window(0..limit))
            .await
            .context("MPD find all")?;
        Ok(songs.iter().map(Self::song_to_item).collect())
    }

    /// Find all songs with `Album = name`.
    pub async fn songs_in_album(&self, name: &str) -> Result<Vec<Item>> {
        let filter = Filter::tag(Tag::Album, name.to_string());
        let songs = self
            .client
            .command(commands::Find::new(filter))
            .await
            .context("MPD find album")?;
        // Dedupe by (artist, title). Some libraries hold the same track under
        // two distinct file paths — e.g. an organized copy in the album folder
        // plus a loose "Artist - Title.mp3" dumped at the music_directory root.
        // Both are separate MPD URIs, so a tag `find` returns each track twice;
        // the album view should still show it once. First occurrence wins.
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for s in songs.iter() {
            let item = Self::song_to_item(s);
            let key = (
                item.display
                    .artist
                    .as_deref()
                    .unwrap_or_default()
                    .to_lowercase(),
                item.display.title.to_lowercase(),
            );
            if seen.insert(key) {
                out.push(item);
            }
        }
        Ok(out)
    }
}

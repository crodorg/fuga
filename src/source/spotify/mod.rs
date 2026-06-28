#![allow(dead_code)]

pub mod auth;
pub mod cache;
pub mod governor;
pub mod metadata;
pub mod player;
pub mod raw;

pub use player::SpotifyEvent;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use futures::{StreamExt, TryStreamExt};

/// Max entries fetched per browse call. Large Spotify libraries (Liked Songs
/// in the thousands) would otherwise lock the UI. The user can re-browse with
/// `?offset=N` to walk the rest; the activate path appends a synthetic
/// `(load more)` entry on each page boundary.
const PAGE_LIMIT: usize = 200;
/// Spotify's `/v1/search` endpoint capped `limit` at 10 in Feb 2026 (down
/// from 50). 11+ returns "Invalid limit". rspotify documents this in its
/// crate-level comment but doesn't enforce it. To match spotatui's ~20
/// results per type we paginate two pages: offset=0 and offset=10.
const SEARCH_LIMIT: u32 = 10;
const SEARCH_PAGES: u32 = 2;
use rspotify::AuthCodePkceSpotify;
use rspotify::clients::{BaseClient, OAuthClient};
use rspotify::model::{AlbumId, ArtistId, PlaylistId, ShowId, TrackId};
use rspotify::prelude::Id;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;

use crate::config::SpotifyConfig;
use crate::source::MusicSource;
use crate::source::spotify::player::SpotifyPlayer;
use crate::types::{
    ArtSize, DeviceEntry, Entry, EntryKind, Item, ItemDisplay, Playable, PlaybackStatus,
};

pub struct SpotifySource {
    api: Arc<Mutex<AuthCodePkceSpotify>>,
    http: reqwest::Client,
    config: SpotifyConfig,
    player: Mutex<Option<SpotifyPlayer>>,
    events_tx: UnboundedSender<SpotifyEvent>,
    /// Disk + LRU cache of `browse()` results. Reduces first-open latency
    /// after the initial paginated walk has been persisted.
    browse_cache: Arc<crate::source::spotify::cache::BrowseCache>,
}

/// Map an rspotify error into anyhow, engaging the rate-limit gate on a 429.
/// The real `Retry-After` isn't reachable through rspotify's error type, so we
/// record a short fallback cooldown; the header-bearing `raw::get_normalized`
/// path (and the view poller) upgrade it to the true value within a cycle.
fn classify_api_err(e: rspotify::ClientError) -> anyhow::Error {
    if governor::is_rate_limit_err(&e) {
        // Do NOT set the gate here. A direct rspotify call can't read the real
        // Retry-After (its error buries a different-version reqwest Response),
        // so a guessed cooldown would "poison" the gate short and make the
        // header-reading `raw::get_normalized` calls fail fast before they ever
        // measure the true window. Only those calls set the gate; here we just
        // surface the known remaining time (or a short hint if none is set).
        let rem = governor::instance()
            .remaining_block()
            .unwrap_or(governor::FALLBACK_COOLDOWN);
        governor::RateLimited(rem).into()
    } else {
        anyhow::Error::new(e)
    }
}

impl SpotifySource {
    pub fn new(
        api: Arc<Mutex<AuthCodePkceSpotify>>,
        http: reqwest::Client,
        config: SpotifyConfig,
        events_tx: UnboundedSender<SpotifyEvent>,
        browse_cache: Arc<crate::source::spotify::cache::BrowseCache>,
        ratelimit_path: std::path::PathBuf,
    ) -> Self {
        // Initialize the process-global Web-API governor with its persistence
        // path so a multi-hour rate-limit cooldown survives a restart.
        governor::configure(ratelimit_path);
        Self {
            api,
            http,
            config,
            player: Mutex::new(None),
            events_tx,
            browse_cache,
        }
    }

    /// Run an rspotify-direct Web API call through the rate-limit governor:
    /// fail fast while a cooldown is active, pace the call start, and on a 429
    /// record a fallback cooldown (rspotify buries the `Retry-After` header in
    /// an error type we can't read — the real value is captured by the
    /// header-bearing `raw::get_normalized` path and the view poller).
    async fn governed<T, F>(&self, fut: F) -> Result<T>
    where
        F: std::future::Future<Output = std::result::Result<T, rspotify::ClientError>>,
    {
        governor::instance()
            .enter()
            .await
            .map_err(anyhow::Error::new)?;
        fut.await.map_err(classify_api_err)
    }

    async fn ensure_player(&self) -> Result<()> {
        {
            let g = self.player.lock().await;
            if let Some(p) = g.as_ref() {
                if !p.session.is_invalid() {
                    return Ok(());
                }
                // librespot's idle keepalive marked the session invalid (it
                // can't be reused). Fall through to drop and rebuild.
                tracing::info!("spotify session invalid; rebuilding player");
            }
        }
        // Drop any dead player before building a fresh one.
        *self.player.lock().await = None;
        // Need a fresh access token from rspotify.
        let access_token = {
            let api = self.api.lock().await;
            // Trigger refresh if expired.
            if let Ok(Some(t)) = api.read_token_cache(true).await {
                if t.is_expired() {
                    if let Err(e) = api.refresh_token().await {
                        tracing::warn!("rspotify refresh: {e}");
                    }
                }
            }
            let tg = api.token.lock().await;
            let tg = tg.map_err(|_| anyhow!("rspotify token mutex poisoned"))?;
            let t = tg
                .as_ref()
                .ok_or_else(|| anyhow!("no Spotify access token; run --spotify-auth"))?;
            t.access_token.clone()
        };
        let player =
            SpotifyPlayer::connect(&access_token, &self.config, self.events_tx.clone()).await?;
        *self.player.lock().await = Some(player);
        Ok(())
    }

    /// Top-level browse entries returned for `path == ""`.
    async fn browse_root(&self) -> Result<Vec<Entry>> {
        let mk = |uri: &str, label: &str| Entry {
            uri: uri.into(),
            label: label.into(),
            kind: EntryKind::Directory,
            display: None,
        };
        // Saved Albums / Playlists / Followed Artists are reachable as
        // top-level tabs in Spotify mode (Albums / Playlists / Artists), so
        // they no longer appear here — keeps the Library landing focused on
        // the Spotify-only views you can't reach elsewhere.
        Ok(vec![
            mk("spotify:view:discover_weekly", "Discover Weekly"),
            // Release Radar omitted: Spotify doesn't expose it via the Web
            // API and doesn't always auto-add it to the rootlist either
            // (mercury fallback fails for accounts that never followed it
            // through the desktop client, even after listening in mobile).
            mk("spotify:view:saved_tracks", "Liked Songs"),
            mk("spotify:view:recently_played", "Recently Played"),
            mk("spotify:view:top_tracks", "Top Tracks"),
            mk("spotify:view:top_artists", "Top Artists"),
        ])
    }

    /// Top tracks via mercury (Web API endpoint deprecated). Requires an
    /// authenticated librespot session; ensure_player connects on first use.
    async fn browse_artist_top(&self, artist_id: &str) -> Result<Vec<Entry>> {
        self.ensure_player().await?;
        let g = self.player.lock().await;
        let p = g
            .as_ref()
            .ok_or_else(|| anyhow!("spotify player not initialised"))?;
        let tracks = metadata::artist_top_tracks(&p.session, artist_id).await?;
        Ok(tracks
            .into_iter()
            .map(|t| Entry {
                uri: t.uri.clone(),
                label: format!("{} — {}", t.artist_name.as_deref().unwrap_or(""), t.name),
                kind: EntryKind::Track,
                display: Some(ItemDisplay {
                    title: t.name,
                    artist: t.artist_name,
                    album: t.album_name,
                    art_uri: t.art_url,
                    art_uri_full: t.art_url_full,
                    duration: Some(std::time::Duration::from_millis(t.duration_ms as u64)),
                    sort_hint: None,
                    track_no: None,
                    year_hint: None,
                }),
            })
            .collect())
    }

    /// Artist albums via mercury. Spotify's Web API endpoint
    /// `artists/{id}/albums` returns 400 ("Invalid limit", regardless of
    /// limit value) for non-allowlisted dev apps. The desktop client
    /// fetches via the artist metadata mercury proto instead, which still
    /// works.
    async fn browse_artist_albums(&self, artist_id: &str) -> Result<Vec<Entry>> {
        self.ensure_player().await?;
        let g = self.player.lock().await;
        let p = g
            .as_ref()
            .ok_or_else(|| anyhow!("spotify player not initialised"))?;
        let albums = metadata::artist_albums(&p.session, artist_id).await?;
        Ok(albums
            .into_iter()
            .map(|a| {
                let year_suffix = a.year.map(|y| format!(" ({y})")).unwrap_or_default();
                Entry {
                    uri: a.uri.clone(),
                    label: format!("{}{}", a.name, year_suffix),
                    kind: EntryKind::Album,
                    display: Some(ItemDisplay {
                        title: a.name,
                        artist: a.artist_name,
                        album: None,
                        art_uri: a.art_url,
                        art_uri_full: a.art_url_full,
                        duration: None,
                        sort_hint: None,
                        track_no: None,
                        year_hint: None,
                    }),
                }
            })
            .collect())
    }

    /// Playlist tracks via mercury, paginated 200 per page, with a
    /// permanent per-page cache keyed by mercury revision.
    ///
    /// Original working flow restored: cheap mercury call fetches the
    /// FULL URI list + sort hints, sort newest-first, hydrate only the
    /// visible window [offset..offset+PAGE_LIMIT], append a `(load more)`
    /// sentinel if more remain. The user pays one page worth of hydration
    /// per click; no background task competes with the foreground browse.
    ///
    /// Per-page cache key includes the mercury revision so a playlist
    /// edit (add/remove/reorder) invalidates every cached page for that
    /// playlist on the next open. Within the same revision, opening a
    /// previously-loaded page is instant; only new pages pay hydration.
    async fn browse_playlist_via_mercury(
        &self,
        playlist_id: &str,
        offset: usize,
        base_path: &str,
    ) -> Result<Vec<Entry>> {
        tracing::info!(playlist_id, offset, "mercury: enter");
        self.ensure_player().await?;
        tracing::info!(playlist_id, "mercury: player ready");
        // Grab session + URI list, then drop the player lock immediately.
        let (session, sorted_uris, revision) = {
            let g = self.player.lock().await;
            let p = g
                .as_ref()
                .ok_or_else(|| anyhow!("spotify player not initialised"))?;
            let session = p.session.clone();
            drop(g);
            tracing::info!(playlist_id, "mercury: fetching URI list");
            let (mut uris, revision) = metadata::playlist_track_uris(&session, playlist_id).await?;
            let mraw: Vec<i64> = uris.iter().map(|(_, h)| *h).collect();
            let muniq: std::collections::HashSet<i64> = mraw.iter().copied().collect();
            tracing::info!(
                playlist_id,
                uris = uris.len(),
                rev = %revision,
                unique_hints = muniq.len(),
                first3 = ?&mraw[..mraw.len().min(3)],
                last3 = ?&mraw[mraw.len().saturating_sub(3)..],
                "mercury: URI list ok"
            );
            // Sort the FULL URI list newest-first before any slicing. The
            // hint comes from mercury's `attributes.timestamp` when
            // meaningful, else playlist position. Sorting before pagination
            // is what makes page 1 the newest adds rather than the
            // playlist's oldest-added prefix.
            uris.sort_by_key(|b| std::cmp::Reverse(b.1));
            (session, uris, revision)
        };
        let total = sorted_uris.len();

        // Cache lookup. Key includes the mercury revision so a playlist
        // edit invalidates every stored page automatically.
        let pl_id_str = playlist_id
            .strip_prefix("spotify:playlist:")
            .unwrap_or(playlist_id);
        // Cache key carries a schema version (v2) so a code change to
        // sort_hint/year_hint semantics auto-invalidates prior on-disk
        // entries instead of requiring the user to `rm -rf` the cache dir.
        // Bump this whenever the entry shape or hint semantics change.
        let cache_key = if revision.is_empty() {
            None
        } else {
            Some(format!(
                "spotify:plpage:v2:{pl_id_str}::rev={revision}::off={offset}"
            ))
        };
        if let Some(ref key) = cache_key {
            if let Some((cached, cached_rev)) = self.browse_cache.get_raw(key).await {
                if cached_rev.as_deref() == Some(revision.as_str()) {
                    tracing::info!(key = %key, n = cached.len(), "mercury: cache hit");
                    // On offset=0, opportunistically pull every cached
                    // subsequent page for this revision and concatenate.
                    // If the user has paged through the whole playlist in
                    // a previous session, re-open serves all 500+ rows
                    // instantly with no "load more" click needed. If any
                    // page is missing from cache, we fall back to the
                    // normal "page 0 + load more sentinel" behavior.
                    if offset == 0 {
                        return Ok(self
                            .assemble_cached_pages(pl_id_str, &revision, total, cached)
                            .await);
                    }
                    return Ok(cached);
                }
            }
        }

        // Paginate the visible window.
        let end = (offset + PAGE_LIMIT).min(total);
        let window_uris: Vec<String> = sorted_uris
            .get(offset..end)
            .unwrap_or(&[])
            .iter()
            .map(|(u, _)| u.clone())
            .collect();
        tracing::info!(
            offset,
            end,
            total,
            window = window_uris.len(),
            "mercury: hydrate window"
        );
        let hydrated = hydrate_uris_to_entries(&session, &self.api, &self.http, &window_uris).await;
        tracing::info!(offset, end, "mercury: hydrate done");
        let mut out: Vec<Entry> = window_uris
            .iter()
            .zip(hydrated)
            .enumerate()
            .filter_map(|(i, (_uri, maybe_entry))| {
                let mut e = maybe_entry?;
                if let Some(d) = e.display.as_mut() {
                    d.sort_hint = sorted_uris.get(offset + i).map(|(_, h)| *h);
                }
                Some(e)
            })
            .collect();
        if end < total {
            out.push(load_more_entry(base_path, end));
        }

        // Permanent write under the revision-keyed key. Re-open of the
        // same page within the same revision is instant.
        if let Some(key) = cache_key {
            let _ = self
                .browse_cache
                .put_with_revision(&key, out.clone(), Some(revision))
                .await;
        }

        Ok(out)
    }

    /// Walk the per-page cache forward from `page_0` and concatenate every
    /// stored page for `(playlist_id, revision)` up to `total`. Strips the
    /// trailing `(load more)` sentinel from intermediate pages so the
    /// joined list reads like one continuous tracklist. Returns just
    /// `page_0` (with its sentinel intact) the moment a page is missing,
    /// so the UI still shows a "load more" affordance for the unfetched
    /// remainder.
    async fn assemble_cached_pages(
        &self,
        pl_id_str: &str,
        revision: &str,
        total: usize,
        page_0: Vec<Entry>,
    ) -> Vec<Entry> {
        // Strip trailing load_more sentinel; we'll only re-add it if we
        // bail mid-walk.
        let mut all = page_0.clone();
        let had_sentinel = matches!(
            all.last(),
            Some(e) if matches!(e.kind, EntryKind::Directory)
                && e.uri.contains("?offset=")
        );
        if had_sentinel {
            all.pop();
        }
        let mut off = PAGE_LIMIT;
        while off < total {
            let key = format!("spotify:plpage:v2:{pl_id_str}::rev={revision}::off={off}");
            let Some((mut page, rev)) = self.browse_cache.get_raw(&key).await else {
                // Missing page — return only what we have plus the
                // original sentinel so the user can resume paging.
                return page_0;
            };
            if rev.as_deref() != Some(revision) {
                return page_0;
            }
            let page_had_sentinel = matches!(
                page.last(),
                Some(e) if matches!(e.kind, EntryKind::Directory)
                    && e.uri.contains("?offset=")
            );
            if page_had_sentinel {
                page.pop();
            }
            all.extend(page);
            off += PAGE_LIMIT;
        }
        tracing::info!(
            playlist_id = %pl_id_str,
            total = all.len(),
            "mercury: assembled all pages from cache"
        );
        all
    }

    /// Song radio: ~30 similar tracks seeded on a single track. Goes
    /// through librespot's apollo-station mercury endpoint — Spotify's
    /// `/v1/recommendations` Web API endpoint was deprecated for
    /// non-allowlisted apps in Nov 2024 (returns 404).
    async fn browse_song_radio(&self, track_id: &str) -> Result<Vec<Entry>> {
        self.ensure_player().await?;
        let g = self.player.lock().await;
        let p = g
            .as_ref()
            .ok_or_else(|| anyhow!("spotify player not initialised"))?;
        let tracks = metadata::song_radio(&p.session, track_id, 30).await?;
        Ok(tracks
            .into_iter()
            .map(|t| Entry {
                uri: t.uri.clone(),
                label: format!("{} — {}", t.artist_name.as_deref().unwrap_or(""), t.name),
                kind: EntryKind::Track,
                display: Some(ItemDisplay {
                    title: t.name,
                    artist: t.artist_name,
                    album: t.album_name,
                    art_uri: t.art_url,
                    art_uri_full: t.art_url_full,
                    duration: Some(std::time::Duration::from_millis(t.duration_ms as u64)),
                    sort_hint: None,
                    track_no: None,
                    year_hint: None,
                }),
            })
            .collect())
    }

    /// Related artists via mercury (Web API endpoint deprecated).
    async fn browse_artist_related(&self, artist_id: &str) -> Result<Vec<Entry>> {
        self.ensure_player().await?;
        let g = self.player.lock().await;
        let p = g
            .as_ref()
            .ok_or_else(|| anyhow!("spotify player not initialised"))?;
        let related = metadata::artist_related(&p.session, artist_id).await?;
        Ok(related
            .into_iter()
            .map(|r| Entry {
                uri: r.uri,
                label: r.name.clone(),
                kind: EntryKind::Artist,
                display: Some(ItemDisplay {
                    title: r.name,
                    artist: None,
                    album: None,
                    art_uri: r.portrait_url,
                    art_uri_full: None,
                    duration: None,
                    sort_hint: None,
                    track_no: None,
                    year_hint: None,
                }),
            })
            .collect())
    }
    async fn browse_uncached(&self, path: &str) -> Result<Vec<Entry>> {
        let (base_path, offset) = parse_offset(path);
        // Session-bound mercury paths run before grabbing the api lock so we
        // don't deadlock against ensure_player(), which itself takes api.lock.
        if let Some(rest) = base_path.strip_prefix("spotify:artistview:") {
            if let Some((id_str, sub)) = rest.rsplit_once(':') {
                match sub {
                    "top" => return self.browse_artist_top(id_str).await,
                    "albums" => return self.browse_artist_albums(id_str).await,
                    "related" => return self.browse_artist_related(id_str).await,
                    _ => {}
                }
            }
        }
        // Song-radio: seed recommendations from a single track. Returns
        // ~30 similar tracks. Path: "spotify:radio:track:<id>".
        if let Some(id) = base_path.strip_prefix("spotify:radio:track:") {
            return self.browse_song_radio(id).await;
        }
        // Add-to-playlist picker source: user's own writable playlists,
        // not the followed/curated ones (which the user can't modify).
        if base_path == "spotify:view:saved_playlists_picker" {
            let api = self.api.lock().await;
            governor::instance()
                .enter()
                .await
                .map_err(anyhow::Error::new)?;
            let me = api
                .current_user()
                .await
                .map_err(classify_api_err)
                .context("current_user")?;
            let me_id = me.id.id().to_string();
            let mut out: Vec<Entry> = Vec::new();
            let mut s = api.current_user_playlists();
            while let Some(next) = s.next().await {
                let Ok(pl) = next else { continue };
                if pl.owner.id.id() != me_id {
                    continue;
                }
                let art = pl
                    .images
                    .iter()
                    .min_by_key(|i| i.width.unwrap_or(0))
                    .map(|i| i.url.clone());
                out.push(Entry {
                    uri: pl.id.to_string(),
                    label: pl.name.clone(),
                    kind: EntryKind::Playlist,
                    display: Some(ItemDisplay {
                        title: pl.name,
                        artist: pl.owner.display_name.clone(),
                        album: None,
                        art_uri: art,
                        art_uri_full: None,
                        duration: None,
                        sort_hint: None,
                        track_no: None,
                        year_hint: None,
                    }),
                });
            }
            return Ok(out);
        }
        // Discover Weekly: Spotify removed algorithmic playlists from the
        // public Web API (`current_user_playlists` silently omits them).
        // Walk the user's playlist rootlist via mercury — the same path
        // the desktop client uses — to recover the personalized URI.
        let curated = match base_path {
            "spotify:view:discover_weekly" => Some("Discover Weekly"),
            _ => None,
        };
        if let Some(name) = curated {
            self.ensure_player().await?;
            let g = self.player.lock().await;
            let p = g
                .as_ref()
                .ok_or_else(|| anyhow!("spotify player not initialised"))?;
            let pid = metadata::find_user_playlist_id_by_name(&p.session, name).await?;
            drop(g);
            return Box::pin(self.browse_uncached(&format!("spotify:playlist:{pid}"))).await;
        }
        let api = self.api.lock().await;
        // Gate + pace this browse through the rate-limit governor (fail fast if
        // a cooldown is active). The per-arm paginator error handlers below
        // engage the gate on a 429 that slips through mid-pagination.
        governor::instance()
            .enter()
            .await
            .map_err(anyhow::Error::new)?;
        match base_path {
            "" | "spotify:" => self.browse_root().await,
            "spotify:view:saved_albums" => {
                let mut out = Vec::new();
                let mut s = api.current_user_saved_albums(None).skip(offset);
                while let Some(next) = s.next().await {
                    if out.len() >= PAGE_LIMIT {
                        break;
                    }
                    let saved = next
                        .map_err(classify_api_err)
                        .context("paginated saved_albums")?;
                    let a = &saved.album;
                    let art = a
                        .images
                        .iter()
                        .min_by_key(|i| i.width.unwrap_or(0))
                        .map(|i| i.url.clone());
                    let year: Option<i32> = a.release_date.get(..4).and_then(|s| s.parse().ok());
                    let year_suffix = year.map(|y| format!(" ({y})")).unwrap_or_default();
                    // Sort hint = release-date timestamp so "RecentlyAdded"
                    // axis renders newest-released first (matches the artist
                    // page convention the user expects on this tab).
                    let sort_hint = parse_release_date_to_ts(&a.release_date);
                    out.push(Entry {
                        uri: a.id.to_string(),
                        label: format!(
                            "{} — {}{}",
                            a.artists.first().map(|x| x.name.as_str()).unwrap_or(""),
                            a.name,
                            year_suffix,
                        ),
                        kind: EntryKind::Album,
                        display: Some(ItemDisplay {
                            title: a.name.clone(),
                            artist: a.artists.first().map(|x| x.name.clone()),
                            album: None,
                            art_uri: art,
                            art_uri_full: None,
                            duration: None,
                            sort_hint,
                            track_no: None,
                            year_hint: None,
                        }),
                    });
                }
                if out.len() == PAGE_LIMIT {
                    out.push(load_more_entry(base_path, offset + PAGE_LIMIT));
                }
                Ok(out)
            }
            "spotify:view:saved_shows" => {
                // Paginate via get_saved_show stream (Show = saved show wrapper).
                let mut out = Vec::new();
                let mut s = api.get_saved_show().skip(offset);
                while let Some(next) = s.next().await {
                    if out.len() >= PAGE_LIMIT {
                        break;
                    }
                    let saved = next
                        .map_err(classify_api_err)
                        .context("paginated saved_shows")?;
                    let sh = &saved.show;
                    let art = sh
                        .images
                        .iter()
                        .min_by_key(|i| i.width.unwrap_or(0))
                        .map(|i| i.url.clone());
                    let art_full = sh
                        .images
                        .iter()
                        .max_by_key(|i| i.width.unwrap_or(0))
                        .map(|i| i.url.clone());
                    out.push(Entry {
                        // ShowId Display emits "spotify:show:<id>"; no double prefix.
                        uri: sh.id.to_string(),
                        label: sh.name.clone(),
                        kind: EntryKind::Playlist,
                        display: Some(ItemDisplay {
                            title: sh.name.clone(),
                            artist: Some("(podcast)".into()),
                            album: None,
                            art_uri: art,
                            art_uri_full: art_full,
                            duration: None,
                            // Saved Show.added_at is ISO 8601 String, not
                            // DateTime — parse the YYYY-MM-DD prefix.
                            sort_hint: parse_release_date_to_ts(
                                saved.added_at.get(..10).unwrap_or(&saved.added_at),
                            ),
                            track_no: None,
                            year_hint: None,
                        }),
                    });
                }
                if out.len() == PAGE_LIMIT {
                    out.push(load_more_entry(base_path, offset + PAGE_LIMIT));
                }
                Ok(out)
            }
            "spotify:view:saved_tracks" => {
                use rspotify::model::{Page, SavedTrack};
                let mut out = Vec::new();
                let mut page_off = offset;
                // Paged via raw::get_normalized (not rspotify's paginator) so a
                // 429 reads the real Retry-After header and sets an accurate
                // gate — a direct paginator can only guess a fallback.
                while out.len() < PAGE_LIMIT {
                    let page: Page<SavedTrack> = raw::get_normalized(
                        &api,
                        &self.http,
                        "me/tracks",
                        &[
                            ("limit", raw::SAFE_LIMIT.to_string()),
                            ("offset", page_off.to_string()),
                        ],
                    )
                    .await
                    .context("paginated saved_tracks")?;
                    let got = page.items.len();
                    for saved in &page.items {
                        let t = &saved.track;
                        let mut it = track_to_item(t);
                        it.display.sort_hint = Some(saved.added_at.timestamp());
                        out.push(Entry {
                            uri: it.uri.clone(),
                            label: format!(
                                "{} — {}",
                                it.display.artist.as_deref().unwrap_or(""),
                                it.display.title
                            ),
                            kind: EntryKind::Track,
                            display: Some(it.display),
                        });
                    }
                    if got == 0 || page.next.is_none() {
                        break;
                    }
                    page_off += got;
                }
                if out.len() >= PAGE_LIMIT {
                    out.truncate(PAGE_LIMIT);
                    out.push(load_more_entry(base_path, offset + PAGE_LIMIT));
                }
                Ok(out)
            }
            "spotify:view:playlists" => {
                // Per-item resilience: rspotify's strict deser will choke on a
                // single playlist with an unusual shape (e.g. owner missing
                // `external_urls`) and abort the whole list — the symptom user
                // reports as "sometimes playlists don't load". Skip the bad
                // entry, log, keep going. If even the first paginate call
                // 403s, fall back to the raw normalized helper.
                use rspotify::model::{Page, SimplifiedPlaylist};
                let mut out: Vec<Entry> = Vec::new();
                let mut consecutive_errors = 0usize;
                {
                    let mut s = api.current_user_playlists().skip(offset);
                    while let Some(next) = s.next().await {
                        if out.len() >= PAGE_LIMIT {
                            break;
                        }
                        let pl = match next {
                            Ok(pl) => {
                                consecutive_errors = 0;
                                pl
                            }
                            Err(e) => {
                                tracing::warn!("playlist item deser skipped: {e:?}");
                                consecutive_errors += 1;
                                if consecutive_errors >= 3 {
                                    // Bail to raw fallback when the stream is
                                    // pathologically broken.
                                    out.clear();
                                    break;
                                }
                                continue;
                            }
                        };
                        let art = pl
                            .images
                            .iter()
                            .min_by_key(|i| i.width.unwrap_or(0))
                            .map(|i| i.url.clone());
                        out.push(Entry {
                            uri: pl.id.to_string(),
                            label: pl.name.clone(),
                            kind: EntryKind::Playlist,
                            display: Some(ItemDisplay {
                                title: pl.name.clone(),
                                artist: pl.owner.display_name.clone(),
                                album: None,
                                art_uri: art,
                                art_uri_full: None,
                                duration: None,
                                sort_hint: None,
                                track_no: None,
                                year_hint: None,
                            }),
                        });
                    }
                }
                // If rspotify's stream gave us nothing AND we didn't hit
                // PAGE_LIMIT, retry through the raw helper which patches
                // missing-field shapes that rspotify's deser rejects.
                if out.is_empty() {
                    let path = "me/playlists";
                    let res: Result<Page<SimplifiedPlaylist>> = raw::get_normalized(
                        &api,
                        &self.http,
                        path,
                        &[
                            ("limit", raw::SAFE_LIMIT.to_string()),
                            ("offset", offset.to_string()),
                        ],
                    )
                    .await;
                    if let Ok(page) = res {
                        for pl in page.items {
                            let art = pl
                                .images
                                .iter()
                                .min_by_key(|i| i.width.unwrap_or(0))
                                .map(|i| i.url.clone());
                            out.push(Entry {
                                uri: pl.id.to_string(),
                                label: pl.name.clone(),
                                kind: EntryKind::Playlist,
                                display: Some(ItemDisplay {
                                    title: pl.name.clone(),
                                    artist: pl.owner.display_name.clone(),
                                    album: None,
                                    art_uri: art,
                                    art_uri_full: None,
                                    duration: None,
                                    sort_hint: None,
                                    track_no: None,
                                    year_hint: None,
                                }),
                            });
                        }
                    }
                }
                if out.len() == PAGE_LIMIT {
                    out.push(load_more_entry(base_path, offset + PAGE_LIMIT));
                }
                Ok(out)
            }
            "spotify:view:followed_artists" => {
                // 1) Followed artists (Spotify caps at 50/page; paginate in v2 if needed).
                use rspotify::model::FullArtist;
                let followed_page = api
                    .current_user_followed_artists(None, Some(50))
                    .await
                    .map_err(classify_api_err)
                    .context("followed_artists")?;
                let mut by_id: std::collections::HashMap<String, FullArtist> =
                    std::collections::HashMap::new();
                for a in followed_page.items {
                    by_id.insert(a.id.to_string(), a);
                }
                // 2) Mine first ~250 saved tracks for featured-artist IDs.
                use futures::StreamExt;
                let mut stream = api.current_user_saved_tracks(None);
                let mut featured_ids: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                let mut scanned = 0usize;
                while let Some(item) = stream.next().await {
                    match item {
                        Ok(saved) => {
                            for art in &saved.track.artists {
                                if let Some(id) = &art.id {
                                    let s = id.to_string();
                                    if !by_id.contains_key(&s) {
                                        featured_ids.insert(s);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!("saved_tracks scan: {e:?}");
                            break;
                        }
                    }
                    scanned += 1;
                    if scanned >= 250 {
                        break;
                    }
                }
                // 3) Resolve featured artist IDs to FullArtist (50 per request cap).
                let ids: Vec<rspotify::model::ArtistId> = featured_ids
                    .iter()
                    .filter_map(|s| rspotify::model::ArtistId::from_id(s).ok())
                    .collect();
                for chunk in ids.chunks(50) {
                    // rspotify marks `artists` deprecated because Spotify
                    // hardened it for non-allowlisted apps; it still works
                    // for our allowlisted dev app, so suppress the lint.
                    #[allow(deprecated)]
                    let res = api.artists(chunk.iter().cloned()).await;
                    match res {
                        Ok(full_list) => {
                            for full in full_list {
                                by_id.entry(full.id.to_string()).or_insert(full);
                            }
                        }
                        Err(e) => {
                            if governor::is_rate_limit_err(&e) {
                                break;
                            }
                            tracing::warn!("artists batch: {e:?}");
                        }
                    }
                }
                // 4) Build entries, alpha sorted.
                let mut artists: Vec<FullArtist> = by_id.into_values().collect();
                artists.sort_by(|a, b| {
                    a.name
                        .to_ascii_lowercase()
                        .cmp(&b.name.to_ascii_lowercase())
                });
                let mut out = Vec::new();
                for a in artists {
                    let art = a
                        .images
                        .iter()
                        .min_by_key(|i| i.width.unwrap_or(0))
                        .map(|i| i.url.clone());
                    out.push(Entry {
                        uri: a.id.to_string(),
                        label: a.name.clone(),
                        kind: EntryKind::Artist,
                        display: Some(ItemDisplay {
                            title: a.name.clone(),
                            artist: None,
                            album: None,
                            art_uri: art,
                            art_uri_full: None,
                            duration: None,
                            sort_hint: None,
                            track_no: None,
                            year_hint: None,
                        }),
                    });
                }
                Ok(out)
            }
            "spotify:view:recently_played" => {
                // rspotify chokes on FullTrack payloads missing
                // `external_ids`. Hit raw endpoint so JSON normalization
                // backfills the field.
                use rspotify::model::{CursorBasedPage, PlayHistory};
                let page: CursorBasedPage<PlayHistory> = raw::get_normalized(
                    &api,
                    &self.http,
                    "me/player/recently-played",
                    &[("limit", raw::SAFE_LIMIT.to_string())],
                )
                .await
                .context("recently_played")?;
                let mut out = Vec::new();
                for ph in page.items {
                    let it = track_to_item(&ph.track);
                    out.push(Entry {
                        uri: it.uri.clone(),
                        label: format!(
                            "{} — {}",
                            it.display.artist.as_deref().unwrap_or(""),
                            it.display.title
                        ),
                        kind: EntryKind::Track,
                        display: Some(it.display),
                    });
                }
                Ok(out)
            }
            "spotify:view:top_tracks" => {
                // rspotify's wrapper hits the deprecated query shape and 403s
                // on non-allowlisted apps. Raw endpoint with normalize gets
                // through (recently_played pattern) for apps that still have
                // access. Spotify caps `limit` at 50 here; SAFE_LIMIT (20) is
                // safely under it.
                use rspotify::model::{FullTrack, Page};
                let page: Page<FullTrack> = raw::get_normalized(
                    &api,
                    &self.http,
                    "me/top/tracks",
                    &[("limit", raw::SAFE_LIMIT.to_string())],
                )
                .await
                .context("top_tracks")?;
                let mut out = Vec::new();
                for track in page.items {
                    let it = track_to_item(&track);
                    out.push(Entry {
                        uri: it.uri.clone(),
                        label: format!(
                            "{} — {}",
                            it.display.artist.as_deref().unwrap_or(""),
                            it.display.title
                        ),
                        kind: EntryKind::Track,
                        display: Some(it.display),
                    });
                }
                Ok(out)
            }
            "spotify:view:top_artists" => {
                use rspotify::model::{FullArtist, Page};
                let page: Page<FullArtist> = raw::get_normalized(
                    &api,
                    &self.http,
                    "me/top/artists",
                    &[("limit", raw::SAFE_LIMIT.to_string())],
                )
                .await
                .context("top_artists")?;
                let mut out = Vec::new();
                for a in page.items {
                    let art = a
                        .images
                        .iter()
                        .min_by_key(|i| i.width.unwrap_or(0))
                        .map(|i| i.url.clone());
                    let art_full = a
                        .images
                        .iter()
                        .max_by_key(|i| i.width.unwrap_or(0))
                        .map(|i| i.url.clone());
                    out.push(Entry {
                        uri: a.id.to_string(),
                        label: a.name.clone(),
                        kind: EntryKind::Artist,
                        display: Some(ItemDisplay {
                            title: a.name.clone(),
                            artist: None,
                            album: None,
                            art_uri: art,
                            art_uri_full: art_full,
                            duration: None,
                            sort_hint: None,
                            track_no: None,
                            year_hint: None,
                        }),
                    });
                }
                Ok(out)
            }
            uri if uri.starts_with("spotify:album:") => {
                let id = AlbumId::from_id_or_uri(uri).context("parse album id")?;
                // Fetch the album once for its cover images, then attach the
                // same cover to every track entry. The big Now-Playing art
                // pane reads `art_uri_full` from the queued item; without
                // this the pane is blank when starting playback from an
                // album view.
                let album = api
                    .album(id.clone(), None)
                    .await
                    .context("album metadata")?;
                let art_uri = album
                    .images
                    .iter()
                    .min_by_key(|i| i.width.unwrap_or(0))
                    .map(|i| i.url.clone());
                let art_uri_full = album
                    .images
                    .iter()
                    .max_by_key(|i| i.width.unwrap_or(0))
                    .map(|i| i.url.clone());
                let album_name = Some(album.name.clone());
                // Albums rarely exceed PAGE_LIMIT but cap anyway so the trait
                // contract stays consistent.
                let tracks: Vec<rspotify::model::SimplifiedTrack> = api
                    .album_track(id, None)
                    .skip(offset)
                    .take(PAGE_LIMIT)
                    .try_collect()
                    .await
                    .context("album tracks")?;
                let hit_cap = tracks.len() == PAGE_LIMIT;
                let mut out = Vec::new();
                for tr in tracks {
                    out.push(Entry {
                        uri: tr.id.as_ref().map(|i| i.to_string()).unwrap_or_default(),
                        label: format!(
                            "{} — {}",
                            tr.artists.first().map(|x| x.name.as_str()).unwrap_or(""),
                            tr.name
                        ),
                        kind: EntryKind::Track,
                        display: Some(ItemDisplay {
                            title: tr.name.clone(),
                            artist: tr.artists.first().map(|x| x.name.clone()),
                            album: album_name.clone(),
                            art_uri: art_uri.clone(),
                            art_uri_full: art_uri_full.clone(),
                            duration: Some(std::time::Duration::from_millis(
                                tr.duration.num_milliseconds() as u64,
                            )),
                            sort_hint: None,
                            track_no: Some(tr.track_number),
                            year_hint: None,
                        }),
                    });
                }
                if hit_cap {
                    out.push(load_more_entry(base_path, offset + PAGE_LIMIT));
                }
                Ok(out)
            }
            uri if uri.starts_with("spotify:playlist:") => {
                // Fetch the entire playlist in one browse call. Spotify Web
                // API returns playlist items in playlist order (oldest-added
                // first by default). Without the full set, auto-sort can
                // only reorder the prefix the user happened to load — newest
                // tracks live near the end of the playlist and would never
                // surface at the top. 100/page is the endpoint's hard cap.
                //
                // Try Web API first. On 403 (Spotify-curated playlists
                // locked out of public API) or 404 (algorithmic playlists
                // like Discover Weekly / Release Radar), fall back to
                // mercury via librespot — same path the desktop client
                // uses, bypasses the public-API restriction.
                //
                // `/items` not `/tracks`: episode-containing playlists 403
                // on the legacy `/tracks` endpoint. `raw::get_normalized`
                // patches the JSON before typed parse so rspotify's strict
                // deserializer accepts null items and podcast-mixed entries.
                use rspotify::model::{Page, PlaylistItem};
                const PAGE_SIZE: usize = 100;
                let id = PlaylistId::from_id_or_uri(uri).context("parse playlist id")?;
                let path = format!("playlists/{}/items", id.id());
                let mut out: Vec<Entry> = Vec::new();
                let mut cursor = offset;
                let mut hit_forbidden = false;
                tracing::info!(playlist = %uri, offset, "playlist browse: trying Web API path");
                loop {
                    let res = raw::get_normalized::<Page<PlaylistItem>>(
                        &api,
                        &self.http,
                        &path,
                        &[
                            ("limit", PAGE_SIZE.to_string()),
                            ("offset", cursor.to_string()),
                        ],
                    )
                    .await;
                    let page = match res {
                        Ok(p) => p,
                        Err(e)
                            if cursor == offset
                                && (raw::is_forbidden(&e) || raw::is_not_found(&e)) =>
                        {
                            tracing::warn!(playlist = %uri, err = %e, "playlist browse: Web API forbidden/404 on first page, falling back to mercury");
                            hit_forbidden = true;
                            break;
                        }
                        Err(e) => {
                            tracing::error!(playlist = %uri, cursor, err = %e, "playlist browse: Web API error mid-pagination (NOT falling back to mercury)");
                            return Err(e).context("playlist tracks (normalized)");
                        }
                    };
                    // Drive pagination solely by `page.next`. `raw::normalize`
                    // strips null `items` entries (deleted / region-blocked
                    // tracks); a page can come back with 0 surviving items
                    // even though Spotify's `next` URL points to more, so
                    // bailing on `items.is_empty()` would truncate the
                    // playlist. Advance `cursor` by the request size, not
                    // the surviving-items count, to keep playlist-position
                    // alignment.
                    let no_more = page.next.is_none();
                    for (idx, item) in page.items.into_iter().enumerate() {
                        // Web API returns null `added_at` for local files and
                        // some imported items. Fall back to the playlist
                        // position so those items still sort newest-first
                        // (later playlist position = more recently added in
                        // the default un-reordered case). Real timestamps are
                        // ~1.7e9; positions are <1e5, so position fallbacks
                        // sort below dated items rather than mixing.
                        let pos_fallback = (cursor + idx) as i64;
                        let added_ts = item.added_at.map(|t| t.timestamp()).unwrap_or(pos_fallback);
                        let track = match item.item {
                            Some(rspotify::model::PlayableItem::Track(t)) => t,
                            _ => continue,
                        };
                        let mut it = track_to_item(&track);
                        it.display.sort_hint = Some(added_ts);
                        out.push(Entry {
                            uri: it.uri.clone(),
                            label: format!(
                                "{} — {}",
                                it.display.artist.as_deref().unwrap_or(""),
                                it.display.title
                            ),
                            kind: EntryKind::Track,
                            display: Some(it.display),
                        });
                    }
                    cursor += PAGE_SIZE;
                    if no_more {
                        break;
                    }
                }
                if hit_forbidden {
                    // Drop the api lock before mercury (ensure_player relocks).
                    drop(api);
                    return self
                        .browse_playlist_via_mercury(uri, offset, base_path)
                        .await;
                }
                // Diag: confirm Web API path success + sort_hint quality. If
                // every hint is identical or zero, RecentlyAdded will only
                // separate by position fallback — same limitation as mercury.
                let hints: Vec<i64> = out
                    .iter()
                    .filter_map(|e| e.display.as_ref().and_then(|d| d.sort_hint))
                    .collect();
                let unique: std::collections::HashSet<i64> = hints.iter().copied().collect();
                tracing::info!(
                    playlist = %uri,
                    entries = out.len(),
                    with_hint = hints.len(),
                    unique_hints = unique.len(),
                    first3 = ?&hints[..hints.len().min(3)],
                    last3 = ?&hints[hints.len().saturating_sub(3)..],
                    "playlist browse: Web API path OK"
                );
                Ok(out)
            }
            uri if uri.starts_with("spotify:artist:") => {
                // Landing page: 3 sub-views. Real Spotify URI dispatched here.
                let id = ArtistId::from_id_or_uri(uri).context("parse artist id")?;
                let id_str = id.id().to_string();
                let mk = |sub: &str, label: &str| Entry {
                    uri: format!("spotify:artistview:{id_str}:{sub}"),
                    label: label.into(),
                    kind: EntryKind::Directory,
                    display: None,
                };
                Ok(vec![mk("top", "Top Tracks"), mk("albums", "Albums")])
            }
            uri if uri.starts_with("spotify:artistview:") => {
                // All `artistview:*` subs go through mercury — see the
                // dispatch above the api lock. Reaching here means an
                // unrecognized sub.
                Err(anyhow!("unknown artistview path: {uri}"))
            }
            uri if uri.starts_with("spotify:show:") => {
                // Use the raw HTTP path that bypasses rspotify's typed call
                // (mirrors spotatui's `get_show_episodes`): the typed wrapper
                // 400s on shows whose episodes contain fields rspotify's
                // SimplifiedEpisode no longer expects, and Spotify itself
                // rejects calls with no market parameter from some token
                // combinations even when the spec says it's optional.
                use rspotify::model::{Page, SimplifiedEpisode};
                // Spotify caps the shows/{id}/episodes `limit` at 50;
                // PAGE_LIMIT (200, used elsewhere) triggers "Invalid limit".
                const SHOW_EP_LIMIT: usize = 50;
                let id = ShowId::from_id_or_uri(uri).context("parse show id")?;
                let path = format!("shows/{}/episodes", id.id());
                let page: Page<SimplifiedEpisode> = raw::get_normalized(
                    &api,
                    &self.http,
                    &path,
                    &[
                        ("limit", SHOW_EP_LIMIT.to_string()),
                        ("offset", offset.to_string()),
                        ("market", "from_token".to_string()),
                    ],
                )
                .await
                .context("show episodes")?;
                let hit_cap = page.items.len() == SHOW_EP_LIMIT;
                let mut out = Vec::new();
                for ep in page.items {
                    // Episode release_date is "YYYY-MM-DD" / "YYYY-MM" / "YYYY".
                    // Parse leading digits → year, then a coarse epoch so newer
                    // episodes sort to top under RecentlyAdded (we treat
                    // "release date" as the same axis for podcasts).
                    let sort_hint = parse_release_date_to_ts(&ep.release_date);
                    out.push(Entry {
                        // EpisodeId Display emits "spotify:episode:<id>"; no double.
                        uri: ep.id.to_string(),
                        label: ep.name.clone(),
                        kind: EntryKind::Track,
                        display: Some(ItemDisplay {
                            title: ep.name,
                            artist: None,
                            album: None,
                            art_uri: ep
                                .images
                                .iter()
                                .min_by_key(|i| i.width.unwrap_or(0))
                                .map(|i| i.url.clone()),
                            art_uri_full: ep
                                .images
                                .iter()
                                .max_by_key(|i| i.width.unwrap_or(0))
                                .map(|i| i.url.clone()),
                            duration: Some(std::time::Duration::from_millis(
                                ep.duration.num_milliseconds() as u64,
                            )),
                            sort_hint,
                            track_no: None,
                            year_hint: None,
                        }),
                    });
                }
                if hit_cap {
                    out.push(load_more_entry(base_path, offset + SHOW_EP_LIMIT));
                }
                Ok(out)
            }
            other => Err(anyhow!("unknown Spotify path: {other}")),
        }
    }
}

/// Parse a Spotify "YYYY-MM-DD" / "YYYY-MM" / "YYYY" release_date into a
/// Unix seconds timestamp. Used as `sort_hint` for podcast episodes so
/// release-date sorts work without dragging in chrono parsing every site.
fn parse_release_date_to_ts(s: &str) -> Option<i64> {
    let mut parts = s.split('-');
    let y: i32 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next().and_then(|x| x.parse().ok()).unwrap_or(1);
    let d: u32 = parts.next().and_then(|x| x.parse().ok()).unwrap_or(1);
    // Cheap epoch approximation — exact UTC math not needed for sort order.
    let days_since_epoch = ((y - 1970) as i64) * 365 + ((m as i64) - 1) * 30 + (d as i64 - 1);
    Some(days_since_epoch * 86_400)
}

/// Strip an `?offset=N` suffix from a path. Returns (base_path, offset).
fn parse_offset(path: &str) -> (&str, usize) {
    if let Some((base, q)) = path.rsplit_once("?offset=") {
        if let Ok(n) = q.parse::<usize>() {
            return (base, n);
        }
    }
    (path, 0)
}

/// Sentinel entry appended at the end of a paged result. Activating it
/// re-browses the same path with the next offset.
fn load_more_entry(base_path: &str, next_offset: usize) -> Entry {
    Entry {
        uri: format!("{base_path}?offset={next_offset}"),
        label: format!("(load more — next {next_offset})"),
        kind: EntryKind::Directory,
        display: None,
    }
}

/// Map a search-result `FullArtist` to an `Item` so the unified search list
/// shows artists alongside tracks. URI uses the `spotify:artist:` form so
/// activating the row navigates into the artist view; the duration column
/// stays empty, the artist name takes the title slot.
fn artist_to_item(artist: &rspotify::model::FullArtist) -> Item {
    let art_uri = artist
        .images
        .iter()
        .min_by_key(|i| i.width.unwrap_or(0))
        .map(|i| i.url.clone());
    let art_uri_full = artist
        .images
        .iter()
        .max_by_key(|i| i.width.unwrap_or(0))
        .map(|i| i.url.clone());
    Item {
        // ArtistId Display already emits "spotify:artist:<id>"; don't double.
        uri: artist.id.to_string(),
        display: ItemDisplay {
            title: artist.name.clone(),
            artist: Some("(artist)".into()),
            album: None,
            art_uri,
            art_uri_full,
            duration: None,
            sort_hint: None,
            track_no: None,
            year_hint: None,
        },
    }
}

fn album_to_item(album: &rspotify::model::SimplifiedAlbum) -> Item {
    let art_uri = album
        .images
        .iter()
        .min_by_key(|i| i.width.unwrap_or(0))
        .map(|i| i.url.clone());
    let art_uri_full = album
        .images
        .iter()
        .max_by_key(|i| i.width.unwrap_or(0))
        .map(|i| i.url.clone());
    // AlbumId Display already emits "spotify:album:<id>"; don't double.
    let uri = album
        .id
        .as_ref()
        .map(|id| id.to_string())
        .unwrap_or_default();
    Item {
        uri,
        display: ItemDisplay {
            title: album.name.clone(),
            artist: album.artists.first().map(|a| a.name.clone()),
            album: Some("(album)".into()),
            art_uri,
            art_uri_full,
            duration: None,
            sort_hint: None,
            track_no: None,
            year_hint: None,
        },
    }
}

/// Hydrate a list of `spotify:track:{base62}` URIs into `Entry`s, with
/// mercury's dealer (`metadata::hydrate_tracks`) as the primary path and a
/// single-track Web API fallback (`/v1/tracks/{id}`) for whatever mercury
/// returns as `(unavailable)`. Returns one slot per input URI, preserving
/// order; `None` means even the Web API fallback failed (region-blocked or
/// truly deleted). Free function so background tasks can call it without
/// borrowing `SpotifySource`.
async fn hydrate_uris_to_entries(
    session: &librespot::core::session::Session,
    api: &Arc<Mutex<AuthCodePkceSpotify>>,
    http: &reqwest::Client,
    uris: &[String],
) -> Vec<Option<Entry>> {
    let mut tracks = metadata::hydrate_tracks(session, uris).await;

    // Web API single-track fallback for the placeholders mercury leaves
    // behind. spotatui pattern — `/v1/tracks/{id}` is less strict than
    // mercury's `/metadata/4/track` and survives the gaps the dealer's
    // rate limiter creates. Paced at 250ms each via `raw::get_normalized`.
    let placeholders: Vec<usize> = tracks
        .iter()
        .enumerate()
        .filter_map(|(i, t)| match t {
            Some(t) if t.name == "(unavailable)" => Some(i),
            _ => None,
        })
        .collect();
    for i in placeholders {
        let uri = &uris[i];
        let Some(base62) = uri.strip_prefix("spotify:track:") else {
            continue;
        };
        let path = format!("tracks/{base62}");
        // Re-acquire the api lock per call instead of holding it across the
        // whole loop. Holding through N * (pace + network) = many seconds
        // blocks every other Spotify caller (the foreground browse the
        // user just clicked, the background hydrate for the previous
        // playlist, etc). Releasing between calls lets other tasks
        // interleave; the lock is held only for the duration of one
        // Web API round-trip.
        // Per-call timeout: Web API single-track fetch is paced 250ms +
        // network. A stuck call would block the whole placeholder loop
        // (each iteration is serial against the api lock), and the user's
        // "load more" click would never return. 5s is generous for a
        // single GET that normally lands in <300ms.
        let res = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let api_g = api.lock().await;
            raw::get_normalized::<rspotify::model::FullTrack>(&api_g, http, &path, &[]).await
        })
        .await;
        let res = match res {
            Ok(r) => r,
            Err(_) => {
                tracing::warn!(uri = %uri, "web-api track fallback timed out (5s); skipping");
                continue;
            }
        };
        if let Ok(t) = res {
            let it = track_to_item(&t);
            // Parse "YYYY-MM-DD" / "YYYY-MM" / "YYYY" → year. Drives the
            // `Year` sort axis when this track came in via the Web API
            // fallback (mercury wrote `year` directly from its parsed
            // date, but the fallback path needs to re-parse here).
            let year_from_release = t
                .album
                .release_date
                .as_deref()
                .and_then(|s| s.get(..4))
                .and_then(|s| s.parse::<i32>().ok());
            if let Some(slot) = tracks.get_mut(i) {
                *slot = Some(metadata::ArtistTrack {
                    uri: uri.clone(),
                    name: it.display.title,
                    artist_name: it.display.artist,
                    album_name: it.display.album,
                    art_url: it.display.art_uri,
                    art_url_full: it.display.art_uri_full,
                    duration_ms: it
                        .display
                        .duration
                        .map(|d| d.as_millis() as u32)
                        .unwrap_or(0),
                    year: year_from_release,
                });
            }
        }
    }

    tracks
        .into_iter()
        .zip(uris.iter())
        .map(|(maybe, uri)| {
            // Drop tracks whose name is still "(unavailable)" (both mercury
            // and Web API failed). Caller treats `None` as "skip the row".
            let t = maybe?;
            if t.name == "(unavailable)" {
                return None;
            }
            Some(Entry {
                uri: uri.clone(),
                label: format!("{} — {}", t.artist_name.as_deref().unwrap_or(""), t.name),
                kind: EntryKind::Track,
                display: Some(ItemDisplay {
                    title: t.name,
                    artist: t.artist_name,
                    album: t.album_name,
                    art_uri: t.art_url,
                    art_uri_full: t.art_url_full,
                    duration: Some(std::time::Duration::from_millis(t.duration_ms as u64)),
                    sort_hint: None,
                    track_no: None,
                    year_hint: t.year,
                }),
            })
        })
        .collect()
}

fn track_to_item(track: &rspotify::model::FullTrack) -> Item {
    let title = track.name.clone();
    let artist = track.artists.first().map(|a| a.name.clone());
    let album = Some(track.album.name.clone());
    let art_uri = track
        .album
        .images
        .iter()
        .min_by_key(|i| i.width.unwrap_or(0))
        .map(|i| i.url.clone());
    let art_uri_full = track
        .album
        .images
        .iter()
        .max_by_key(|i| i.width.unwrap_or(0))
        .map(|i| i.url.clone());
    Item {
        uri: track
            .id
            .as_ref()
            .map(|id| id.to_string())
            .unwrap_or_default(),
        display: ItemDisplay {
            title,
            artist,
            album,
            art_uri,
            art_uri_full,
            duration: Some(std::time::Duration::from_millis(
                track.duration.num_milliseconds() as u64,
            )),
            sort_hint: None,
            track_no: None,
            year_hint: None,
        },
    }
}

/// Map a search-result `SimplifiedPlaylist` to an `Item`. Used to surface
/// playlists in the unified search list.
fn playlist_to_item(pl: &rspotify::model::SimplifiedPlaylist) -> Item {
    let art_uri = pl
        .images
        .iter()
        .min_by_key(|i| i.width.unwrap_or(0))
        .map(|i| i.url.clone());
    let art_uri_full = pl
        .images
        .iter()
        .max_by_key(|i| i.width.unwrap_or(0))
        .map(|i| i.url.clone());
    Item {
        // PlaylistId Display already emits "spotify:playlist:<id>".
        uri: pl.id.to_string(),
        display: ItemDisplay {
            title: pl.name.clone(),
            artist: Some("(playlist)".into()),
            album: pl.owner.display_name.clone(),
            art_uri,
            art_uri_full,
            duration: None,
            sort_hint: None,
            track_no: None,
            year_hint: None,
        },
    }
}

/// Map a search-result `SimplifiedShow` (podcast) to an `Item`.
fn show_to_item(s: &rspotify::model::SimplifiedShow) -> Item {
    let art_uri = s
        .images
        .iter()
        .min_by_key(|i| i.width.unwrap_or(0))
        .map(|i| i.url.clone());
    let art_uri_full = s
        .images
        .iter()
        .max_by_key(|i| i.width.unwrap_or(0))
        .map(|i| i.url.clone());
    Item {
        // ShowId Display already emits "spotify:show:<id>".
        uri: s.id.to_string(),
        display: ItemDisplay {
            title: s.name.clone(),
            artist: Some("(podcast)".into()),
            album: None,
            art_uri,
            art_uri_full,
            duration: None,
            sort_hint: None,
            track_no: None,
            year_hint: None,
        },
    }
}

#[async_trait]
impl MusicSource for SpotifySource {
    fn scheme(&self) -> &'static str {
        "spotify"
    }

    fn display_name(&self) -> &'static str {
        "Spotify"
    }

    async fn search(&self, query: &str) -> Result<Vec<Item>> {
        if query.is_empty() {
            return Ok(Vec::new());
        }
        tracing::info!("spotify search start: q={query:?}");
        // Mirror spotatui:
        //   * 5 result kinds: Track, Album, Playlist, Show, Artist.
        //   * rspotify for Track/Album/Playlist/Show (no market — passing it
        //     makes Spotify omit `available_markets` which rspotify's models
        //     require).
        //   * Raw HTTP for Artist via `raw::get_normalized` — rspotify's
        //     FullArtist deserialize fails on artist search results which
        //     omit fields like `images`/`genres`/`href`. Normalize backfills.
        //   * SEARCH_LIMIT=10 (Spotify cut search limit max from 50 to 10
        //     in Feb 2026; >10 returns "Invalid limit").
        //   * Fan out parallel via tokio::join! so search latency is one
        //     round trip, not 5.
        use rspotify::model::{FullArtist, Page, SearchResult, SearchType};
        let api = self.api.lock().await;
        let mut out: Vec<Item> = Vec::new();

        // Build SEARCH_PAGES rspotify futures per type (offset = 0, 10, ...).
        // Spotify's combined-limit cap of 10 means we paginate to reach 20.
        // Each `api.search(...)` is its own borrow of `api`; collecting them
        // into Vec<_> resolves the lifetime to a single concrete future per
        // call. join_all then drives them concurrently.
        let mut track_futs = Vec::with_capacity(SEARCH_PAGES as usize);
        let mut album_futs = Vec::with_capacity(SEARCH_PAGES as usize);
        let mut playlist_futs = Vec::with_capacity(SEARCH_PAGES as usize);
        let mut show_futs = Vec::with_capacity(SEARCH_PAGES as usize);
        for i in 0..SEARCH_PAGES {
            let off = Some(i * SEARCH_LIMIT);
            track_futs.push(api.search(
                query,
                SearchType::Track,
                None,
                None,
                Some(SEARCH_LIMIT),
                off,
            ));
            album_futs.push(api.search(
                query,
                SearchType::Album,
                None,
                None,
                Some(SEARCH_LIMIT),
                off,
            ));
            playlist_futs.push(api.search(
                query,
                SearchType::Playlist,
                None,
                None,
                Some(SEARCH_LIMIT),
                off,
            ));
            show_futs.push(api.search(
                query,
                SearchType::Show,
                None,
                None,
                Some(SEARCH_LIMIT),
                off,
            ));
        }

        #[derive(serde::Deserialize)]
        struct ArtistsResp {
            artists: Page<FullArtist>,
        }
        let mut artist_queries: Vec<Vec<(&str, String)>> =
            Vec::with_capacity(SEARCH_PAGES as usize);
        for i in 0..SEARCH_PAGES {
            artist_queries.push(vec![
                ("q", query.to_string()),
                ("type", "artist".to_string()),
                ("limit", SEARCH_LIMIT.to_string()),
                ("offset", (i * SEARCH_LIMIT).to_string()),
            ]);
        }
        let artist_futs: Vec<_> = artist_queries
            .iter()
            .map(|q| raw::get_normalized::<ArtistsResp>(&api, &self.http, "search", q))
            .collect();

        let track_join = futures::future::join_all(track_futs);
        let album_join = futures::future::join_all(album_futs);
        let playlist_join = futures::future::join_all(playlist_futs);
        let show_join = futures::future::join_all(show_futs);
        let artist_join = futures::future::join_all(artist_futs);

        let (tracks, albums, playlists, shows, artists) = tokio::join!(
            track_join,
            album_join,
            playlist_join,
            show_join,
            artist_join,
        );

        let mut track_total = 0usize;
        for r in tracks {
            match r {
                Ok(SearchResult::Tracks(p)) => {
                    track_total += p.items.len();
                    out.extend(p.items.iter().map(track_to_item));
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("search tracks err: {e:#}"),
            }
        }
        tracing::info!("search tracks: {track_total} items");

        let mut album_total = 0usize;
        for r in albums {
            match r {
                Ok(SearchResult::Albums(p)) => {
                    album_total += p.items.len();
                    out.extend(p.items.iter().map(album_to_item));
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("search albums err: {e:#}"),
            }
        }
        tracing::info!("search albums: {album_total} items");

        let mut playlist_total = 0usize;
        for r in playlists {
            match r {
                Ok(SearchResult::Playlists(p)) => {
                    playlist_total += p.items.len();
                    out.extend(p.items.iter().map(playlist_to_item));
                }
                Ok(_) => {}
                // Playlist deserialize fails intermittently on null fields;
                // spotatui silently swallows. Match.
                Err(e) => tracing::warn!("search playlists err: {e:#}"),
            }
        }
        tracing::info!("search playlists: {playlist_total} items");

        let mut show_total = 0usize;
        for r in shows {
            match r {
                Ok(SearchResult::Shows(p)) => {
                    show_total += p.items.len();
                    out.extend(p.items.iter().map(show_to_item));
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("search shows err: {e:#}"),
            }
        }
        tracing::info!("search shows: {show_total} items");

        let mut artist_total = 0usize;
        for r in artists {
            match r {
                Ok(r) => {
                    artist_total += r.artists.items.len();
                    out.extend(r.artists.items.iter().map(artist_to_item));
                }
                Err(e) => tracing::warn!("search artists err: {e:#}"),
            }
        }
        tracing::info!("search artists: {artist_total} items");

        tracing::info!("spotify search end: {} total items", out.len());
        Ok(out)
    }

    async fn browse(&self, path: &str) -> Result<Vec<Entry>> {
        // Cache layer: instant hit for fresh entries (< 5 min). Stale and
        // miss both fall through to the live API walk; we then refresh the
        // cache with the new entries. No background refetch in this rev —
        // stale paths re-pay one API walk every TTL window, but the cost is
        // bounded and tab switches stay snappy in the common case.
        if cache::is_cacheable(path) {
            match self.browse_cache.get(path).await {
                cache::CacheHit::Fresh(e) => return Ok(e),
                cache::CacheHit::Stale(_) | cache::CacheHit::Miss => {}
            }
        }
        let result = self.browse_uncached(path).await;
        if let Ok(ref entries) = result {
            if cache::is_cacheable(path) {
                let _ = self.browse_cache.put(path, entries.clone()).await;
            }
        }
        result
    }

    async fn invalidate(&self, path: &str) {
        self.browse_cache.invalidate(path).await;
    }

    async fn view_snapshot(&self, path: &str) -> Option<String> {
        let api = self.api.lock().await;
        if path == "spotify:view:saved_tracks" {
            // Total liked-song count via a 1-item page — a cheap change token
            // (no per-track hydration). Add-then-remove nets the same count
            // and is missed; manual refresh (`r`) covers that rare case.
            // Routed through `raw::get_normalized` so the poll shares the
            // rate-limit governor and — being the regular header-bearing call
            // — reads the real `Retry-After` on a 429 to set an accurate gate.
            let page: rspotify::model::Page<rspotify::model::SavedTrack> = raw::get_normalized(
                &api,
                &self.http,
                "me/tracks",
                &[("limit", "1".to_string()), ("offset", "0".to_string())],
            )
            .await
            .ok()?;
            return Some(format!("n={}", page.total));
        }
        if path.starts_with("spotify:playlist:") {
            // Web API snapshot_id changes on any edit. `fields=snapshot_id`
            // keeps the response tiny.
            let pid = rspotify::model::PlaylistId::from_id_or_uri(path).ok()?;
            #[derive(serde::Deserialize)]
            struct Snapshot {
                snapshot_id: String,
            }
            let snap: Snapshot = raw::get_normalized(
                &api,
                &self.http,
                &format!("playlists/{}", pid.id()),
                &[("fields", "snapshot_id".to_string())],
            )
            .await
            .ok()?;
            return Some(snap.snapshot_id);
        }
        None
    }

    async fn browse_streaming(
        &self,
        path: &str,
        tx: tokio::sync::mpsc::Sender<Result<Vec<Entry>>>,
    ) {
        // Only saved_albums is paginated heavily enough to benefit from
        // streaming today (PAGE_LIMIT=200 = ~4 round trips per browse).
        // Other paginated paths fall through to the single-batch default
        // until we have time to convert each one's per-item builder into
        // a shareable helper.
        let (base, offset) = parse_offset(path);
        if base != "spotify:view:saved_albums" {
            let _ = tx.send(self.browse(path).await).await;
            return;
        }
        // Cache hit short-circuits streaming: emit the cached page as a
        // single batch. Mirrors `browse()` so a warm cache feels identical.
        if cache::is_cacheable(path) {
            if let cache::CacheHit::Fresh(e) = self.browse_cache.get(path).await {
                let _ = tx.send(Ok(e)).await;
                return;
            }
        }
        const BATCH: usize = 50;
        let api = self.api.lock().await;
        if let Err(e) = governor::instance().enter().await {
            let _ = tx.send(Err(anyhow::Error::new(e))).await;
            return;
        }
        let mut all: Vec<Entry> = Vec::with_capacity(PAGE_LIMIT);
        let mut batch: Vec<Entry> = Vec::with_capacity(BATCH);
        let mut s = api.current_user_saved_albums(None).skip(offset);
        while let Some(next) = s.next().await {
            if all.len() >= PAGE_LIMIT {
                break;
            }
            let saved = match next {
                Ok(s) => s,
                Err(e) => {
                    if !batch.is_empty() {
                        let _ = tx.send(Ok(std::mem::take(&mut batch))).await;
                    }
                    let _ = tx
                        .send(Err(classify_api_err(e).context("paginated saved_albums")))
                        .await;
                    return;
                }
            };
            let a = &saved.album;
            let art = a
                .images
                .iter()
                .min_by_key(|i| i.width.unwrap_or(0))
                .map(|i| i.url.clone());
            let year: Option<i32> = a.release_date.get(..4).and_then(|s| s.parse().ok());
            let year_suffix = year.map(|y| format!(" ({y})")).unwrap_or_default();
            let sort_hint = parse_release_date_to_ts(&a.release_date);
            let entry = Entry {
                uri: a.id.to_string(),
                label: format!(
                    "{} — {}{}",
                    a.artists.first().map(|x| x.name.as_str()).unwrap_or(""),
                    a.name,
                    year_suffix,
                ),
                kind: EntryKind::Album,
                display: Some(ItemDisplay {
                    title: a.name.clone(),
                    artist: a.artists.first().map(|x| x.name.clone()),
                    album: None,
                    art_uri: art,
                    art_uri_full: None,
                    duration: None,
                    sort_hint,
                    track_no: None,
                    year_hint: None,
                }),
            };
            all.push(entry.clone());
            batch.push(entry);
            if batch.len() >= BATCH && tx.send(Ok(std::mem::take(&mut batch))).await.is_err() {
                return;
            }
        }
        if all.len() == PAGE_LIMIT {
            let sentinel = load_more_entry(base, offset + PAGE_LIMIT);
            all.push(sentinel.clone());
            batch.push(sentinel);
        }
        if !batch.is_empty() {
            let _ = tx.send(Ok(batch)).await;
        }
        if cache::is_cacheable(path) {
            let _ = self.browse_cache.put(path, all).await;
        }
    }

    async fn resolve(&self, uri: &str) -> Result<Playable> {
        Ok(Playable::LibraryUri(uri.to_string()))
    }

    async fn play(&self, playable: &Playable) -> Result<()> {
        use crate::source::spotify::player::{PlayableKind, parse_playable_uri};
        let uri = match playable {
            Playable::Url(u) | Playable::LibraryUri(u) => u.as_str(),
        };
        let (kind, id) = parse_playable_uri(uri)?;
        self.ensure_player().await?;
        let g = self.player.lock().await;
        let p = g.as_ref().ok_or_else(|| anyhow!("player gone"))?;
        match kind {
            PlayableKind::Track => p.load_track(id)?,
            PlayableKind::Episode => p.load_episode(id)?,
        }
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        if let Some(p) = self.player.lock().await.as_ref() {
            // Wait for librespot to actually flush before returning. Without this
            // a subsequent source.play() races against still-playing Spotify audio
            // and both streams sound at once. 750ms is the upper bound; a
            // half-second silent gap is acceptable per the dispatcher contract.
            p.stop_and_wait(std::time::Duration::from_millis(750)).await;
        }
        Ok(())
    }

    async fn pause(&self) -> Result<()> {
        if let Some(p) = self.player.lock().await.as_ref() {
            p.pause();
        }
        Ok(())
    }

    async fn resume(&self) -> Result<()> {
        // If the session died while paused (librespot idle keepalive timeout),
        // calling player.play() on it silently no-ops — this is the "won't
        // resume after a long pause" bug. A librespot Session can't be reused
        // once invalid, so rebuild the player and reload the same track at the
        // position it was paused at.
        let dead = {
            let g = self.player.lock().await;
            g.as_ref().map(|p| p.session.is_invalid()).unwrap_or(false)
        };
        if dead {
            // Capture what was playing and where, from the dead player.
            let resume_at = {
                let g = self.player.lock().await;
                match g.as_ref() {
                    Some(p) => {
                        let pos_ms = p
                            .playback_status()
                            .await
                            .elapsed
                            .as_millis()
                            .min(u32::MAX as u128) as u32;
                        p.current().map(|(kind, id)| (kind, id, pos_ms))
                    }
                    None => None,
                }
            };
            self.ensure_player().await?; // rebuilds: session was invalid
            if let Some((kind, id, pos_ms)) = resume_at {
                let g = self.player.lock().await;
                let p = g
                    .as_ref()
                    .ok_or_else(|| anyhow!("player gone after rebuild"))?;
                p.load_at(kind, &id, pos_ms)?;
            }
            return Ok(());
        }
        if let Some(p) = self.player.lock().await.as_ref() {
            p.resume();
        }
        Ok(())
    }

    async fn playback_status(&self) -> Result<Option<PlaybackStatus>> {
        let g = self.player.lock().await;
        match g.as_ref() {
            Some(p) => Ok(Some(p.playback_status().await)),
            None => Ok(None),
        }
    }

    fn rate_limit_remaining(&self) -> Option<std::time::Duration> {
        governor::instance().remaining_block()
    }

    async fn set_volume(&self, vol: u8) -> Result<()> {
        if let Some(p) = self.player.lock().await.as_ref() {
            p.set_volume(vol);
        }
        Ok(())
    }

    async fn is_saved(&self, uri: &str) -> Result<bool> {
        // Spotify deprecated `me/library` for new dev apps (returns 403/400
        // depending on type). The older `me/tracks` endpoints still work
        // for app-scoped tokens. Hit those directly via raw HTTP — rspotify
        // doesn't expose a non-deprecated wrapper that uses them.
        if !uri.starts_with("spotify:track:") {
            return Ok(false);
        }
        // Spotify is consolidating `me/tracks/*` into `me/library/*`; the
        // older per-type endpoints now 403 for non-allowlisted apps.
        // rspotify's `library_contains` uses the new endpoint.
        let id = TrackId::from_id_or_uri(uri).context("track id")?;
        let api = self.api.lock().await;
        let res: Vec<bool> = self
            .governed(api.library_contains([rspotify::model::LibraryId::Track(id)]))
            .await
            .context("library_contains")?;
        Ok(res.into_iter().next().unwrap_or(false))
    }

    async fn save(&self, uri: &str) -> Result<()> {
        if !uri.starts_with("spotify:track:") {
            return Ok(());
        }
        let id = TrackId::from_id_or_uri(uri).context("track id")?;
        let api = self.api.lock().await;
        self.governed(api.library_add([rspotify::model::LibraryId::Track(id)]))
            .await
            .context("library_add")?;
        Ok(())
    }

    async fn unsave(&self, uri: &str) -> Result<()> {
        if !uri.starts_with("spotify:track:") {
            return Ok(());
        }
        let id = TrackId::from_id_or_uri(uri).context("track id")?;
        let api = self.api.lock().await;
        self.governed(api.library_remove([rspotify::model::LibraryId::Track(id)]))
            .await
            .context("library_remove")?;
        Ok(())
    }

    async fn seek(&self, position: std::time::Duration) -> Result<()> {
        let g = self.player.lock().await;
        let p = g
            .as_ref()
            .ok_or_else(|| anyhow!("spotify player not initialised"))?;
        let ms = position.as_millis().min(u32::MAX as u128) as u32;
        p.seek(ms);
        Ok(())
    }

    async fn list_devices(&self) -> Result<Vec<DeviceEntry>> {
        let api = self.api.lock().await;
        let devs = self
            .governed(api.device())
            .await
            .context("Spotify devices")?;
        Ok(devs
            .into_iter()
            .filter_map(|d| {
                let id = d.id?;
                Some(DeviceEntry {
                    id,
                    name: d.name,
                    kind: format!("{:?}", d._type),
                    is_active: d.is_active,
                    volume_percent: d.volume_percent.map(|v| v.min(100) as u8),
                })
            })
            .collect())
    }

    async fn relation_uri(&self, track_uri: &str, kind: &str) -> Result<String> {
        let id = TrackId::from_id_or_uri(track_uri).context("track id")?;
        let api = self.api.lock().await;
        let track = api.track(id, None).await.context("spotify track fetch")?;
        match kind {
            "album" => {
                let aid = track
                    .album
                    .id
                    .ok_or_else(|| anyhow!("track has no album id"))?;
                Ok(aid.to_string())
            }
            "artist" => {
                let aid = track
                    .artists
                    .first()
                    .and_then(|a| a.id.clone())
                    .ok_or_else(|| anyhow!("track has no artist id"))?;
                Ok(aid.to_string())
            }
            _ => Err(anyhow!("unknown relation kind: {kind}")),
        }
    }

    async fn add_to_playlist(&self, playlist_uri: &str, track_uri: &str) -> Result<()> {
        use rspotify::model::{PlayableId, PlaylistId};
        let pid = PlaylistId::from_id_or_uri(playlist_uri).context("playlist id")?;
        let tid = TrackId::from_id_or_uri(track_uri).context("track id")?;
        let api = self.api.lock().await;
        self.governed(api.playlist_add_items(pid, [PlayableId::Track(tid)], None))
            .await
            .context("playlist_add_items")?;
        Ok(())
    }

    async fn remove_from_playlist(&self, playlist_uri: &str, track_uri: &str) -> Result<()> {
        use rspotify::model::{PlayableId, PlaylistId};
        let pid = PlaylistId::from_id_or_uri(playlist_uri).context("playlist id")?;
        let tid = TrackId::from_id_or_uri(track_uri).context("track id")?;
        let api = self.api.lock().await;
        self.governed(api.playlist_remove_all_occurrences_of_items(
            pid,
            [PlayableId::Track(tid)],
            None,
        ))
        .await
        .context("playlist_remove_all_occurrences_of_items")?;
        Ok(())
    }

    async fn transfer_to_device(&self, device_id: &str) -> Result<()> {
        let api = self.api.lock().await;
        self.governed(api.transfer_playback(device_id, Some(true)))
            .await
            .context("Spotify transfer_playback")?;
        Ok(())
    }

    async fn art(&self, uri: &str, _size: ArtSize) -> Result<Vec<u8>> {
        if !uri.starts_with("http://") && !uri.starts_with("https://") {
            return Err(anyhow!("Spotify art expects a CDN URL, got {uri}"));
        }
        let bytes = self
            .http
            .get(uri)
            .send()
            .await
            .with_context(|| format!("GET {uri}"))?
            .error_for_status()?
            .bytes()
            .await
            .context("read art body")?;
        Ok(bytes.to_vec())
    }
}

//! Mercury-based artist metadata fetches via librespot's internal protocol.
//!
//! Spotify deprecated the Web API endpoints for artist top tracks and related
//! artists; their desktop client uses the mercury (protobuf) protocol instead,
//! which is what librespot-metadata wraps. As long as a librespot Session is
//! authenticated (Premium account), these endpoints stay alive.
//!
//! Track hydration also goes through mercury here because rspotify's batch
//! `tracks(ids)` and `artists(ids)` endpoints are themselves deprecated.

use anyhow::{anyhow, Context, Result};
use futures::future::{join_all, try_join_all};
use librespot::core::Session;
use librespot::core::SpotifyUri;
use librespot::core::spotify_id::SpotifyId;
use librespot_metadata::image::Image;
use librespot_metadata::playlist::Playlist;
use librespot_metadata::playlist::item::{PlaylistItem, PlaylistItems};
use librespot_metadata::{Album, Artist, Metadata, Track};
use protobuf::Message as _;
use reqwest::Method;

pub struct RelatedArtist {
    pub uri: String,
    pub name: String,
    pub portrait_url: Option<String>,
}

pub struct ArtistAlbum {
    pub uri: String,
    pub name: String,
    pub artist_name: Option<String>,
    pub art_url: Option<String>,
    pub art_url_full: Option<String>,
    /// Release year from the album's `date` field. Drives the "(YYYY)"
    /// label suffix and newest-first sort on the artist page.
    pub year: Option<i32>,
}

pub struct ArtistTrack {
    pub uri: String,
    pub name: String,
    pub artist_name: Option<String>,
    pub album_name: Option<String>,
    pub art_url: Option<String>,
    /// Largest available cover URL — used by the now-playing pane. `None`
    /// means "fall back to `art_url`" (or no art at all).
    pub art_url_full: Option<String>,
    pub duration_ms: u32,
    /// Release year of the track's album. Drives "(YYYY)" suffix and
    /// newest-first sort on the artist-top-tracks view.
    pub year: Option<i32>,
}

/// Country used to pick per-region top tracks. Falls back to global if missing.
const DEFAULT_COUNTRY: &str = "US";

fn parse_artist_uri(id_or_uri: &str) -> Result<SpotifyUri> {
    let base62 = id_or_uri
        .strip_prefix("spotify:artist:")
        .unwrap_or(id_or_uri);
    let id = SpotifyId::from_base62(base62)
        .map_err(|e| anyhow!("parse artist id `{base62}`: {e}"))?;
    Ok(SpotifyUri::Artist { id })
}

fn smallest_image_url(images: &[Image]) -> Option<String> {
    let img = images.iter().min_by_key(|i| i.width)?;
    let hex = img.id.to_base16().ok()?;
    Some(format!("https://i.scdn.co/image/{hex}"))
}

fn largest_image_url(images: &[Image]) -> Option<String> {
    let img = images.iter().max_by_key(|i| i.width)?;
    let hex = img.id.to_base16().ok()?;
    Some(format!("https://i.scdn.co/image/{hex}"))
}

fn track_art_url(album: &Album) -> Option<String> {
    smallest_image_url(&album.covers.0)
}

fn track_art_url_full(album: &Album) -> Option<String> {
    largest_image_url(&album.covers.0)
}

fn uri_to_base62(uri: &SpotifyUri) -> Option<String> {
    SpotifyId::try_from(uri).ok()?.to_base62().ok()
}

/// Fetch the artist's top tracks (per the configured country) and hydrate each
/// to a `ArtistTrack` via mercury. Returns up to ~10 entries.
pub async fn artist_top_tracks(session: &Session, artist_id: &str) -> Result<Vec<ArtistTrack>> {
    let uri = parse_artist_uri(artist_id)?;
    let artist = Artist::get(session, &uri)
        .await
        .context("mercury artist metadata")?;
    let track_uris: Vec<SpotifyUri> = artist.top_tracks.for_country(DEFAULT_COUNTRY).0;
    let fetches = track_uris.iter().map(|u| Track::get(session, u));
    let tracks: Vec<Track> = try_join_all(fetches)
        .await
        .context("mercury top tracks hydrate")?;
    let mut out: Vec<ArtistTrack> = tracks
        .into_iter()
        .filter_map(|t| {
            let base62 = uri_to_base62(&t.id)?;
            let year = Some(t.album.date.0.year());
            Some(ArtistTrack {
                uri: format!("spotify:track:{base62}"),
                name: t.name.clone(),
                artist_name: t.artists.first().map(|a| a.name.clone()),
                album_name: Some(t.album.name.clone()),
                art_url: track_art_url(&t.album),
                art_url_full: track_art_url_full(&t.album),
                duration_ms: t.duration.max(0) as u32,
                year,
            })
        })
        .collect();
    // Newest first; stable sort preserves Spotify's per-country top-tracks
    // popularity order within the same year.
    out.sort_by_key(|x| std::cmp::Reverse(x.year));
    Ok(out)
}

/// Fetch an artist's albums + singles via mercury. Used as a fallback when
/// the Web API endpoint returns 400/403 (Spotify-side restrictions on
/// non-allowlisted dev apps). Mirrors what the desktop client does — same
/// path the player itself uses, so it works as long as the librespot session
/// is alive.
pub async fn artist_albums(
    session: &Session,
    artist_id: &str,
) -> Result<Vec<ArtistAlbum>> {
    let uri = parse_artist_uri(artist_id)?;
    let artist = Artist::get(session, &uri)
        .await
        .context("mercury artist metadata")?;
    // Walk albums + singles, taking the current release of each group so
    // duplicate variants (deluxe, remastered) collapse to a single entry.
    let album_uris: Vec<SpotifyUri> = artist
        .albums
        .current_releases()
        .chain(artist.singles.current_releases())
        .cloned()
        .collect();
    let fetches = album_uris.iter().map(|u| Album::get(session, u));
    // join_all (not try_join_all) so one missing album doesn't kill the lot.
    let results = join_all(fetches).await;
    let mut out: Vec<ArtistAlbum> = results
        .into_iter()
        .filter_map(|r| r.ok())
        .filter_map(|a| {
            let base62 = uri_to_base62(&a.id)?;
            let year = Some(a.date.0.year());
            Some(ArtistAlbum {
                uri: format!("spotify:album:{base62}"),
                name: a.name.clone(),
                artist_name: a.artists.0.first().map(|x| x.name.clone()),
                art_url: smallest_image_url(&a.covers.0),
                art_url_full: largest_image_url(&a.covers.0),
                year,
            })
        })
        .collect();
    // Newest first. Stable sort keeps mercury's per-group ordering (albums
    // then singles) within the same year.
    out.sort_by_key(|x| std::cmp::Reverse(x.year));
    Ok(out)
}

/// Song radio via librespot's apollo-station mercury endpoint. Spotify
/// deprecated `/v1/recommendations` in Nov 2024 for non-allowlisted apps
/// (404), but the desktop client's "Go to Song Radio" still works through
/// `/radio-apollo/v3/tracks/<uri>` over mercury.
///
/// Returns up to `count` similar tracks, hydrated through `Track::get` so
/// the result mirrors what `artist_top_tracks` produces. Unsupported track
/// types (podcasts, locals) are silently filtered.
pub async fn song_radio(
    session: &Session,
    track_id: &str,
    count: usize,
) -> Result<Vec<ArtistTrack>> {
    let base62 = track_id
        .strip_prefix("spotify:track:")
        .unwrap_or(track_id);
    let track_uri = format!("spotify:track:{base62}");
    let bytes = session
        .spclient()
        .get_apollo_station("tracks", &track_uri, Some(count), vec![], false)
        .await
        .map_err(|e| anyhow!("mercury apollo-station: {e}"))?;
    // The apollo-station response is JSON: { "tracks": [ { "track_uri": "spotify:track:..." }, ... ] }
    // The seed track is included in the response; filter it out.
    #[derive(serde::Deserialize)]
    struct StationResp {
        tracks: Vec<StationTrack>,
    }
    #[derive(serde::Deserialize)]
    struct StationTrack {
        track_uri: Option<String>,
        uri: Option<String>,
    }
    let resp: StationResp = serde_json::from_slice(&bytes)
        .context("apollo-station JSON decode")?;
    let seed_uri = format!("spotify:track:{base62}");
    let track_uris: Vec<SpotifyUri> = resp
        .tracks
        .into_iter()
        .filter_map(|t| t.track_uri.or(t.uri))
        .filter(|u| u != &seed_uri)
        .filter_map(|u| {
            let b62 = u.strip_prefix("spotify:track:")?;
            let id = SpotifyId::from_base62(b62).ok()?;
            Some(SpotifyUri::Track { id })
        })
        .collect();
    let fetches = track_uris.iter().map(|u| Track::get(session, u));
    let results = join_all(fetches).await;
    Ok(results
        .into_iter()
        .filter_map(|r| r.ok())
        .filter_map(|t| {
            let b62 = uri_to_base62(&t.id)?;
            let year = Some(t.album.date.0.year());
            Some(ArtistTrack {
                uri: format!("spotify:track:{b62}"),
                name: t.name.clone(),
                artist_name: t.artists.first().map(|a| a.name.clone()),
                album_name: Some(t.album.name.clone()),
                art_url: track_art_url(&t.album),
                art_url_full: track_art_url_full(&t.album),
                duration_ms: t.duration.max(0) as u32,
                year,
            })
        })
        .collect())
}

/// `(Vec<(track_uri, sort_hint)>, revision_hex)` for a playlist via
/// mercury. Cheap — one or two protobuf calls regardless of playlist size.
/// No per-track hydration; callers slice into a visible window and hydrate
/// that.
///
/// The revision is the mercury `SelectedListContent.revision` bytes
/// hex-encoded; it changes whenever Spotify rewrites the playlist
/// (add/remove/reorder). Callers compare against a cached revision to
/// decide whether a delta-hydrate suffices or the cache is up to date.
/// Empty string if the server omits revision (rare; treat as "no cache").
///
/// Sort hint: mercury `attributes.timestamp` (unix seconds) when set; else
/// `pos + 1` so curated playlists (Spotify leaves the field at 0) still
/// sort newest-at-bottom-of-playlist first. Position fallbacks (1..N)
/// stay below real timestamps (~1.7e9) so mixed lists still segregate.
pub async fn playlist_track_uris(
    session: &Session,
    playlist_id: &str,
) -> Result<(Vec<(String, i64)>, String)> {
    use librespot::protocol::playlist4_external::SelectedListContent;

    let base62 = playlist_id
        .strip_prefix("spotify:playlist:")
        .unwrap_or(playlist_id);
    let _ = SpotifyId::from_base62(base62)
        .map_err(|e| anyhow!("parse playlist id `{base62}`: {e}"))?;

    // librespot's `Playlist::get` hits `/playlist/v2/playlist/{id}` with no
    // query params, which Spotify silently caps at ~300 items. Walk the
    // endpoint directly with explicit `from` / `length` paging so playlists
    // larger than the implicit cap come back complete.
    const PAGE: usize = 500;
    let mut all_items: Vec<PlaylistItem> = Vec::new();
    let mut from: usize = 0;
    let mut iter = 0usize;
    // First-page revision is authoritative; later pages echo the same one
    // because mercury serves a consistent snapshot per `from`-walk.
    let mut revision_hex = String::new();
    loop {
        iter += 1;
        let endpoint = format!("/playlist/v2/playlist/{base62}?from={from}&length={PAGE}");
        // Mercury occasionally returns 500 (transient backend hiccup,
        // happens reliably when a background hydrate is fetching the same
        // playlist concurrently with a foreground browse). Retry a few
        // times with backoff before propagating — Spotify's own desktop
        // client does the same.
        let mut attempt = 0u8;
        let bytes = loop {
            match session
                .spclient()
                .request(&Method::GET, &endpoint, None, None)
                .await
            {
                Ok(b) => break b,
                Err(e) if attempt < 3 => {
                    let msg = e.to_string();
                    if msg.contains("500") || msg.contains("502") || msg.contains("503") {
                        let backoff = std::time::Duration::from_millis(
                            300u64 * (1u64 << attempt),
                        );
                        tracing::warn!(
                            attempt,
                            backoff_ms = backoff.as_millis() as u64,
                            "mercury playlist fetch transient err; retrying"
                        );
                        tokio::time::sleep(backoff).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(e).with_context(|| {
                        format!("mercury playlist fetch from={from}")
                    });
                }
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("mercury playlist fetch from={from} (after retries)")
                    });
                }
            }
        };
        let msg = SelectedListContent::parse_from_bytes(&bytes)
            .context("mercury playlist parse")?;
        if revision_hex.is_empty() {
            let rev = msg.revision();
            if !rev.is_empty() {
                revision_hex = rev.iter().map(|b| format!("{b:02x}")).collect();
            }
        }
        let total = msg.length() as usize;
        let page_items: PlaylistItems = msg
            .contents
            .items
            .as_slice()
            .try_into()
            .context("convert mercury playlist items")?;
        if page_items.0.is_empty() {
            break;
        }
        all_items.extend(page_items.0);
        from = all_items.len();
        if total > 0 && from >= total {
            break;
        }
        if iter >= 50 {
            tracing::warn!("mercury playlist loop bail at iter=50");
            break;
        }
    }

    // (uri, raw_ts) per track. raw_ts comes from mercury's
    // `attributes.timestamp` (unix seconds). Mercury often returns the
    // PLAYLIST's revision timestamp on every item — not per-item added-at —
    // so all values can come back identical. We post-process below to fall
    // back to playlist position when that happens.
    let raw: Vec<(String, i64)> = all_items
        .iter()
        .filter_map(|it| {
            let SpotifyUri::Track { .. } = it.id else { return None };
            let base62 = uri_to_base62(&it.id)?;
            let ts_sec = it.attributes.timestamp.as_timestamp_ms() / 1000;
            Some((format!("spotify:track:{base62}"), ts_sec))
        })
        .collect();

    // If every track shares the same timestamp (mercury returned the
    // playlist's revision instead of per-item added-at), per-item sort by
    // ts is meaningless — fall back to playlist position so RecentlyAdded
    // still ranks "newest by Spotify add-order" (later in playlist =
    // more recently added under default end-append behavior).
    let unique_ts: std::collections::HashSet<i64> = raw.iter().map(|(_, t)| *t).collect();
    let use_position = unique_ts.len() <= 1;

    let uris: Vec<(String, i64)> = raw
        .into_iter()
        .enumerate()
        .map(|(pos, (uri, ts))| {
            let hint = if use_position || ts <= 0 { (pos + 1) as i64 } else { ts };
            (uri, hint)
        })
        .collect();
    Ok((uris, revision_hex))
}

/// Hydrate a slice of track URIs via mercury `Track::get`. Bounded
/// concurrency + retry pass for the failures — keeps the dealer's
/// rate-limiter from silently dropping requests. Failures fall through to
/// a `None` so callers can render a placeholder rather than truncating.
pub async fn hydrate_tracks(
    session: &Session,
    uris: &[String],
) -> Vec<Option<ArtistTrack>> {
    let parsed: Vec<SpotifyUri> = uris
        .iter()
        .filter_map(|u| {
            let base62 = u.strip_prefix("spotify:track:")?;
            let id = SpotifyId::from_base62(base62).ok()?;
            Some(SpotifyUri::Track { id })
        })
        .collect();
    // Caller guarantees URI format; bail safely if a malformed URI snuck in.
    if parsed.len() != uris.len() {
        return uris.iter().map(|_| None).collect();
    }

    // Chunk=20 is the empirical sweet spot — chunk=50 triggered ~40%
    // silent drops from mercury's dealer; chunk=20 lands ~95% on first
    // pass. Retry catches the remainder.
    //
    // Per-call timeout: librespot's `Track::get` has no built-in deadline,
    // and the dealer occasionally silently drops a request mid-flight —
    // the await then hangs forever and the user's "load more" click never
    // returns. 4 seconds is well past p99 for a healthy dealer (~150ms);
    // anything slower is the silent-drop case where the retry pass would
    // do better anyway.
    const HYDRATE_CHUNK: usize = 20;
    const TRACK_GET_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);
    let mut tracks: Vec<Option<Track>> = (0..parsed.len()).map(|_| None).collect();
    for (chunk_idx, chunk) in parsed.chunks(HYDRATE_CHUNK).enumerate() {
        let fetches = chunk
            .iter()
            .map(|u| tokio::time::timeout(TRACK_GET_TIMEOUT, Track::get(session, u)));
        let results = join_all(fetches).await;
        for (i, r) in results.into_iter().enumerate() {
            if let Ok(Ok(t)) = r {
                tracks[chunk_idx * HYDRATE_CHUNK + i] = Some(t);
            }
        }
    }
    // Two retry passes with backoff. Most rate-limit drops clear within
    // ~300ms; region-blocked items keep failing and become placeholders.
    for delay_ms in [300u64, 800u64] {
        let misses: Vec<usize> = tracks
            .iter()
            .enumerate()
            .filter_map(|(i, t)| if t.is_none() { Some(i) } else { None })
            .collect();
        if misses.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        for chunk in misses.chunks(10) {
            let fetches = chunk
                .iter()
                .map(|&i| tokio::time::timeout(TRACK_GET_TIMEOUT, Track::get(session, &parsed[i])));
            let results = join_all(fetches).await;
            for (&i, r) in chunk.iter().zip(results) {
                if let Ok(Ok(t)) = r {
                    tracks[i] = Some(t);
                }
            }
        }
    }
    let remaining_misses = tracks.iter().filter(|t| t.is_none()).count();
    if remaining_misses > 0 {
        tracing::warn!(
            misses = remaining_misses,
            total = parsed.len(),
            "mercury hydrate: tracks left unresolved after retries (timeout or region-block)"
        );
    }

    tracks
        .into_iter()
        .zip(uris.iter())
        .map(|(maybe_track, uri)| match maybe_track {
            Some(t) => {
                let base62 = uri_to_base62(&t.id)?;
                let year = Some(t.album.date.0.year());
                Some(ArtistTrack {
                    uri: format!("spotify:track:{base62}"),
                    name: t.name.clone(),
                    artist_name: t.artists.first().map(|a| a.name.clone()),
                    album_name: Some(t.album.name.clone()),
                    art_url: track_art_url(&t.album),
                    art_url_full: track_art_url_full(&t.album),
                    duration_ms: t.duration.max(0) as u32,
                    year,
                })
            }
            None => {
                // Region-blocked / deleted — placeholder keeps the row visible.
                let base62 = uri.strip_prefix("spotify:track:")?;
                Some(ArtistTrack {
                    uri: format!("spotify:track:{base62}"),
                    name: "(unavailable)".to_string(),
                    artist_name: None,
                    album_name: None,
                    art_url: None,
                    art_url_full: None,
                    duration_ms: 0,
                    year: None,
                })
            }
        })
        .collect()
}

/// Walk the user's playlist rootlist via mercury (the protobuf endpoint
/// the desktop client uses) and find the playlist whose name matches
/// `name` (case-insensitive). Returns the base62 playlist id.
///
/// Used to locate Spotify-curated playlists like Discover Weekly and
/// Release Radar — Spotify no longer surfaces these through the public
/// Web API (`current_user_playlists` silently omits them), but the
/// rootlist still includes anything the user follows in the desktop app.
///
/// Parses the raw protobuf directly rather than going through
/// `librespot_metadata::SelectedListContent::try_from`: the rootlist
/// contains folder markers (`spotify:start-group:...` /
/// `spotify:end-group:...`) and other non-playlist URIs that the typed
/// wrapper rejects with "ID cannot be parsed", failing the whole walk.
pub async fn find_user_playlist_id_by_name(
    session: &Session,
    name: &str,
) -> Result<String> {
    let bytes = session
        .spclient()
        .get_rootlist(0, Some(1000))
        .await
        .map_err(|e| anyhow!("mercury rootlist: {e}"))?;
    let msg = librespot::protocol::playlist4_external::SelectedListContent::parse_from_bytes(&bytes)
        .context("rootlist protobuf decode")?;
    // Collect every playlist URI from the rootlist. `items` includes folder
    // markers and `meta_items` doesn't, so they DON'T share an index —
    // hydrate each playlist via mercury `Playlist::get` and check its name.
    let contents = msg.contents.get_or_default();
    let playlist_uris: Vec<SpotifyUri> = contents
        .items
        .iter()
        .filter_map(|it| {
            let uri = it.uri();
            let base62 = uri.strip_prefix("spotify:playlist:")?;
            let id = SpotifyId::from_base62(base62).ok()?;
            Some(SpotifyUri::Playlist { id, user: None })
        })
        .collect();
    tracing::info!(
        "rootlist contains {} playlists; searching for `{name}`",
        playlist_uris.len()
    );
    let fetches = playlist_uris.iter().map(|u| Playlist::get(session, u));
    let results = join_all(fetches).await;
    let mut found_names: Vec<String> = Vec::new();
    for (uri, res) in playlist_uris.iter().zip(results) {
        let Ok(pl) = res else { continue };
        found_names.push(pl.attributes.name.clone());
        if pl.attributes.name.eq_ignore_ascii_case(name) {
            let SpotifyUri::Playlist { id, .. } = uri else {
                continue;
            };
            return id
                .to_base62()
                .map_err(|e| anyhow!("base62 encode: {e}"));
        }
    }
    tracing::warn!(
        "playlist `{name}` not in rootlist. Rootlist names: {found_names:?}"
    );
    Err(anyhow!(
        "playlist `{name}` not in your library — open the Spotify app once to follow it (rootlist had {} playlists; see the fuga log for names)",
        found_names.len()
    ))
}

/// Fetch related artists embedded in the artist metadata payload. Each entry
/// carries name + id; portraits are usually present too.
pub async fn artist_related(session: &Session, artist_id: &str) -> Result<Vec<RelatedArtist>> {
    let uri = parse_artist_uri(artist_id)?;
    let artist = Artist::get(session, &uri)
        .await
        .context("mercury artist metadata")?;
    Ok(artist
        .related
        .0
        .into_iter()
        .filter_map(|a| {
            let base62 = uri_to_base62(&a.id)?;
            let portrait_url = smallest_image_url(&a.portraits.0);
            Some(RelatedArtist {
                uri: format!("spotify:artist:{base62}"),
                name: a.name.clone(),
                portrait_url,
            })
        })
        .collect())
}

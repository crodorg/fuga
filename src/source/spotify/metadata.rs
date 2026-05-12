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
use librespot_metadata::{Album, Artist, Metadata, Track};
use protobuf::Message as _;

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

/// Fetch a playlist's tracks via mercury. Used as a fallback when the Web
/// API endpoint returns 403 (Spotify-curated / algorithmic playlists are no
/// longer accessible to non-allowlisted apps via the public API).
///
/// Walks `Playlist::contents.items`, batch-hydrates each track via mercury,
/// and silently drops items that fail to hydrate (deleted, region-blocked,
/// or non-track URIs like episodes).
pub async fn playlist_tracks(
    session: &Session,
    playlist_id: &str,
) -> Result<Vec<ArtistTrack>> {
    let base62 = playlist_id
        .strip_prefix("spotify:playlist:")
        .unwrap_or(playlist_id);
    let id = SpotifyId::from_base62(base62)
        .map_err(|e| anyhow!("parse playlist id `{base62}`: {e}"))?;
    let uri = SpotifyUri::Playlist {
        id,
        user: None,
    };
    let playlist = Playlist::get(session, &uri)
        .await
        .context("mercury playlist fetch")?;
    // Filter to track URIs; skip episode/local entries.
    let track_uris: Vec<SpotifyUri> = playlist
        .contents
        .items
        .0
        .iter()
        .filter_map(|it| match it.id {
            SpotifyUri::Track { .. } => Some(it.id.clone()),
            _ => None,
        })
        .collect();
    let fetches = track_uris.iter().map(|u| Track::get(session, u));
    // Use join_all (not try_join_all) so one bad track doesn't kill the lot.
    let results = join_all(fetches).await;
    Ok(results
        .into_iter()
        .filter_map(|r| r.ok())
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
        .collect())
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
        "playlist `{name}` not in your library — open the Spotify app once to follow it (rootlist had {} playlists; see ~/.cache/fuga/fuga.log for names)",
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

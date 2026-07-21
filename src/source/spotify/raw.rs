//! Raw HTTP path that bypasses rspotify's strict deserializer for endpoints
//! where Spotify's response shape diverges from rspotify's model.
//!
//! Mirrors spotatui's `spotify_get_typed_compat_for` + `normalize_spotify_payload`
//! pattern. The Spotify
//! Web API returns null entries inside `items` arrays for deleted/blocked
//! tracks, and omits several fields rspotify's `FullTrack` requires; rspotify
//! parses fail and the playlist appears empty. We patch the JSON before
//! typed deserialize.
//!
//! Scope: this module is intentionally tiny — only the patches we need today.
//! Add new patches as new endpoints surface new shape divergences.
//!
//! Used by `mod.rs` for the `playlist:items` endpoint.

use anyhow::{Context, Result, anyhow};
use rspotify::AuthCodePkceSpotify;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::time::Duration;

/// Conservative per-page limit for Spotify list endpoints. Different
/// endpoints accept different maxes (some 50, some 100, some 20), and
/// Spotify silently changes them. 20 is below every known cap, so callers
/// don't have to remember which endpoint takes which limit. Loop on
/// pagination instead of asking for one big page.
pub const SAFE_LIMIT: usize = 20;

/// GET <https://api.spotify.com/v1/{path}>?{query}, normalize the JSON,
/// then deserialize into `T`.
///
/// Resilience layers:
/// - routed through the [`governor`](super::governor): ≥250ms pacing + a
///   fail-fast gate that returns immediately while a rate-limit window is open
/// - 401 → refresh token, retry once
/// - 429 → record the real `Retry-After` in the governor and return
///   [`RateLimited`](super::governor::RateLimited); never sleep inline (the
///   value can be hours)
/// - transient connect/timeout/request errors → backoff, retry up to 3 times
///
/// Uses a fresh `reqwest::Client` (not the caller's `self.http`) so the
/// default User-Agent is sent. Spotify search/quasi-internal endpoints are
/// known to misbehave on custom UAs.
pub async fn get_normalized<T: DeserializeOwned>(
    api: &AuthCodePkceSpotify,
    _http: &reqwest::Client,
    path: &str,
    query: &[(&str, String)],
) -> Result<T> {
    let http = reqwest::Client::new();
    let url = build_url(path, query)?;
    let mut refreshed = false;
    let mut attempt: u8 = 0;
    const MAX_ATTEMPTS: u8 = 4;

    loop {
        let token = current_token(api).await?;
        super::governor::instance()
            .enter()
            .await
            .map_err(anyhow::Error::new)?;
        let resp_result = http.get(url.clone()).bearer_auth(&token).send().await;

        let resp = match resp_result {
            Ok(r) => r,
            Err(e) => {
                if attempt + 1 < MAX_ATTEMPTS
                    && (e.is_connect() || e.is_timeout() || e.is_request())
                {
                    tokio::time::sleep(Duration::from_secs(1 + u64::from(attempt))).await;
                    attempt += 1;
                    continue;
                }
                return Err(anyhow!("spotify GET send: {e}"));
            }
        };

        let status = resp.status();
        if status.is_success() {
            let body = resp.text().await.context("spotify GET read body")?;
            let mut value: Value =
                serde_json::from_str(&body).context("spotify GET decode json")?;
            normalize(&mut value);
            match serde_json::from_value::<T>(value.clone()) {
                Ok(parsed) => return Ok(parsed),
                Err(e) => {
                    let snippet: String = body.chars().take(400).collect();
                    tracing::error!(
                        path = %path,
                        error = %e,
                        body_snippet = %snippet,
                        "spotify normalized parse failed",
                    );
                    return Err(anyhow!("typed parse: {e}"));
                }
            }
        }

        if status == reqwest::StatusCode::UNAUTHORIZED && !refreshed {
            super::auth::refresh_preserving(api)
                .await
                .context("spotify token refresh")?;
            refreshed = true;
            continue;
        }

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            // Record the real Retry-After (this is the one transport that can
            // read the header) and fail fast — never sleep inline; the value
            // can be hours. The governor gates every other call until it
            // passes, and persists it across restarts.
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|h| h.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(30)
                .max(1);
            let d = Duration::from_secs(retry_after);
            super::governor::instance().note_rate_limited(d);
            return Err(super::governor::RateLimited(d).into());
        }

        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("spotify GET {} {} failed: {}", path, status, body));
    }
}

fn build_url(path: &str, query: &[(&str, String)]) -> Result<reqwest::Url> {
    let mut url =
        reqwest::Url::parse(&format!("https://api.spotify.com/v1/{path}")).context("parse url")?;
    if !query.is_empty() {
        let mut qp = url.query_pairs_mut();
        for (k, v) in query {
            qp.append_pair(k, v);
        }
    }
    Ok(url)
}

async fn current_token(api: &AuthCodePkceSpotify) -> Result<String> {
    let tg = api.token.lock().await;
    let tg = tg.map_err(|_| anyhow!("rspotify token mutex poisoned"))?;
    let t = tg
        .as_ref()
        .ok_or_else(|| anyhow!("no Spotify access token; run --spotify-auth"))?;
    Ok(t.access_token.clone())
}

/// True iff the error was a Spotify HTTP 403 from `get_normalized`. Used by
/// callers to fall back to a different transport (e.g. librespot mercury).
pub fn is_forbidden(e: &anyhow::Error) -> bool {
    e.to_string().contains("403 Forbidden")
}

/// True iff the error was a Spotify HTTP 404 from `get_normalized`.
/// Algorithmic Spotify-curated playlists (Discover Weekly, Release Radar,
/// Daily Mixes) 404 on `/playlists/{id}/items` for non-allowlisted dev
/// apps even when the playlist URI is valid — mercury fallback recovers
/// them.
pub fn is_not_found(e: &anyhow::Error) -> bool {
    e.to_string().contains("404 Not Found")
}

/// PUT https://api.spotify.com/v1/{path}, no body, with bearer auth.
/// Refreshes the token once on 401. Returns Err on any non-2xx.
///
/// Used for endpoints rspotify routes through its deprecated/broken
/// `me/library` consolidation (Spotify's `me/library` endpoint isn't
/// available for new dev apps; the older per-type endpoints like
/// `me/tracks` still work). Caller builds the full path + query string.
pub async fn put_empty(api: &AuthCodePkceSpotify, path_with_query: &str) -> Result<()> {
    no_body_method(api, reqwest::Method::PUT, path_with_query).await
}

/// DELETE https://api.spotify.com/v1/{path}, mirrors `put_empty`.
pub async fn delete_empty(api: &AuthCodePkceSpotify, path_with_query: &str) -> Result<()> {
    no_body_method(api, reqwest::Method::DELETE, path_with_query).await
}

async fn no_body_method(
    api: &AuthCodePkceSpotify,
    method: reqwest::Method,
    path_with_query: &str,
) -> Result<()> {
    let http = reqwest::Client::new();
    let url = reqwest::Url::parse(&format!("https://api.spotify.com/v1/{path_with_query}"))
        .context("parse url")?;
    let mut refreshed = false;
    loop {
        let token = current_token(api).await?;
        super::governor::instance()
            .enter()
            .await
            .map_err(anyhow::Error::new)?;
        let resp = http
            .request(method.clone(), url.clone())
            .bearer_auth(&token)
            .header("Content-Length", "0")
            .send()
            .await
            .context("spotify send")?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        if status == reqwest::StatusCode::UNAUTHORIZED && !refreshed {
            super::auth::refresh_preserving(api)
                .await
                .context("spotify token refresh")?;
            refreshed = true;
            continue;
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|h| h.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(30)
                .max(1);
            let d = Duration::from_secs(retry_after);
            super::governor::instance().note_rate_limited(d);
            return Err(super::governor::RateLimited(d).into());
        }
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "spotify {} {} failed: {}",
            method,
            path_with_query,
            body
        ));
    }
}

/// Patch Spotify response oddities so rspotify's strict deserializer parses.
/// Mirrors spotatui's helper plus a few extra backfills rspotify 0.16.1
/// requires that spotatui (older rspotify) doesn't.
///
/// rspotify's `PlaylistItemShadow` requires `is_local: bool` with no default;
/// `FullTrack` requires `disc_number`, `explicit`, `external_urls`,
/// `external_ids`, `is_local`, `track_number`. Spotify omits some of these
/// for podcast/local entries — backfill defaults so the parse succeeds.
pub fn normalize(v: &mut Value) {
    match v {
        Value::Object(map) => {
            // Strip null items from any "items" array (deleted/region-blocked
            // entries that rspotify's PlaylistItem can't represent).
            if let Some(Value::Array(items)) = map.get_mut("items") {
                items.retain(|i| !i.is_null());
            }
            // Saved-track wrapper: surface `track` if only `item` is present.
            if map.contains_key("added_at") && !map.contains_key("track") {
                if let Some(it) = map.get("item").cloned() {
                    map.insert("track".into(), it);
                }
            }
            // PlaylistItemShadow: rspotify 0.16 needs `is_local` non-Option.
            if map.contains_key("added_at")
                && (map.contains_key("track") || map.contains_key("item"))
            {
                map.entry(String::from("is_local"))
                    .or_insert_with(|| json!(false));
            }
            // FullTrack: backfill every required-no-default field rspotify
            // expects. Some podcast-mixed / regional playlists omit these.
            if map.contains_key("album")
                && map.contains_key("artists")
                && (map.contains_key("track_number") || map.contains_key("duration_ms"))
            {
                map.entry(String::from("available_markets"))
                    .or_insert_with(|| json!([]));
                map.entry(String::from("external_ids"))
                    .or_insert_with(|| json!({}));
                map.entry(String::from("external_urls"))
                    .or_insert_with(|| json!({}));
                map.entry(String::from("linked_from"))
                    .or_insert(Value::Null);
                map.entry(String::from("popularity"))
                    .or_insert_with(|| json!(0));
                map.entry(String::from("disc_number"))
                    .or_insert_with(|| json!(1));
                map.entry(String::from("track_number"))
                    .or_insert_with(|| json!(0));
                map.entry(String::from("explicit"))
                    .or_insert_with(|| json!(false));
                map.entry(String::from("is_local"))
                    .or_insert_with(|| json!(false));
            }
            // PublicUser (added_by): rspotify requires external_urls + href +
            // id. Spotify sometimes returns just `{ id: "x" }`. Drop the
            // entire `added_by` if it can't satisfy PublicUser — we don't
            // use it anywhere.
            if let Some(added_by) = map.get_mut("added_by") {
                if let Value::Object(au) = added_by {
                    if !au.contains_key("external_urls")
                        || !au.contains_key("href")
                        || !au.contains_key("id")
                    {
                        *added_by = Value::Null;
                    }
                }
            }
            // PublicUser (owner) on SimplifiedPlaylist: same required-fields
            // story. Backfill stubs for missing scalars instead of dropping
            // (callers DO read playlist.owner.display_name). Spotify also
            // returns `null` values (not just missing keys) for these
            // fields on some playlists — overwrite nulls too, otherwise
            // rspotify's UserId fails to parse with "invalid type: null".
            if let Some(Value::Object(owner)) = map.get_mut("owner") {
                let needs_string = |v: &Value| !v.is_string();
                let needs_object = |v: &Value| !v.is_object();
                if owner.get("id").is_none_or(needs_string) {
                    owner.insert("id".into(), json!(""));
                }
                if owner.get("href").is_none_or(needs_string) {
                    owner.insert("href".into(), json!(""));
                }
                if owner.get("external_urls").is_none_or(needs_object) {
                    owner.insert("external_urls".into(), json!({}));
                }
            }
            // Recurse into nested objects so nested tracks/users get patched.
            for (_, nested) in map.iter_mut() {
                normalize(nested);
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                normalize(item);
            }
        }
        _ => {}
    }
}

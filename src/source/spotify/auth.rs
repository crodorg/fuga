use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use librespot::core::authentication::Credentials as SessionCredentials;
use librespot::core::cache::Cache as LibrespotCache;
use librespot::core::config::SessionConfig;
use librespot::core::session::Session;
use librespot::oauth::OAuthClientBuilder;
use rspotify::clients::OAuthClient;
use rspotify::{
    AuthCodePkceSpotify, Config, Credentials, OAuth, Token, prelude::BaseClient, scopes,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

const SCOPES: &[&str] = &[
    "streaming",
    "user-library-read",
    "user-library-modify",
    "user-read-playback-state",
    "user-modify-playback-state",
    "user-read-currently-playing",
    "user-read-recently-played",
    "user-top-read",
    "user-follow-read",
    "playlist-read-private",
    "playlist-read-collaborative",
    "playlist-modify-public",
    "playlist-modify-private",
];

/// Loopback port for librespot's OAuth redirect. Spotify's desktop client id
/// accepts any 127.0.0.1 port on the `/login` path (OAuth loopback rule), so
/// this needs no registration — it only has to be free while `--spotify-auth`
/// runs.
const LIBRESPOT_REDIRECT_PORT: u16 = 8898;

/// Scopes requested for the playback session. Kept to the set proven to yield
/// login5-acceptable credentials; the Web-API client carries its own scopes.
const LIBRESPOT_SCOPES: &[&str] = &[
    "app-remote-control",
    "playlist-read",
    "playlist-read-private",
    "streaming",
    "user-library-read",
    "user-read-email",
    "user-read-private",
];

/// librespot's credential cache: reusable session credentials only — no volume
/// file, no audio cache (fuga streams; MPD owns local files).
pub fn librespot_cache(data_dir: &Path) -> Result<LibrespotCache> {
    let dir = data_dir.join("librespot");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    LibrespotCache::new::<&Path>(Some(&dir), None, None, None)
        .map_err(|e| anyhow!("librespot credential cache: {e}"))
}

/// Path of the cached playback credentials inside `librespot_cache`.
pub fn librespot_credentials_path(data_dir: &Path) -> PathBuf {
    data_dir.join("librespot").join("credentials.json")
}

/// Authorize the *playback* session — a second, separate login from the
/// Web-API one, run once by `--spotify-auth`.
///
/// librespot turns session credentials into the token spclient needs to fetch
/// audio through Spotify's login5 endpoint, and login5 only accepts stored
/// credentials whose originating client id matches the one asking. Since
/// 2026-08-10 Spotify rejects credentials derived from a third-party app token
/// with INVALID_CREDENTIALS, so the Web-API token can no longer drive playback
/// (every track then fails to load at 0:00). Take a token through librespot's
/// own client id instead and cache the reusable credentials it yields; those
/// don't expire, so this never runs again unless the user revokes access.
pub async fn librespot_login(data_dir: &Path) -> Result<()> {
    let cache = librespot_cache(data_dir)?;
    let client_id = SessionConfig::default().client_id;
    let redirect = format!("http://127.0.0.1:{LIBRESPOT_REDIRECT_PORT}/login");
    println!("\nfuga: authorizing Spotify playback (a second login, for the audio session):\n");
    // The OAuth helper is blocking (it binds the redirect listener and waits).
    let token = tokio::task::spawn_blocking(move || {
        OAuthClientBuilder::new(&client_id, &redirect, LIBRESPOT_SCOPES.to_vec())
            .open_in_browser()
            .build()?
            .get_access_token()
    })
    .await
    .context("librespot OAuth task")?
    .map_err(|e| anyhow!("librespot OAuth: {e}"))?;

    let session = Session::new(SessionConfig::default(), Some(cache));
    session
        .connect(
            SessionCredentials::with_access_token(token.access_token),
            true,
        )
        .await
        .map_err(|e| anyhow!("librespot session connect: {e}"))?;
    session.shutdown();
    // The blob is full account access; librespot writes it with the process
    // umask, same trap as the Web-API token cache.
    harden_token_file(&librespot_credentials_path(data_dir));
    println!("fuga: Spotify playback authorized; session credentials cached.");
    Ok(())
}

pub fn build_client(
    client_id: &str,
    redirect_port: u16,
    cache_path: PathBuf,
) -> AuthCodePkceSpotify {
    let creds = Credentials::new_pkce(client_id);
    let mut scope_set = scopes!();
    for s in SCOPES {
        scope_set.insert((*s).to_string());
    }
    let oauth = OAuth {
        redirect_uri: format!("http://127.0.0.1:{redirect_port}/callback"),
        scopes: scope_set,
        ..Default::default()
    };
    let config = Config {
        cache_path,
        token_cached: true,
        // Disable rspotify's internal auto-refresh: it calls the plain
        // `refresh_token()`, which overwrites the cached token wholesale and
        // drops the refresh token whenever Spotify's PKCE refresh response
        // omits it (see `refresh_preserving`). We drive every refresh through
        // `refresh_preserving` instead, gated by `ensure_token_fresh`.
        token_refreshing: false,
        ..Default::default()
    };
    AuthCodePkceSpotify::with_config(creds, oauth, config)
}

/// Best-effort tighten the token cache to owner-only (0600) on unix. rspotify
/// writes it with the process umask (often world-readable), and the token
/// grants full account access. No-op if the file is absent or on non-unix.
/// `File::create` preserves an existing file's mode on rewrite, so applying
/// this once — after the file exists — sticks across refreshes.
pub fn harden_token_file(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mut perms = meta.permissions();
            if perms.mode() & 0o777 != 0o600 {
                perms.set_mode(0o600);
                if let Err(e) = std::fs::set_permissions(path, perms) {
                    tracing::warn!("could not restrict token cache to 0600: {e}");
                }
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Loads cached token if usable. Returns true if loaded from cache.
pub async fn load_cached_token(client: &AuthCodePkceSpotify) -> Result<bool> {
    match client.read_token_cache(true).await {
        Ok(Some(token)) => {
            // A cached token without a refresh token can't be renewed: it dies
            // within the hour and every Spotify call then fails with an opaque
            // `InvalidToken`. Refuse it and force a clean re-auth rather than
            // limping on. (This is the exact trap a pre-guard build could leave
            // behind — see `refresh_preserving`.)
            if token.refresh_token.is_none() {
                tracing::warn!(
                    "cached Spotify token has no refresh_token; it can't be renewed \
                     and would die within the hour — run `fuga --spotify-auth` to re-authorize"
                );
                return Ok(false);
            }
            *client.token.lock().await.unwrap() = Some(token.clone());
            // If expired, ask for a refresh.
            if token.is_expired() {
                if let Err(e) = refresh_preserving(client).await {
                    tracing::warn!("token refresh failed: {e}; need re-auth");
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Ok(None) => Ok(false),
        Err(e) => {
            tracing::warn!("token cache unreadable: {e}");
            Ok(false)
        }
    }
}

/// Refresh the access token while preserving the refresh token across refresh
/// responses that omit it.
///
/// Spotify's PKCE refresh may return `200 OK` without a `refresh_token`
/// ("When a refresh token is not returned, continue using the existing token"
/// — Spotify Web API docs), but rspotify 0.16.1 replaces the whole cached
/// token with the response, dropping the refresh token to `None`. After that
/// no further refresh is possible and Spotify silently goes dead until the
/// user re-authenticates. Snapshot the old refresh token and, if the refreshed
/// token came back without one, restore it and rewrite the cache.
pub async fn refresh_preserving(client: &AuthCodePkceSpotify) -> Result<()> {
    let prev = client
        .token
        .lock()
        .await
        .map_err(|_| anyhow!("rspotify token mutex poisoned"))?
        .as_ref()
        .and_then(|t| t.refresh_token.clone());
    client
        .refresh_token()
        .await
        .context("spotify token refresh")?;
    let restored = {
        let mut g = client
            .token
            .lock()
            .await
            .map_err(|_| anyhow!("rspotify token mutex poisoned"))?;
        match g.as_mut() {
            Some(t) => preserve_refresh_token(t, prev),
            None => false,
        }
    };
    if restored {
        client
            .write_token_cache()
            .await
            .context("rewrite token cache after refresh-token restore")?;
        tracing::info!(
            "spotify: refresh response omitted refresh_token; preserved the existing one"
        );
    }
    // Invariant: a refresh must never leave us without a refresh token. If it
    // did and there was none to restore, the account goes dead within the hour
    // — surface it loudly as an error instead of silently persisting a token
    // that only lasts one more cycle.
    let has_refresh = client
        .token
        .lock()
        .await
        .map_err(|_| anyhow!("rspotify token mutex poisoned"))?
        .as_ref()
        .is_some_and(|t| t.refresh_token.is_some());
    if !has_refresh {
        return Err(anyhow!(
            "spotify refresh produced a token without a refresh_token; re-auth required"
        ));
    }
    Ok(())
}

/// If a refresh response dropped the refresh token, restore the previous one.
/// Returns true when a restore happened, so the caller knows to rewrite the
/// token cache. Pure so it can be unit-tested without a live Spotify session.
fn preserve_refresh_token(tok: &mut Token, prev: Option<String>) -> bool {
    match (tok.refresh_token.is_none(), prev) {
        (true, Some(rt)) => {
            tok.refresh_token = Some(rt);
            true
        }
        _ => false,
    }
}

/// Run the interactive PKCE flow: print URL, await redirect on local port,
/// exchange code for token, persist to cache.
pub async fn interactive_login(client: &mut AuthCodePkceSpotify, redirect_port: u16) -> Result<()> {
    let url = client
        .get_authorize_url(None)
        .context("build authorize URL")?;
    println!("\nfuga: opening this URL in your browser to authorize Spotify:\n");
    println!("    {url}\n");
    // OS-agnostic via the `open` crate: xdg-open on Linux, `open` on macOS,
    // `start` on Windows. Fall back to printing the URL if the platform
    // can't auto-open (headless / sandboxed).
    if let Err(e) = open::that(&url) {
        println!("fuga: couldn't auto-open browser ({e}); paste the URL above manually.");
    }
    println!("Waiting for redirect on http://127.0.0.1:{redirect_port}/callback ...");

    let expected_state = client.get_oauth().state.clone();
    let code = wait_for_code(redirect_port, &expected_state).await?;
    client
        .request_token(&code)
        .await
        .context("exchange auth code for token")?;
    println!("fuga: Spotify auth complete; token cached.");
    Ok(())
}

async fn wait_for_code(port: u16, expected_state: &str) -> Result<String> {
    let bind = format!("127.0.0.1:{port}");
    let listener = TcpListener::bind(&bind)
        .await
        .with_context(|| format!("bind {bind}"))?;
    let (mut socket, _) = listener.accept().await.context("accept redirect")?;

    let mut buf = vec![0u8; 8192];
    let n = socket.read(&mut buf).await.context("read request")?;
    let req = String::from_utf8_lossy(&buf[..n]);
    let first = req.lines().next().unwrap_or_default();
    let path = first.split_whitespace().nth(1).unwrap_or_default();
    let parsed = Url::parse(&format!("http://localhost{path}"))
        .with_context(|| format!("parse redirect path: {path}"))?;

    let mut code: Option<String> = None;
    let mut state: Option<String> = None;
    for (k, v) in parsed.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            _ => {}
        }
    }

    let body = "fuga: auth complete; close this tab.\n";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;

    if state.as_deref() != Some(expected_state) {
        return Err(anyhow!("state mismatch in redirect"));
    }
    code.ok_or_else(|| anyhow!("no `code` parameter in redirect"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_dropped_refresh_token() {
        // Spotify PKCE refresh omitted the refresh token: restore the old one.
        let mut tok = Token {
            refresh_token: None,
            ..Default::default()
        };
        let restored = preserve_refresh_token(&mut tok, Some("old-rt".into()));
        assert!(restored);
        assert_eq!(tok.refresh_token.as_deref(), Some("old-rt"));
    }

    #[test]
    fn keeps_rotated_refresh_token() {
        // Refresh returned a fresh token: don't clobber it, don't rewrite cache.
        let mut tok = Token {
            refresh_token: Some("new-rt".into()),
            ..Default::default()
        };
        let restored = preserve_refresh_token(&mut tok, Some("old-rt".into()));
        assert!(!restored);
        assert_eq!(tok.refresh_token.as_deref(), Some("new-rt"));
    }

    #[test]
    fn no_previous_token_nothing_to_restore() {
        let mut tok = Token {
            refresh_token: None,
            ..Default::default()
        };
        assert!(!preserve_refresh_token(&mut tok, None));
        assert!(tok.refresh_token.is_none());
    }
}

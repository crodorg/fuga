use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use rspotify::clients::OAuthClient;
use rspotify::{AuthCodePkceSpotify, Config, Credentials, OAuth, prelude::BaseClient, scopes};
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
        token_refreshing: true,
        ..Default::default()
    };
    AuthCodePkceSpotify::with_config(creds, oauth, config)
}

/// Loads cached token if usable. Returns true if loaded from cache.
pub async fn load_cached_token(client: &AuthCodePkceSpotify) -> Result<bool> {
    match client.read_token_cache(true).await {
        Ok(Some(token)) => {
            *client.token.lock().await.unwrap() = Some(token.clone());
            // If expired, ask for a refresh.
            if token.is_expired() {
                if let Err(e) = client.refresh_token().await {
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

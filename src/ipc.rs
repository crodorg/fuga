//! Unix-socket control plane. The TUI binds a socket; `fuga <cmd>` invocations
//! connect, send a line, and read a one-line reply. Commands:
//!
//!   play <uri>      Push to queue, play immediately
//!   next            Advance queue
//!   prev            Previous track
//!   pause           Toggle pause
//!   stop            Stop current source
//!   vol <0..100>    Set master volume
//!   status          One-line "title | artist | elapsed/duration | scheme"
//!
//! Errors are reported as `err: <message>` so a script can detect failure
//! with `=~ ^err:`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

pub fn socket_path() -> PathBuf {
    // Prefer $XDG_RUNTIME_DIR, but only when it actually points at a real
    // directory. Some environments export it as `/run/user/UID`, which doesn't
    // exist on macOS — using it there makes the bind fail and (since client and
    // server must agree on the path) breaks the control plane. Fall back to
    // /tmp so both ends resolve the same usable socket.
    if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
        let dir = PathBuf::from(rt);
        if dir.is_dir() {
            return dir.join("fuga.sock");
        }
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    PathBuf::from(format!("/tmp/fuga-{user}.sock"))
}

/// Bind the control socket, trying the primary path then a `/tmp` fallback.
/// The primary (`$XDG_RUNTIME_DIR/fuga.sock`) can be unusable when the env var
/// points at a dir that doesn't exist on this OS (e.g. `/run/user/UID` on
/// macOS); falling back keeps `fuga <cmd>` working. Returns `None` only if both
/// fail. Caller MUST keep the request sender alive on `None` (see `serve`).
fn bind_socket() -> Option<UnixListener> {
    let primary = socket_path();
    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    let fallback = PathBuf::from(format!("/tmp/fuga-{user}.sock"));
    let mut paths = vec![primary];
    if !paths.contains(&fallback) {
        paths.push(fallback);
    }
    for path in paths {
        let _ = std::fs::remove_file(&path); // stale-socket cleanup
        match UnixListener::bind(&path) {
            Ok(listener) => {
                tracing::info!("ipc listening on {}", path.display());
                return Some(listener);
            }
            Err(e) => tracing::warn!("ipc bind {} failed: {e}", path.display()),
        }
    }
    None
}

/// Commands wired into the TUI event loop. The reply slot lets the socket
/// reader synchronously hand back a result line.
pub struct IpcRequest {
    pub line: String,
    pub reply: oneshot::Sender<String>,
}

pub async fn serve(tx: UnboundedSender<IpcRequest>) -> Result<()> {
    let listener = match bind_socket() {
        Some(l) => l,
        None => {
            // Couldn't bind anywhere. Park forever HOLDING `tx` rather than
            // returning (which drops the sender, closes the receiver, and
            // busy-spins the main select! loop). The control plane is simply
            // unavailable; the TUI keeps working.
            tracing::error!("ipc: could not bind a control socket; `fuga <cmd>` disabled");
            let _keep = tx;
            std::future::pending::<()>().await;
            return Ok(());
        }
    };
    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("ipc accept: {e}");
                continue;
            }
        };
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, tx).await {
                tracing::debug!("ipc client: {e}");
            }
        });
    }
}

async fn handle_client(stream: UnixStream, tx: UnboundedSender<IpcRequest>) -> Result<()> {
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);
    let mut line = String::new();
    let n = reader.read_line(&mut line).await.context("read")?;
    if n == 0 {
        return Ok(());
    }
    let (rep_tx, rep_rx) = oneshot::channel();
    tx.send(IpcRequest {
        line: line.trim().to_string(),
        reply: rep_tx,
    })
    .map_err(|_| anyhow::anyhow!("ipc channel closed"))?;
    let reply = rep_rx
        .await
        .map_err(|_| anyhow::anyhow!("reply channel dropped"))?;
    w.write_all(reply.as_bytes()).await?;
    w.write_all(b"\n").await?;
    Ok(())
}

/// Client side: send one command, print response, exit.
pub async fn client_send(cmd: &str) -> Result<String> {
    let path = socket_path();
    let stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("connect {}", path.display()))?;
    let (r, mut w) = stream.into_split();
    w.write_all(cmd.as_bytes()).await?;
    w.write_all(b"\n").await?;
    drop(w);
    let mut reader = BufReader::new(r);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    Ok(line.trim().to_string())
}

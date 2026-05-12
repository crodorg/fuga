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
    if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(rt).join("fuga.sock");
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    PathBuf::from(format!("/tmp/fuga-{user}.sock"))
}

/// Commands wired into the TUI event loop. The reply slot lets the socket
/// reader synchronously hand back a result line.
pub struct IpcRequest {
    pub line: String,
    pub reply: oneshot::Sender<String>,
}

pub async fn serve(tx: UnboundedSender<IpcRequest>) -> Result<()> {
    let path = socket_path();
    let _ = std::fs::remove_file(&path); // stale-socket cleanup
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("bind unix socket {}", path.display()))?;
    tracing::info!("ipc listening on {}", path.display());
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

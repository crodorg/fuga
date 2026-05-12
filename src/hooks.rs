use std::process::Stdio;

use crate::config::Hooks;
use crate::queue::QueuedItem;

/// Spawn a hook command in the background. Stderr/stdout are dropped so a
/// chatty hook doesn't corrupt the TUI. Errors only surface in the log.
fn spawn(cmd: &str, env: &[(&str, String)]) {
    let cmd = cmd.to_string();
    let env: Vec<(String, String)> = env.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    tokio::spawn(async move {
        let mut child = match tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .envs(env)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("hook spawn failed: {cmd}: {e}");
                return;
            }
        };
        if let Err(e) = child.wait().await {
            tracing::warn!("hook wait failed: {cmd}: {e}");
        }
    });
}

pub fn on_startup(hooks: &Hooks) {
    if let Some(cmd) = &hooks.on_startup {
        spawn(cmd, &[]);
    }
}

pub fn on_track_change(hooks: &Hooks, item: &QueuedItem) {
    if let Some(cmd) = &hooks.on_track_change {
        let env = vec![
            ("FUGA_URI", item.uri.clone()),
            ("FUGA_SOURCE", item.source_scheme.to_string()),
            ("FUGA_TITLE", item.display.title.clone()),
            (
                "FUGA_ARTIST",
                item.display.artist.clone().unwrap_or_default(),
            ),
            (
                "FUGA_ALBUM",
                item.display.album.clone().unwrap_or_default(),
            ),
        ];
        spawn(cmd, &env);
    }
}

pub fn on_source_switch(hooks: &Hooks, from: Option<&str>, to: &str) {
    if let Some(cmd) = &hooks.on_source_switch {
        let env = vec![
            ("FUGA_SOURCE_FROM", from.unwrap_or("").to_string()),
            ("FUGA_SOURCE_TO", to.to_string()),
        ];
        spawn(cmd, &env);
    }
}

mod app;
mod app_state;
mod art_cache;
mod config;
mod dispatch;
mod hooks;
mod ipc;
mod keys;
mod mpris;
mod queue;
mod source;
mod term_probe;
mod theme;
mod types;
mod ui;
mod widgets;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::app::App;
use crate::art_cache::{art_dir, ArtCache};
use crate::config::Config;
use crate::dispatch::Dispatcher;
use crate::source::local::LocalSource;
use crate::source::radio::RadioSource;
use crate::source::somafm::SomaFmSource;
use crate::source::spotify::auth as spotify_auth;
use crate::source::spotify::SpotifySource;
use crate::source::youtube::YouTubeSource;
use crate::source::MusicSource;
use crate::term_probe::{Term, ThumbMode};

#[derive(Debug, Parser)]
#[command(name = "fuga", version, about = "Terminal music library aggregator")]
struct Args {
    /// Verbose logging (info-level fuga, warn for deps).
    #[arg(long)]
    debug: bool,

    /// Override config path.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Run only the Spotify OAuth flow and exit. Use this once after setting
    /// `[spotify]` in config.toml.
    #[arg(long)]
    spotify_auth: bool,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Push a URI to the queue and play it now.
    Play { uri: String },
    /// Advance queue.
    Next,
    /// Previous track.
    Prev,
    /// Toggle pause.
    Pause,
    /// Stop the active source.
    Stop,
    /// Set master volume (0..100).
    Vol { vol: u8 },
    /// Print one line of status: `title | artist | elapsed/duration | scheme`.
    Status,
}

impl Cmd {
    fn to_line(&self) -> String {
        match self {
            Cmd::Play { uri } => format!("play {uri}"),
            Cmd::Next => "next".into(),
            Cmd::Prev => "prev".into(),
            Cmd::Pause => "pause".into(),
            Cmd::Stop => "stop".into(),
            Cmd::Vol { vol } => format!("vol {vol}"),
            Cmd::Status => "status".into(),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    async_main(None).await
}

/// Body of `main`. Split out so that platforms which need to own the OS main
/// thread for something else (macOS Cocoa run loop) can spawn this on a
/// worker thread and pass in pre-built MPRIS channels. `prebuilt_mpris=None`
/// means "spawn the MPRIS server yourself if available on this platform".
async fn async_main(prebuilt_mpris: Option<mpris::MprisHandles>) -> Result<()> {
    // rustls 0.23 requires an explicit CryptoProvider when more than one is
    // available in the dep graph. reqwest pulls aws-lc-rs; install it as the
    // process default before any TLS code runs.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let args = Args::parse();
    let _ = args.config; // override path support: v2

    // Subcommand mode: connect to a running fuga and exit. No TUI start, no
    // log file noise — these are scriptable one-shots.
    if let Some(cmd) = &args.cmd {
        let line = cmd.to_line();
        match crate::ipc::client_send(&line).await {
            Ok(reply) => {
                println!("{reply}");
                return Ok(());
            }
            Err(e) => {
                eprintln!("err: {e}");
                std::process::exit(1);
            }
        }
    }

    let filter = if args.debug {
        EnvFilter::try_new("fuga=debug,warn").unwrap()
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("fuga=info,warn"))
    };

    let config = Config::load().context("loading config")?;
    let cache_dir = config.cache_dir();
    std::fs::create_dir_all(&cache_dir).ok();
    std::fs::create_dir_all(art_dir(&cache_dir)).ok();

    let log_path = cache_dir.join("fuga.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(file)
        .with_ansi(false)
        .init();

    tracing::info!("fuga starting");

    // Standalone auth flow: log in to Spotify, persist token, exit.
    if args.spotify_auth {
        if !config.spotify.enabled || config.spotify.client_id.is_empty() {
            anyhow::bail!("[spotify] not configured: set enabled=true and client_id");
        }
        let data_dir = config.data_dir();
        std::fs::create_dir_all(&data_dir).ok();
        let token_path = data_dir.join("spotify_tokens.json");
        let mut client = spotify_auth::build_client(
            &config.spotify.client_id,
            config.spotify.redirect_port,
            token_path,
        );
        spotify_auth::interactive_login(&mut client, config.spotify.redirect_port).await?;
        return Ok(());
    }

    let conn = LocalSource::connect(
        &config.mpd.host,
        config.mpd.port,
        config.mpd.password.as_deref(),
        config.mpd.music_directory.clone(),
    )
    .await
    .context("connecting to MPD")?;

    let mpd_client = conn.client.clone();
    let local = Arc::new(conn.source);

    let http = reqwest::Client::builder()
        .user_agent(concat!("fuga/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build HTTP client")?;

    let mut dispatcher = Dispatcher::new();
    dispatcher.register(local.clone() as Arc<dyn MusicSource>);

    if !config.radio.is_empty() {
        let radio = Arc::new(RadioSource::new(
            config.radio.clone(),
            mpd_client.clone(),
            http.clone(),
        ));
        dispatcher.register(radio as Arc<dyn MusicSource>);
    }

    if config.somafm.enabled {
        let somafm = Arc::new(SomaFmSource::new(
            cache_dir.clone(),
            config.somafm.cache_ttl_hours,
            mpd_client.clone(),
            http.clone(),
        ));
        dispatcher.register(somafm as Arc<dyn MusicSource>);
    }

    let (spotify_event_tx, spotify_event_rx) = tokio::sync::mpsc::unbounded_channel();

    let mut spotify_status: Option<String> = None;

    if config.spotify.enabled && !config.spotify.client_id.is_empty() {
        if config.spotify.lossless_unsupported() {
            tracing::warn!(
                "spotify.quality=lossless requested but librespot 0.8 doesn't support FLAC/HiFi yet — falling back to OGG 320 kbps"
            );
        }
        let data_dir = config.data_dir();
        std::fs::create_dir_all(&data_dir).ok();
        let token_path = data_dir.join("spotify_tokens.json");
        let client = spotify_auth::build_client(
            &config.spotify.client_id,
            config.spotify.redirect_port,
            token_path,
        );
        match spotify_auth::load_cached_token(&client).await {
            Ok(true) => {
                let api = std::sync::Arc::new(tokio::sync::Mutex::new(client));
                let browse_cache_dir = cache_dir.join("spotify_browse");
                let browse_cache = Arc::new(
                    crate::source::spotify::cache::BrowseCache::new(browse_cache_dir, 64),
                );
                let spotify = Arc::new(SpotifySource::new(
                    api,
                    http.clone(),
                    config.spotify.clone(),
                    spotify_event_tx.clone(),
                    browse_cache,
                ));
                dispatcher.register(spotify as Arc<dyn MusicSource>);
            }
            Ok(false) => {
                tracing::warn!(
                    "Spotify token missing; run `fuga --spotify-auth` once to authorize"
                );
                spotify_status =
                    Some("Spotify not authed — run `fuga --spotify-auth`".into());
            }
            Err(e) => {
                tracing::warn!("Spotify token load error: {e}; skipping source");
                spotify_status = Some(format!("Spotify auth error: {e}"));
            }
        }
    } else if config.spotify.enabled && config.spotify.client_id.is_empty() {
        spotify_status = Some("Spotify enabled but [spotify] client_id is empty".into());
    }

    if config.youtube.enabled {
        let data_dir = config.data_dir();
        std::fs::create_dir_all(&data_dir).ok();
        let youtube = Arc::new(YouTubeSource::new(
            mpd_client.clone(),
            http.clone(),
            config.youtube.yt_dlp_bin.clone(),
            data_dir,
            config.mpd.music_directory.clone(),
        ));
        dispatcher.register(youtube as Arc<dyn MusicSource>);
    }

    // Disk-persistent across runs (sha256(uri) → bytes under cache_dir/art/),
    // 500-entry decoded LRU keeps recently-rendered icons hot in RAM so
    // re-scrolling doesn't pay disk-read + decode again. Bumped from 100
    // because per-row art now spans more sources (Spotify thumbs, full,
    // local album covers, lsinfo files) and the old cap thrashed.
    let art = Arc::new(ArtCache::new(art_dir(&cache_dir), 8, 500));

    let term = Term::probe(ThumbMode::from_config(&config.ui.thumb_mode)).unwrap_or(Term {
        picker: ratatui_image::picker::Picker::halfblocks(),
        mode: ThumbMode::Off,
        kitty_capable: false,
    });

    let thumb_cells = config.ui.thumb_cells;
    let keymap = crate::keys::Keymap::from_config(&config.keybindings);
    let base_theme = crate::theme::Theme::from_config(&config.theme);
    let hooks = config.hooks.clone();
    crate::hooks::on_startup(&hooks);
    let tab_alignment = crate::config::TabAlignment::from_str(&config.ui.tab_alignment);
    let modes = available_modes(&dispatcher);
    let active_source = modes
        .first()
        .copied()
        .unwrap_or(crate::types::SourceMode::Local);
    let tabs = tabs_for_mode(active_source);
    let theme = base_theme.clone().with_source_accent(active_source);
    // Build the T-cycle list: every configured entry parsed into a ThumbMode,
    // de-duped, with the startup mode appended if missing so we always start
    // on a member of the cycle. Empty config falls back to kitty/off.
    let startup_mode = ThumbMode::from_config(&config.ui.thumb_mode);
    let mut thumb_cycle: Vec<ThumbMode> = config
        .ui
        .thumb_cycle
        .iter()
        .map(|s| ThumbMode::from_config(s))
        .collect();
    if thumb_cycle.is_empty() {
        thumb_cycle = vec![ThumbMode::Kitty, ThumbMode::Off];
    }
    if !thumb_cycle.contains(&startup_mode) {
        thumb_cycle.insert(0, startup_mode);
    }
    let (mut app, wake_rx) = App::new(
        local,
        dispatcher,
        art,
        term,
        thumb_cells,
        keymap,
        theme,
        base_theme,
        hooks,
        tabs,
        tab_alignment,
        active_source,
        modes,
        thumb_cycle,
    );

    // Restore art-collapsed state from disk. Falls back to the config
    // default when the state file is missing or unparseable, so the first
    // run honors `[ui] art_collapsed`.
    let data_dir = config.data_dir();
    let state_path = app_state::state_path(&data_dir);
    let persisted = if state_path.exists() {
        app_state::AppState::load(&state_path)
    } else {
        app_state::AppState {
            art_collapsed: config.ui.art_collapsed,
            pinned: Vec::new(),
        }
    };
    app.art_collapsed = persisted.art_collapsed;
    app.pinned = persisted.pinned.into_iter().collect();
    app.state_path = Some(state_path);

    if let Some(msg) = spotify_status {
        app.status = Some(msg);
        app.status_set_at = Some(std::time::Instant::now());
        app.dirty = true;
    }

    let mpris = match prebuilt_mpris {
        Some(h) => Some(h),
        None => match mpris::spawn() {
            Ok(h) => Some(h),
            Err(e) => {
                tracing::warn!("mpris spawn failed: {e}; media keys disabled");
                None
            }
        },
    };

    app::run(config, conn.events, app, wake_rx, spotify_event_rx, mpris).await
}

/// Modes registered with the dispatcher, in canonical cycle order. `t` cycles
/// through this list; an unconfigured source is skipped.
pub fn available_modes(dispatcher: &Dispatcher) -> Vec<crate::types::SourceMode> {
    use crate::types::SourceMode;
    SourceMode::cycle_order()
        .iter()
        .copied()
        .filter(|m| dispatcher.get(m.scheme()).is_some())
        .collect()
}

/// Tab list for a given source mode. Replaces the prior config-driven
/// `resolve_tabs` — when the user toggles `t`, the entire tab bar swaps to
/// the new mode's set. `[ui] tabs` is no longer honored.
pub fn tabs_for_mode(mode: crate::types::SourceMode) -> Vec<crate::types::Category> {
    use crate::types::{Category, SourceMode};
    match mode {
        SourceMode::Local => vec![
            Category::Directories,
            Category::Albums,
            Category::Playlists,
            Category::Queue,
            Category::Search,
        ],
        SourceMode::Spotify => vec![
            Category::Spotify,
            Category::Albums,
            Category::Artists,
            Category::Playlists,
            Category::Podcasts,
            Category::Queue,
            Category::Search,
        ],
        SourceMode::SomaFm | SourceMode::Radio => {
            // Search tab dropped: ~30 SomaFM channels + a handful of user
            // radio entries don't merit a search box.
            vec![Category::Stations, Category::Queue]
        }
        SourceMode::YouTube => vec![Category::YouTube, Category::Queue, Category::Search],
    }
}

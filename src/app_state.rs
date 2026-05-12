//! Minimal cross-run state file. Currently tracks just whether the now-
//! playing art is collapsed; expand as more bits of UI need to survive
//! restarts.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AppState {
    #[serde(default)]
    pub art_collapsed: bool,
    /// URIs the user has pinned. Sort routines surface these to the top
    /// of every browse view regardless of the active axis.
    #[serde(default)]
    pub pinned: Vec<String>,
}

impl AppState {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(s) = serde_json::to_string(self) {
            let _ = std::fs::write(path, s);
        }
    }
}

/// Canonical path for the state file: `$XDG_DATA_HOME/fuga/state.json`.
pub fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join("state.json")
}

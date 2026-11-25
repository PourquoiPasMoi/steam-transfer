//! Remembered choices (destination drive, restore source/target), stored as JSON in the app config dir.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Where backups go (drive or folder; "Steam Transfer" is added automatically).
    pub destination: Option<String>,
    /// Binder to restore from.
    pub restore_source: Option<String>,
    /// Steam library to restore into.
    pub restore_target: Option<String>,
    /// Last libraries used on the Move tab.
    pub move_from: Option<String>,
    pub move_to: Option<String>,
    /// Where torrent downloads are saved.
    pub download_dir: Option<String>,
    /// Torrent speed limits in KB/s (0 or missing = unlimited).
    pub torrent_down_kbps: Option<u32>,
    pub torrent_up_kbps: Option<u32>,
    /// Check GitHub for a new version at startup.
    pub auto_update_check: Option<bool>,
}

fn path(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_config_dir().ok().map(|d| d.join("settings.json"))
}

pub fn load(app: &AppHandle) -> Settings {
    path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save(app: &AppHandle, settings: &Settings) -> Result<(), String> {
    let p = path(app).ok_or("No config folder")?;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(p, serde_json::to_string_pretty(settings).unwrap_or_default()).map_err(|e| e.to_string())
}

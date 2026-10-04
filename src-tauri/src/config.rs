//! App-level configuration persisted at `<app_data_dir>/config.json`.
//!
//! A small, human-readable JSON file for app-wide UI state that isn't tied to
//! any plugin or connection (e.g. whether the first-run plugin install step has
//! been shown). Mirrors the frontend `AppConfig` (camelCase on the wire).

use std::fs;
use std::path::Path;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

/// App-wide UI configuration. `#[serde(default)]` means a config written by an
/// older version (missing newer fields) still loads, falling back to defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppConfig {
    /// Whether the first-run plugin install step has been shown to the user.
    pub plugins_dialog_shown: bool,
    /// GitHub repo (`owner/name`) the in-app plugin installer fetches from.
    /// Defaults to this project's own repo so installing the bundled plugins
    /// needs no repo/tag entry.
    pub plugin_repo: String,
    /// Sidebar width in CSS pixels, set by dragging the sidebar's resize handle.
    pub sidebar_width: u32,
    /// When true, the sidebar collapses to a narrow rail and expands on hover.
    pub sidebar_collapsible: bool,
    /// Active UI theme id (matches a `[data-theme]` block in `styles.css` and an
    /// entry in the frontend `THEMES` list). Defaults to the built-in dark theme.
    pub theme: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            plugins_dialog_shown: false,
            plugin_repo: "somyatripathi7/polyclient".to_string(),
            sidebar_width: 240,
            sidebar_collapsible: false,
            theme: "mocha".to_string(),
        }
    }
}

/// `<app_data_dir>/config.json`.
fn config_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("cannot resolve app data dir: {e}"))?;
    Ok(dir.join("config.json"))
}

fn write_config(path: &Path, config: &AppConfig) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_vec_pretty(config).map_err(|e| e.to_string())?;
    fs::write(path, json).map_err(|e| e.to_string())
}

/// Load the app config, creating `config.json` with defaults on first run
/// (`pluginsDialogShown = false`).
#[tauri::command]
pub fn load_config(app: AppHandle) -> Result<AppConfig, String> {
    let path = config_path(&app)?;
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let cfg = AppConfig::default();
            write_config(&path, &cfg)?;
            Ok(cfg)
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Persist the full app config.
#[tauri::command]
pub fn save_config(app: AppHandle, config: AppConfig) -> Result<(), String> {
    let path = config_path(&app)?;
    write_config(&path, &config)
}

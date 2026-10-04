//! Persistence for saved connection profiles.
//!
//! Profiles are stored as human-readable JSON, grouped by the plugin that owns
//! them: `<app_data_dir>/connections/<plugin_id>/connections.json`. The files
//! survive restarts and can be inspected. NOTE: a credential field is stored as
//! a `SecretField` (`{"type":"PLAIN_TEXT","value":...}`); the `PLAIN_TEXT`
//! variant holds the secret in plaintext.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use polyclient_core::ConnectionConfig;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::plugin_manager::PluginManager;

/// A reusable connection the user has saved. Mirrors the frontend
/// `SavedConnection` (camelCase on the wire).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedConnection {
    pub id: String,
    pub name: String,
    pub plugin_id: String,
    pub config: ConnectionConfig,
    /// Sidebar sort position (ascending). Defaults to 0; only set when the user
    /// reorders the list via drag-and-drop.
    #[serde(default)]
    pub order: i64,
    /// Per-connection UI preferences (e.g. `treeWidth`, `editorHeight`), stored
    /// alongside the connection so they travel with it and vanish when the
    /// profile is deleted. A free-form map keeps it open to future prefs.
    #[serde(default)]
    pub settings: HashMap<String, serde_json::Value>,
}

/// `<app_data_dir>/connections` — the root holding one subdirectory per plugin.
fn connections_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("cannot resolve app data dir: {e}"))?;
    Ok(dir.join("connections"))
}

/// Reject plugin ids that could escape the connections root. Plugin ids are
/// used verbatim as directory names, so anything with a path separator or `..`
/// is refused rather than silently writing outside the intended tree.
fn validate_plugin_id(plugin_id: &str) -> Result<(), String> {
    if plugin_id.is_empty()
        || plugin_id == ".."
        || plugin_id.contains('/')
        || plugin_id.contains('\\')
    {
        return Err(format!("invalid plugin id: {plugin_id:?}"));
    }
    Ok(())
}

/// Load saved profiles from every `connections/<plugin_id>/connections.json`,
/// limited to plugins that are currently installed. Profiles whose owning
/// plugin is no longer installed are skipped (their files are left on disk).
/// Returns an empty list when nothing has been saved yet. Plugins are visited
/// in sorted order so the merged list is stable across loads.
#[tauri::command]
pub fn load_connections(
    app: AppHandle,
    manager: State<'_, Arc<PluginManager>>,
) -> Result<Vec<SavedConnection>, String> {
    let dir = connections_dir(&app)?;
    let installed: HashSet<String> = manager.list_plugins().into_iter().map(|p| p.id).collect();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };

    // Collect connection files for installed plugins only, sorted by plugin id
    // for deterministic order.
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| installed.contains(name))
        })
        .map(|p| p.join("connections.json"))
        .collect();
    files.sort();

    let mut out = Vec::new();
    for file in files {
        match fs::read(&file) {
            Ok(bytes) => {
                let conns: Vec<SavedConnection> =
                    serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                out.extend(conns);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(out)
}

/// Persist the full set of profiles, splitting them into one file per owning
/// plugin (`connections/<plugin_id>/connections.json`). Plugins whose profiles
/// were all removed have their file deleted so stale entries don't linger.
#[tauri::command]
pub fn save_connections(app: AppHandle, connections: Vec<SavedConnection>) -> Result<(), String> {
    let dir = connections_dir(&app)?;

    // Group incoming connections by their owning plugin (sorted by id).
    let mut groups: BTreeMap<String, Vec<SavedConnection>> = BTreeMap::new();
    for conn in connections {
        validate_plugin_id(&conn.plugin_id)?;
        groups.entry(conn.plugin_id.clone()).or_default().push(conn);
    }

    // Clear out files for plugins that no longer have any saved connections.
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).map(str::to_owned);
            if let Some(name) = name {
                if path.is_dir() && !groups.contains_key(&name) {
                    let file = path.join("connections.json");
                    match fs::remove_file(&file) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.to_string()),
                    }
                }
            }
        }
    }

    for (plugin_id, conns) in groups {
        let plugin_dir = dir.join(&plugin_id);
        fs::create_dir_all(&plugin_dir).map_err(|e| e.to_string())?;
        let json = serde_json::to_vec_pretty(&conns).map_err(|e| e.to_string())?;
        fs::write(plugin_dir.join("connections.json"), json).map_err(|e| e.to_string())?;
    }
    Ok(())
}

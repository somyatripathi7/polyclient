//! Persistence for saved connection profiles.
//!
//! Secret fields are stored in the platform credential store. JSON contains
//! opaque references; plaintext values are restored only in memory for plugins.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use polyclient_core::ConnectionConfig;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::plugin_manager::PluginManager;

// Serialize migration and saves so keychain and profile writes cannot interleave.
static PROFILE_LOCK: Mutex<()> = Mutex::new(());
const SERVICE: &str = "com.somyatripathi7.polyclient.connections";

fn credential(account: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, account)
        .map_err(|_| "Cannot access the OS credential store".to_owned())
}

fn reference(value: &serde_json::Value) -> Option<&str> {
    (value.get("type")?.as_str()? == "OS_KEYCHAIN")
        .then(|| value.get("value")?.as_str())
        .flatten()
}

fn protect(conns: &mut [SavedConnection]) -> Result<(), String> {
    let previous = conns.to_vec();
    let result = protect_with(conns, |account, payload| {
        credential(account)?.set_password(payload).map_err(|_| {
            "Cannot save connection configuration to the OS credential store".to_owned()
        })
    });
    if result.is_err() {
        remove_unused(conns, &previous);
    }
    result
}

fn protect_with(
    conns: &mut [SavedConnection],
    mut put: impl FnMut(&str, &str) -> Result<(), String>,
) -> Result<(), String> {
    for conn in conns {
        for value in conn.config.values_mut() {
            if reference(value).is_some() {
                continue;
            }
            // Protect every config value, including credentials embedded in URLs,
            // headers and plugin-specific nested maps. No schema heuristics needed.
            let payload = serde_json::to_string(value).map_err(|e| e.to_string())?;
            // Random, immutable entries neither expose a password hash nor
            // overwrite credentials still referenced by a previous saved file.
            let account = uuid::Uuid::new_v4().to_string();
            put(&account, &payload)?;
            *value = serde_json::json!({"type": "OS_KEYCHAIN", "value": account});
        }
    }
    Ok(())
}

fn restore(conns: &mut [SavedConnection]) -> Result<(), String> {
    restore_with(conns, |account| {
        credential(account)?.get_password().map_err(|_| "Cannot read saved connection configuration: unlock the OS credential store or restore the missing credential".to_owned())
    })
}

fn restore_with(
    conns: &mut [SavedConnection],
    mut get: impl FnMut(&str) -> Result<String, String>,
) -> Result<(), String> {
    for conn in conns {
        for value in conn.config.values_mut() {
            if let Some(account) = reference(value) {
                let payload = get(account)?;
                *value = serde_json::from_str(&payload)
                    .map_err(|_| "Invalid saved credential data".to_owned())?;
            }
        }
    }
    Ok(())
}

fn read_profiles(file: &std::path::Path) -> Result<Vec<SavedConnection>, String> {
    match fs::read(file) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.to_string()),
    }
}

fn references(conns: &[SavedConnection]) -> HashSet<String> {
    conns
        .iter()
        .flat_map(|c| c.config.values())
        .filter_map(reference)
        .map(str::to_owned)
        .collect()
}

fn remove_unused(old: &[SavedConnection], new: &[SavedConnection]) {
    for account in references(old).difference(&references(new)) {
        if let Ok(entry) = credential(account) {
            if let Err(error) = entry.delete_credential() {
                if !matches!(error, keyring::Error::NoEntry) {
                    tracing::warn!("Could not remove an unused connection credential");
                }
            }
        }
    }
}

fn write_profiles(file: &std::path::Path, conns: &[SavedConnection]) -> Result<(), String> {
    let old = read_profiles(file)?;
    let json = serde_json::to_vec_pretty(conns).map_err(|e| e.to_string())?;
    let temporary = file.with_extension("json.tmp");
    let result = fs::write(&temporary, json).and_then(|()| fs::rename(&temporary, file));
    if let Err(error) = result {
        remove_unused(conns, &old);
        let _ = fs::remove_file(&temporary);
        return Err(error.to_string());
    }
    // Delete old credentials only after their replacement is committed.
    remove_unused(&old, conns);
    Ok(())
}

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
        || plugin_id == "."
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
    let _guard = PROFILE_LOCK
        .lock()
        .map_err(|_| "Profile storage lock poisoned")?;
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
                let mut conns: Vec<SavedConnection> =
                    serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                let needs_migration = conns
                    .iter()
                    .any(|c| c.config.values().any(|v| reference(v).is_none()));
                if needs_migration {
                    protect(&mut conns)?;
                    write_profiles(&file, &conns)?;
                }
                restore(&mut conns)?;
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
pub fn save_connections(
    app: AppHandle,
    manager: State<'_, Arc<PluginManager>>,
    connections: Vec<SavedConnection>,
) -> Result<(), String> {
    let _guard = PROFILE_LOCK
        .lock()
        .map_err(|_| "Profile storage lock poisoned")?;
    let dir = connections_dir(&app)?;

    // Group incoming connections by their owning plugin (sorted by id).
    let mut groups: BTreeMap<String, Vec<SavedConnection>> = BTreeMap::new();
    for conn in connections {
        validate_plugin_id(&conn.plugin_id)?;
        groups.entry(conn.plugin_id.clone()).or_default().push(conn);
    }

    let installed: HashSet<_> = manager.list_plugins().into_iter().map(|p| p.id).collect();
    let retained: HashSet<_> = groups.keys().cloned().collect();
    for (plugin_id, mut conns) in groups {
        protect(&mut conns)?;
        let plugin_dir = dir.join(&plugin_id);
        fs::create_dir_all(&plugin_dir).map_err(|e| e.to_string())?;
        write_profiles(&plugin_dir.join("connections.json"), &conns)?;
    }
    // Clear out files for plugins that no longer have any saved connections.
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).map(str::to_owned);
            if let Some(name) = name {
                if path.is_dir() && installed.contains(&name) && !retained.contains(&name) {
                    let file = path.join("connections.json");
                    let old = read_profiles(&file)?;
                    match fs::remove_file(&file) {
                        Ok(()) => remove_unused(&old, &[]),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.to_string()),
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profiles() -> Vec<SavedConnection> {
        serde_json::from_value(serde_json::json!([{
            "id":"profile", "name":"Test", "pluginId":"postgres",
            "config": {
                "password":{"type":"PLAIN_TEXT","value":"test-password"},
                "url":"postgres://user:embedded-password@localhost/db",
                "headers":{"Authorization":"Bearer test-token"},
                "port":5432, "ssl":true
            }
        }]))
        .unwrap()
    }

    #[test]
    fn migrates_all_config_values_and_restores_wire_types() {
        let mut conns = profiles();
        let original = serde_json::to_value(&conns).unwrap();
        let mut vault = HashMap::new();
        protect_with(&mut conns, |key, value| {
            vault.insert(key.to_owned(), value.to_owned());
            Ok(())
        })
        .unwrap();
        let dir = std::env::temp_dir().join(format!("polyclient-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("connections.json");
        fs::write(&file, serde_json::to_vec(&original).unwrap()).unwrap();
        write_profiles(&file, &conns).unwrap();
        let disk = fs::read_to_string(&file).unwrap();
        conns = read_profiles(&file).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        for secret in ["test-password", "embedded-password", "test-token"] {
            assert!(!disk.contains(secret));
        }
        assert_eq!(references(&conns).len(), 5);
        protect_with(&mut conns, |_, _| panic!("migration must be idempotent")).unwrap();
        restore_with(&mut conns, |key| Ok(vault[key].clone())).unwrap();
        assert_eq!(serde_json::to_value(&conns).unwrap(), original);
    }

    #[test]
    fn unavailable_store_fails_without_plaintext_fallback() {
        let mut conns = profiles();
        assert!(protect_with(&mut conns, |_, _| Err("locked".into())).is_err());
        protect_with(&mut conns, |_, _| Ok(())).unwrap();
        assert!(restore_with(&mut conns, |_| Err("missing".into())).is_err());
    }
}

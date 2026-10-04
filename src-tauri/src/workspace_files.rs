//! Per-connection workspace files.
//!
//! Each saved profile gets its own folder at
//! `<app_data_dir>/workspace/<connection_id>/`, where `connection_id` is the
//! stable saved-profile id (not the per-session live connection id). A workspace
//! stores an arbitrary directory tree there via the generic path primitives
//! below; the host stays oblivious to what the files mean. Examples: the RDBMS
//! workspace saves `<name>.sql` snippets at the root, the SSH/CLI workspace
//! `<name>.sh` scripts, and the curl workspace a nested `curlui/` tree. Files are
//! human-readable and can be inspected or edited outside the app.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

/// `<app_data_dir>/workspace/<connection_id>` — the folder of files for one profile.
fn workspace_dir(app: &AppHandle, connection_id: &str) -> Result<PathBuf, String> {
    validate_segment(connection_id, "connection id")?;
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("cannot resolve app data dir: {e}"))?;
    Ok(dir.join("workspace").join(connection_id))
}

/// Reject ids/segments that could escape the intended folder; they're used
/// verbatim as path segments.
fn validate_segment(value: &str, what: &str) -> Result<(), String> {
    if value.is_empty() || value == ".." || value.contains('/') || value.contains('\\') {
        return Err(format!("invalid {what}: {value:?}"));
    }
    Ok(())
}

// --- Generic per-connection path operations --------------------------------
//
// Lower-level primitives that let a workspace store an arbitrary directory tree
// under `workspace/<connection_id>/`. Unlike the flat name+ext helpers above,
// these take a `/`-separated relative path so a workspace (e.g. the curl client)
// can build nested layouts. Every path segment is validated to block traversal;
// the host stays oblivious to what the files mean.

/// One immediate child of a workspace directory. Mirrors the frontend `DirEntry`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
}

/// Resolve `workspace/<connection_id>/<path>`, validating the connection id and
/// every `/`-separated segment of `path`. An empty `path` resolves to the
/// connection root. Rejects `.`/`..`/separators in any segment.
fn resolve(app: &AppHandle, connection_id: &str, path: &str) -> Result<PathBuf, String> {
    let mut dir = workspace_dir(app, connection_id)?;
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        if seg == "." || seg == ".." {
            return Err(format!("invalid path segment: {seg:?}"));
        }
        validate_segment(seg, "path segment")?;
        dir.push(seg);
    }
    Ok(dir)
}

/// Read a single file at `path` under `connection_id`. Returns `None` when the
/// file doesn't exist (so callers can distinguish missing from empty).
#[tauri::command]
pub fn read_workspace_file(
    app: AppHandle,
    connection_id: String,
    path: String,
) -> Result<Option<String>, String> {
    let file = resolve(&app, &connection_id, &path)?;
    match fs::read_to_string(&file) {
        Ok(content) => Ok(Some(content)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Write `content` to `path` under `connection_id`, creating parent dirs.
#[tauri::command]
pub fn write_workspace_file_at(
    app: AppHandle,
    connection_id: String,
    path: String,
    content: String,
) -> Result<(), String> {
    let file = resolve(&app, &connection_id, &path)?;
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(&file, content).map_err(|e| e.to_string())
}

/// List the immediate children of the directory at `path` under `connection_id`,
/// sorted by name. The caller recurses into subdirs itself. Returns an empty
/// list when the directory doesn't exist.
#[tauri::command]
pub fn list_workspace_dir(
    app: AppHandle,
    connection_id: String,
    path: String,
) -> Result<Vec<DirEntry>, String> {
    let dir = resolve(&app, &connection_id, &path)?;
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        out.push(DirEntry { name, is_dir });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Delete the file or directory (recursively) at `path` under `connection_id`.
/// A missing path is treated as success.
#[tauri::command]
pub fn delete_workspace_path(
    app: AppHandle,
    connection_id: String,
    path: String,
) -> Result<(), String> {
    let target = resolve(&app, &connection_id, &path)?;
    let meta = match fs::symlink_metadata(&target) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
    };
    let result = if meta.is_dir() {
        fs::remove_dir_all(&target)
    } else {
        fs::remove_file(&target)
    };
    match result {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

//! `#[tauri::command]` bridge between the frontend and the [`PluginManager`].
//!
//! Five generic commands cover the whole surface: the host is a pure pipe and
//! knows nothing about SQL/documents/queues. `plugin_call` forwards an opaque
//! `op` + `params` to the plugin that owns the connection.

use std::sync::Arc;

use polyclient_core::{ConnectionConfig, ConnectionId, PluginInfo};
use serde_json::Value;
use tauri::State;

use crate::plugin_manager::{AvailablePlugin, GithubPreview, PluginManager};
use crate::pty::PtyManager;

/// Shared manager handle managed by Tauri.
type Manager<'a> = State<'a, Arc<PluginManager>>;

/// Convert any displayable error into the string the frontend receives.
fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

#[tauri::command]
pub fn list_plugins(manager: Manager<'_>) -> Vec<PluginInfo> {
    manager.list_plugins()
}

/// The release channel this app build tracks (`"nightly"` or `"stable"`). Gates
/// the installer's plugin channel and drives the nightly logo badge.
#[tauri::command]
pub fn app_channel() -> String {
    crate::release_channel().to_string()
}

#[tauri::command]
pub async fn test_connection(
    manager: Manager<'_>,
    plugin_id: String,
    config: ConnectionConfig,
) -> Result<(), String> {
    manager
        .test_connection(&plugin_id, config)
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn open_connection(
    manager: Manager<'_>,
    plugin_id: String,
    config: ConnectionConfig,
) -> Result<ConnectionId, String> {
    manager
        .open_connection(&plugin_id, config)
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn close_connection(
    manager: Manager<'_>,
    connection_id: ConnectionId,
) -> Result<(), String> {
    manager.close_connection(connection_id).await.map_err(err)
}

/// Forward an opaque capability call to the plugin owning `connection_id`.
/// `op` is a plugin-defined string (e.g. `"rdbms.execute"`); `params` is passed
/// through untouched.
#[tauri::command]
pub async fn plugin_call(
    manager: Manager<'_>,
    connection_id: ConnectionId,
    op: String,
    params: Value,
) -> Result<Value, String> {
    manager
        .plugin_call(connection_id, op, params)
        .await
        .map_err(err)
}

/// Cancel the in-flight `plugin_call` for `connection_id`, if any.
#[tauri::command]
pub async fn cancel_last_plugin_call(
    manager: Manager<'_>,
    connection_id: ConnectionId,
) -> Result<(), String> {
    manager.cancel(connection_id).await.map_err(err)
}

/// List the plugins installable from `repo` (`owner/name`): the rolling
/// `plugins-latest` release's assets with a binary for this platform, one entry
/// per plugin. No download.
#[tauri::command]
pub async fn list_github_plugins(
    manager: Manager<'_>,
    repo: String,
) -> Result<Vec<AvailablePlugin>, String> {
    manager.list_github_plugins(&repo).await
}

/// Resolve a GitHub release and report the asset + checksum that would be
/// installed, without downloading the binary or executing anything.
/// `plugin_id` disambiguates when a single release publishes several plugins.
#[tauri::command]
pub async fn preview_github_plugin(
    manager: Manager<'_>,
    repo: String,
    tag: Option<String>,
    plugin_id: Option<String>,
) -> Result<GithubPreview, String> {
    manager
        .preview_github(&repo, tag, plugin_id.as_deref())
        .await
}

/// Download, checksum-verify, and install the plugin from the previewed release.
/// `expected_sha` is the checksum the user confirmed (`None` = no published
/// checksum, install anyway). Returns the installed plugin's info.
#[tauri::command]
pub async fn install_github_plugin(
    manager: Manager<'_>,
    repo: String,
    tag: String,
    plugin_id: String,
    expected_sha: Option<String>,
) -> Result<PluginInfo, String> {
    manager
        .install_github(&repo, &tag, Some(&plugin_id), expected_sha)
        .await
}

/// Uninstall a plugin: stop its process and delete its manifest + executable
/// from the plugins dir. Fails if the plugin has open connections.
#[tauri::command]
pub async fn uninstall_plugin(manager: Manager<'_>, plugin_id: String) -> Result<(), String> {
    manager.uninstall(&plugin_id).await
}

// ---------------------------------------------------------------------------
// PTY commands (CLI / SSH workspace)
// ---------------------------------------------------------------------------

type Pty<'a> = State<'a, Arc<PtyManager>>;

/// Spawn the CLI plugin's terminal process in a PTY for a connection, keyed by
/// `terminal_id` (one connection may own several terminal tabs). The host asks
/// the owning plugin (via the `cli.spawn_spec` op) how to launch it, then runs
/// that command in a PTY and forwards output as Tauri events on
/// `pty://output/<terminal_id>`. All backend-specific command knowledge lives in
/// the plugin, so the host stays generic.
#[tauri::command]
pub async fn pty_spawn(
    pty: Pty<'_>,
    manager: Manager<'_>,
    app: tauri::AppHandle,
    connection_id: ConnectionId,
    terminal_id: String,
) -> Result<(), String> {
    // Already running? (idempotent — e.g. React StrictMode double-mount.)
    if crate::pty::is_alive(pty.inner().clone(), terminal_id.clone()).await {
        tracing::debug!("pty_spawn: terminal {terminal_id} already alive (no-op)");
        return Ok(());
    }
    // Ask the plugin that owns this connection for its spawn spec.
    let spec_value = manager
        .plugin_call(connection_id, "cli.spawn_spec".into(), Value::Null)
        .await
        .map_err(err)?;
    let spec: polyclient_core::PtySpawnSpec =
        serde_json::from_value(spec_value).map_err(|e| e.to_string())?;
    tracing::info!(
        "pty_spawn: terminal {terminal_id} -> '{}' ({} args)",
        spec.program,
        spec.args.len()
    );
    crate::pty::spawn(pty.inner().clone(), app, connection_id, terminal_id, spec).await
}

/// Write bytes to a terminal's PTY (keystrokes / paste from the terminal).
#[tauri::command]
pub async fn pty_write(pty: Pty<'_>, terminal_id: String, data: Vec<u8>) -> Result<(), String> {
    crate::pty::write(pty.inner().clone(), terminal_id, data).await
}

/// Notify a terminal's PTY of a resize.
#[tauri::command]
pub async fn pty_resize(
    pty: Pty<'_>,
    terminal_id: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    crate::pty::resize(pty.inner().clone(), terminal_id, cols, rows).await
}

/// Close and drop a single terminal's PTY.
#[tauri::command]
pub async fn pty_close(pty: Pty<'_>, terminal_id: String) -> Result<(), String> {
    crate::pty::close(pty.inner().clone(), terminal_id).await
}

/// Close and drop every terminal PTY owned by a connection (explicit teardown
/// on disconnect / delete).
#[tauri::command]
pub async fn pty_close_connection(pty: Pty<'_>, connection_id: ConnectionId) -> Result<(), String> {
    crate::pty::close_connection(pty.inner().clone(), connection_id).await
}

/// Retained scrollback (recent output) for a terminal's PTY, so a freshly
/// (re)mounted terminal can repaint its history. Empty if no live PTY.
#[tauri::command]
pub async fn pty_snapshot(pty: Pty<'_>, terminal_id: String) -> Result<Vec<u8>, String> {
    crate::pty::snapshot(pty.inner().clone(), terminal_id).await
}

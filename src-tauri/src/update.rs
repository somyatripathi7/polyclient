//! App self-update commands backed by `tauri-plugin-updater`.
//!
//! The updater is built per-call so its endpoint can follow this build's
//! release channel (`crate::release_channel`): nightly tracks the rolling
//! `latest` prerelease, stable tracks the latest `vX.Y.Z` release. The signed
//! `latest.json` is verified against the public key in `tauri.conf.json`.

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tauri_plugin_updater::{Updater, UpdaterExt};

/// Update metadata surfaced to the frontend (no live handle crosses the bridge).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    /// The available version.
    pub version: String,
    /// The version currently running.
    pub current_version: String,
    /// Release notes, if the manifest provides them.
    pub notes: Option<String>,
}

/// Build an updater pointed at this channel's `latest.json` endpoint. Inherits
/// the pubkey from `tauri.conf.json`; only the endpoint is overridden.
fn updater(app: &AppHandle) -> Result<Updater, String> {
    let endpoint = crate::updater_endpoint(crate::release_channel(), crate::REPO);
    let url: url::Url = endpoint
        .parse()
        .map_err(|e| format!("invalid updater endpoint {endpoint}: {e}"))?;
    app.updater_builder()
        .endpoints(vec![url])
        .map_err(|e| e.to_string())?
        .build()
        .map_err(|e| e.to_string())
}

/// Check the release channel for a newer signed build. `Ok(None)` means the app
/// is up to date; errors (e.g. offline, or running under `tauri dev` where the
/// updater is inert) are returned as strings for the UI to swallow.
#[tauri::command]
pub async fn check_update(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    match updater(&app)?.check().await {
        Ok(Some(update)) => Ok(Some(UpdateInfo {
            version: update.version.clone(),
            current_version: update.current_version.clone(),
            notes: update.body.clone(),
        })),
        Ok(None) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Download, verify, and install the available update, emitting
/// `update://progress` events (`{ chunk, total }`) as bytes arrive. On success
/// the caller should relaunch (via `@tauri-apps/plugin-process`).
#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<(), String> {
    let update = updater(&app)?
        .check()
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "no update available".to_string())?;

    let progress_app = app.clone();
    update
        .download_and_install(
            move |chunk, total| {
                let _ = progress_app.emit(
                    "update://progress",
                    serde_json::json!({ "chunk": chunk, "total": total }),
                );
            },
            || {},
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

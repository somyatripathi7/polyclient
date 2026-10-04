//! Tauri host for the polyclient desktop client.
//!
//! Ships with only `polyclient-core` (no DB drivers). At startup it discovers
//! out-of-process plugins from the plugins directory and exposes them to the
//! frontend through five generic `#[tauri::command]`s (see [`commands`]).
//!
//! Plugins dir: `$POLYCLIENT_PLUGINS_DIR` if set, else `<app-data>/plugins`.

mod commands;
mod config;
mod github;
mod logging;
mod persistence;
mod plugin_manager;
mod pty;
mod update;
mod workspace_files;

use std::path::PathBuf;
use std::sync::Arc;

use plugin_manager::PluginManager;
use tauri::Manager;

/// Holds the file-appender's `WorkerGuard` (release builds only) in Tauri's
/// managed state so it lives for the process lifetime; dropping it flushes and
/// stops the background log writer. `None` in dev builds (console logging).
#[allow(dead_code)]
struct LogGuard(std::sync::Mutex<Option<tracing_appender::non_blocking::WorkerGuard>>);

/// The release channel this app binary was built for. Stamped at build time by
/// `publish-app.yml` via `POLYCLIENT_RELEASE_CHANNEL`; defaults to `nightly` for
/// local/dev builds so they track nightly plugin releases. Gates which plugin
/// channel the in-app installer offers and drives the nightly logo badge.
pub fn release_channel() -> &'static str {
    option_env!("POLYCLIENT_RELEASE_CHANNEL").unwrap_or("nightly")
}

/// GitHub repo (`owner/name`) the app self-updates from.
pub(crate) const REPO: &str = "somyatripathi7/polyclient";

/// The updater manifest (`latest.json`) URL for a given `channel`. Nightly
/// tracks the rolling `latest` prerelease; stable tracks GitHub's "latest"
/// non-prerelease (the app's `vX.Y.Z`, since plugin releases use
/// `make_latest:false`).
pub(crate) fn updater_endpoint(channel: &str, repo: &str) -> String {
    if channel == "stable" {
        format!("https://github.com/{repo}/releases/latest/download/latest.json")
    } else {
        format!("https://github.com/{repo}/releases/download/latest/latest.json")
    }
}

/// Resolve the directory to scan for plugin manifests.
fn plugins_dir(app: &tauri::App) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Ok(dir) = std::env::var("POLYCLIENT_PLUGINS_DIR") {
        return Ok(PathBuf::from(dir));
    }
    Ok(app.path().app_data_dir()?.join("plugins"))
}

pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            // Initialize logging first. Dev builds log to the console; release
            // builds log to `<app-data-dir>/logs/polyclient-app.log`. The file
            // appender's guard must live for the process lifetime, so stash it
            // in managed state.
            let logs_dir = app
                .path()
                .app_data_dir()
                .map(|d| logging::logs_dir(&d))
                .ok();
            let guard = logging::init(logs_dir.as_deref().unwrap_or(std::path::Path::new(".")));
            app.manage(LogGuard(std::sync::Mutex::new(guard)));

            #[cfg(target_os = "macos")]
            disable_macos_text_substitutions();

            // In release builds, plugin stderr is captured to per-plugin log
            // files in the same logs dir; in dev it's inherited (console).
            let plugin_logs = if logging::logs_to_file() {
                logs_dir
            } else {
                None
            };
            let dir = plugins_dir(app)?;
            tracing::info!("discovering plugins in {}", dir.display());
            let manager = Arc::new(PluginManager::discover(&dir, plugin_logs));
            app.manage(manager);
            app.manage(Arc::new(pty::PtyManager::new()));

            // Self-update: register the updater + process plugins. The
            // channel-aware endpoint is applied per-call in `update::*` via
            // `updater_builder()` (the init Builder has no `.endpoints`).
            app.handle().plugin(tauri_plugin_shell::init())?;
            app.handle().plugin(tauri_plugin_dialog::init())?;
            // The `curlui` workspace sends HTTP requests directly from the
            // frontend via this plugin; `fs` reads local files for multipart
            // uploads. Allowed URLs / read scope are set in capabilities.
            app.handle().plugin(tauri_plugin_http::init())?;
            app.handle().plugin(tauri_plugin_fs::init())?;

            #[cfg(desktop)]
            {
                app.handle().plugin(tauri_plugin_process::init())?;
                app.handle()
                    .plugin(tauri_plugin_updater::Builder::new().build())?;
            }
            // #[cfg(debug_assertions)] // only include this code on debug builds
            // {
            //     let window = app.get_webview_window("main").unwrap();
            //     window.open_devtools();
            //     window.close_devtools();
            // }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_plugins,
            commands::test_connection,
            commands::open_connection,
            commands::close_connection,
            commands::plugin_call,
            commands::cancel_last_plugin_call,
            commands::list_github_plugins,
            commands::preview_github_plugin,
            commands::install_github_plugin,
            commands::uninstall_plugin,
            commands::app_channel,
            update::check_update,
            update::install_update,
            persistence::load_connections,
            persistence::save_connections,
            config::load_config,
            config::save_config,
            workspace_files::read_workspace_file,
            workspace_files::write_workspace_file_at,
            workspace_files::list_workspace_dir,
            workspace_files::delete_workspace_path,
            commands::pty_spawn,
            commands::pty_write,
            commands::pty_resize,
            commands::pty_close,
            commands::pty_close_connection,
            commands::pty_snapshot,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// macOS applies system "smart" text substitutions — first-letter
/// capitalization, smart quotes/dashes — inside WebView text fields. That
/// silently corrupts typed input (JSON, SQL, hostnames, passwords), so disable
/// them in this app's user-defaults domain. The HTML `autocapitalize`/
/// `autocorrect` attributes don't control this on desktop WKWebView.
#[cfg(target_os = "macos")]
fn disable_macos_text_substitutions() {
    use objc2::msg_send;
    use objc2_foundation::{NSString, NSUserDefaults};

    let keys = [
        "NSAutomaticCapitalizationEnabled",
        "NSAutomaticQuoteSubstitutionEnabled",
        "NSAutomaticDashSubstitutionEnabled",
        "NSAutomaticPeriodSubstitutionEnabled",
        "NSAutomaticTextReplacementEnabled",
        "NSAutomaticSpellingCorrectionEnabled",
    ];
    unsafe {
        let defaults = NSUserDefaults::standardUserDefaults();
        for key in keys {
            let k = NSString::from_str(key);
            let _: () = msg_send![&*defaults, setBool: false, forKey: &*k];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updater_endpoint_is_channel_specific() {
        let repo = "somyatripathi7/polyclient";
        assert_eq!(
            updater_endpoint("stable", repo),
            "https://github.com/somyatripathi7/polyclient/releases/latest/download/latest.json"
        );
        assert_eq!(
            updater_endpoint("nightly", repo),
            "https://github.com/somyatripathi7/polyclient/releases/download/latest/latest.json"
        );
        // Unknown channels default to the nightly endpoint.
        assert_eq!(
            updater_endpoint("", repo),
            updater_endpoint("nightly", repo)
        );
    }
}

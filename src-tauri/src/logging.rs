//! Application logging.
//!
//! - **Dev builds** (`debug_assertions`): log to the console (stderr), matching
//!   `npm run tauri dev` expectations. No log files are written.
//! - **Release builds**: log to `<app-data-dir>/logs/polyclient-app.log` (daily-rotated).
//!
//! Plugin processes log to their own stderr; in release builds the
//! [`crate::plugin_manager`] captures that and writes it to
//! `<app-data-dir>/logs/plugin-<id>.log`. In dev builds plugin stderr is
//! inherited so it shows up on the console.

use std::path::{Path, PathBuf};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// The directory plugin + app logs are written to in release builds:
/// `<app-data-dir>/logs`.
pub fn logs_dir(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("logs")
}

/// Whether this build writes logs to files (release) vs the console (dev).
pub const fn logs_to_file() -> bool {
    !cfg!(debug_assertions)
}

fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

/// Initialize the global tracing subscriber.
///
/// In dev builds this installs a console (stderr) subscriber and returns `None`.
/// In release builds it installs a daily-rotated file subscriber at
/// `<logs_dir>/polyclient-app.log` and returns the appender's [`WorkerGuard`], which
/// MUST be kept alive for the process lifetime (dropping it flushes and stops
/// the background writer). If the log file can't be opened, it falls back to a
/// console subscriber and returns `None`.
#[must_use]
pub fn init(logs_dir: &Path) -> Option<WorkerGuard> {
    if !logs_to_file() {
        // Dev: console only.
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(env_filter())
            .finish()
            .init();
        return None;
    }

    // Release: file only.
    match std::fs::create_dir_all(logs_dir) {
        Ok(()) => {
            let appender = tracing_appender::rolling::daily(logs_dir, "polyclient-app.log");
            let (writer, guard) = tracing_appender::non_blocking(appender);
            tracing_subscriber::fmt()
                .with_writer(writer)
                .with_ansi(false)
                .with_env_filter(env_filter())
                .finish()
                .init();
            tracing::info!("logging to {}", logs_dir.display());
            Some(guard)
        }
        Err(e) => {
            // Couldn't create the logs dir; fall back to console so logs aren't lost.
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(env_filter())
                .finish()
                .init();
            tracing::warn!(
                "could not create logs dir {}: {e}; logging to stderr",
                logs_dir.display()
            );
            None
        }
    }
}

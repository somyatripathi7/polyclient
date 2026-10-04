//! Out-of-process plugin manager: discovery, lazy process spawning, request
//! multiplexing, and connection routing.
//!
//! The host knows nothing about SQL/documents/queues. It discovers plugins from
//! `*.plugin.json` manifests, spawns each plugin's executable lazily on first
//! use, and routes opaque JSON `call`s to the plugin that owns a given
//! [`ConnectionId`]. A plugin process stays warm (holding its DB pools) until
//! the app exits.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use polyclient_core::protocol::{Request, Response, RpcError, PROTOCOL_VERSION};
use polyclient_core::{ConnectionConfig, ConnectionId, PluginError, PluginInfo};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::github;

type RpcResult = std::result::Result<Value, RpcError>;
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<RpcResult>>>>;

/// On-disk plugin manifest (`<id>.plugin.json`). `executable` may be relative,
/// in which case it resolves against the manifest's directory.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    plugin_info: PluginInfo,
    executable: String,
    /// Provenance of an in-app GitHub install, used for update detection.
    /// Absent for manually-placed manifests (e.g. `npm run plugins:dev`).
    #[serde(default)]
    source: Option<InstallSource>,
}

/// Where an in-app-installed plugin came from, recorded so the installer can
/// tell whether a newer release exists. All plugins share one release tag, so
/// the tag does not identify the plugin and is not used for staleness: for
/// `stable` we compare `version`, for `nightly` (whose version is a commit
/// count) we compare the binary asset's `updated_at` — the release's own
/// `published_at` is frozen because `plugins-latest` is updated in place.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstallSource {
    repo: String,
    tag: String,
    /// `"nightly"` or `"stable"`.
    channel: String,
    /// The installed plugin's version at install time.
    version: String,
    /// The release's publish timestamp (ISO-8601 UTC). Frozen for the rolling
    /// `plugins-latest` tag; kept as a fallback for plugins installed before
    /// `asset_updated_at` was recorded.
    #[serde(default)]
    published_at: Option<String>,
    /// The installed binary asset's `updated_at` (ISO-8601 UTC); GitHub bumps
    /// it on re-upload, so it drives nightly update detection.
    #[serde(default)]
    asset_updated_at: Option<String>,
}

/// A discovered plugin: its advertised info plus the resolved executable path.
#[derive(Debug)]
struct DiscoveredPlugin {
    info: PluginInfo,
    executable: PathBuf,
    source: Option<InstallSource>,
}

/// A running plugin process plus the plumbing to talk to it. One per plugin id;
/// holds many connections.
struct PluginProcess {
    stdin_tx: mpsc::UnboundedSender<String>,
    pending: Pending,
    next_id: AtomicU64,
    alive: Arc<AtomicBool>,
    /// Kept alive so the child isn't reaped; dropped (killed) on manager drop.
    _child: Child,
}

impl PluginProcess {
    fn spawn(executable: &Path, log_path: Option<PathBuf>) -> std::result::Result<Self, String> {
        // In release builds we capture the plugin's stderr to a per-plugin log
        // file; in dev (`log_path == None`) we inherit it so it shows on the
        // console alongside the host's logs.
        let stderr = if log_path.is_some() {
            Stdio::piped()
        } else {
            Stdio::inherit()
        };
        let mut child = Command::new(executable)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("failed to spawn plugin {}: {e}", executable.display()))?;
        tracing::info!(
            "spawned plugin process {} (pid {:?})",
            executable.display(),
            child.id()
        );

        let mut stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");

        // Forward captured stderr to the plugin's log file (release builds).
        if let Some(path) = log_path {
            if let Some(stderr) = child.stderr.take() {
                tokio::spawn(async move {
                    let file = match tokio::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                        .await
                    {
                        Ok(f) => f,
                        Err(e) => {
                            tracing::warn!("cannot open plugin log {}: {e}", path.display());
                            return;
                        }
                    };
                    let mut file = file;
                    let mut lines = BufReader::new(stderr).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        if file.write_all(line.as_bytes()).await.is_err()
                            || file.write_all(b"\n").await.is_err()
                        {
                            break;
                        }
                        let _ = file.flush().await;
                    }
                });
            }
        }

        let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<String>();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));

        // Writer: serialize outbound request lines onto the child's stdin.
        tokio::spawn(async move {
            while let Some(line) = stdin_rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err()
                    || stdin.write_all(b"\n").await.is_err()
                    || stdin.flush().await.is_err()
                {
                    break;
                }
            }
        });

        // Reader: match each response line to its waiting caller by id. On EOF
        // (child exited/crashed), mark dead and fail every in-flight request.
        let pending_r = pending.clone();
        let alive_r = alive.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(resp) = serde_json::from_str::<Response>(&line) {
                    if let Some(tx) = pending_r.lock().await.remove(&resp.id) {
                        let res = match resp.err {
                            Some(e) => Err(e),
                            None => Ok(resp.ok.unwrap_or(Value::Null)),
                        };
                        let _ = tx.send(res);
                    }
                }
            }
            alive_r.store(false, Ordering::SeqCst);
            let mut p = pending_r.lock().await;
            let dropped = p.len();
            if dropped > 0 {
                tracing::warn!("plugin process exited with {dropped} in-flight request(s) pending");
            } else {
                tracing::info!("plugin process stdout closed (process exited)");
            }
            for (_, tx) in p.drain() {
                let _ = tx.send(Err(RpcError {
                    kind: "backend".into(),
                    message: "plugin process exited".into(),
                }));
            }
        });

        Ok(Self {
            stdin_tx,
            pending,
            next_id: AtomicU64::new(1),
            alive,
            _child: child,
        })
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Send one request and await its correlated response.
    async fn request(&self, method: &str, params: Value) -> Result<Value, PluginError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.request_with_id(id, method, params).await
    }

    /// Reserve a request id (so a caller can record it for cancellation before
    /// awaiting via [`PluginProcess::request_with_id`]).
    fn reserve_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Send a request under a caller-provided `id` and await its response.
    async fn request_with_id(
        &self,
        id: u64,
        method: &str,
        params: Value,
    ) -> Result<Value, PluginError> {
        if !self.is_alive() {
            return Err(PluginError::Backend("plugin process is not running".into()));
        }
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let req = Request {
            id,
            method: method.to_string(),
            params,
        };
        let line = serde_json::to_string(&req).map_err(|e| PluginError::Backend(e.to_string()))?;
        if self.stdin_tx.send(line).is_err() {
            self.pending.lock().await.remove(&id);
            return Err(PluginError::Backend("plugin process is not running".into()));
        }

        match rx.await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(rpc)) => Err(rpc.into()),
            Err(_) => Err(PluginError::Backend("plugin process exited".into())),
        }
    }
}

/// Discovers plugins and routes host commands to the right plugin process.
pub struct PluginManager {
    /// Directory holding plugin executables + `*.plugin.json` manifests; also
    /// the install target for plugins fetched from GitHub.
    plugins_dir: PathBuf,
    /// Where to write captured plugin stderr (`<logs_dir>/plugin-<id>.log`).
    /// `None` in dev builds, where plugin stderr is inherited to the console.
    logs_dir: Option<PathBuf>,
    plugins: RwLock<HashMap<String, DiscoveredPlugin>>,
    processes: Mutex<HashMap<String, Arc<PluginProcess>>>,
    routes: Mutex<HashMap<ConnectionId, String>>,
    /// The current cancellable call per connection: `(plugin_id, request_id)`.
    /// Lets `cancel` target the in-flight request for a connection.
    in_flight: Mutex<HashMap<ConnectionId, (String, u64)>>,
}

/// What [`PluginManager::preview_github`] reports for the UI to confirm before
/// anything is downloaded or executed. `sha256` is `None` when the release
/// publishes no checksum for the asset.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GithubPreview {
    pub repo: String,
    pub tag: String,
    pub asset_name: String,
    pub size_bytes: u64,
    pub sha256: Option<String>,
    pub download_url: String,
}

/// Install/update state of an [`AvailablePlugin`] relative to what's installed.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginStatus {
    /// Not installed.
    NotInstalled,
    /// Installed and matching the latest release in this channel.
    UpToDate,
    /// Installed, but a newer release is available in this channel.
    UpdateAvailable,
    /// Installed, but staleness can't be determined (no install provenance, or
    /// an unparseable version).
    Unknown,
}

/// A plugin available to install from the configured GitHub repo, as reported
/// by [`PluginManager::list_github_plugins`]. All plugins share one release tag
/// (`plugins-latest`/`plugins-v0.2.0`); `id` is derived from the asset name
/// (e.g. `polyclient-plugin-postgres-<triple>` -> `postgres`), which is also the
/// installed plugin's id, so the UI can match against `list_plugins`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailablePlugin {
    pub id: String,
    pub tag: String,
    /// `"nightly"` or `"stable"` — the channel this listing reflects.
    pub channel: String,
    /// Human-facing name from the release's `plugin_info.json`, if published.
    /// Falls back to `None` (UI shows `id`) for releases without the file.
    pub name: Option<String>,
    /// Plugin description from `plugin_info.json`, if published.
    pub description: Option<String>,
    pub asset_name: String,
    pub size_bytes: u64,
    /// Available version. For stable this comes from the tag; for nightly (no
    /// version in the tag) it is filled from `plugin_info.json` when present.
    pub available_version: Option<String>,
    /// Release publish timestamp (used for nightly update detection).
    pub published_at: Option<String>,
    /// The currently-installed version, if this plugin is installed.
    pub installed_version: Option<String>,
    pub status: PluginStatus,
}

impl PluginManager {
    /// Scan `dir` for `*.plugin.json` manifests, caching each plugin's info and
    /// executable. Never spawns a process. Malformed or version-incompatible
    /// manifests are logged and skipped.
    pub fn discover(dir: &Path, logs_dir: Option<PathBuf>) -> Self {
        let mut plugins = HashMap::new();
        match std::fs::read_dir(dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let is_manifest = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.ends_with(".plugin.json"));
                    if !is_manifest {
                        continue;
                    }
                    match load_manifest(&path) {
                        Ok(p) => {
                            tracing::info!(
                                "discovered plugin '{}' ({})",
                                p.info.id,
                                path.display()
                            );
                            plugins.insert(p.info.id.clone(), p);
                        }
                        Err(e) => {
                            tracing::warn!("skipping plugin manifest {}: {e}", path.display())
                        }
                    }
                }
            }
            Err(e) => tracing::warn!("plugins dir {} not readable: {e}", dir.display()),
        }
        Self {
            plugins_dir: dir.to_path_buf(),
            logs_dir,
            plugins: RwLock::new(plugins),
            processes: Mutex::new(HashMap::new()),
            routes: Mutex::new(HashMap::new()),
            in_flight: Mutex::new(HashMap::new()),
        }
    }

    /// Cached plugin infos — no process spawn.
    pub fn list_plugins(&self) -> Vec<PluginInfo> {
        self.plugins
            .read()
            .unwrap()
            .values()
            .map(|p| p.info.clone())
            .collect()
    }

    /// Get (or lazily spawn / respawn) the process for `plugin_id`.
    async fn process(&self, plugin_id: &str) -> Result<Arc<PluginProcess>, PluginError> {
        let mut procs = self.processes.lock().await;
        if let Some(p) = procs.get(plugin_id) {
            if p.is_alive() {
                return Ok(p.clone());
            }
            tracing::warn!("plugin '{plugin_id}' process is dead; respawning");
            procs.remove(plugin_id);
        }
        let executable = {
            let plugins = self.plugins.read().unwrap();
            plugins
                .get(plugin_id)
                .map(|p| p.executable.clone())
                .ok_or_else(|| PluginError::NotFound(format!("plugin {plugin_id}")))?
        };
        let proc = Arc::new(
            PluginProcess::spawn(&executable, self.plugin_log_path(plugin_id))
                .map_err(PluginError::Backend)?,
        );
        procs.insert(plugin_id.to_string(), proc.clone());
        Ok(proc)
    }

    /// Path to a plugin's stderr log file (`<logs_dir>/plugin-<id>.log`), or
    /// `None` in dev builds where stderr is inherited to the console.
    fn plugin_log_path(&self, plugin_id: &str) -> Option<PathBuf> {
        self.logs_dir
            .as_ref()
            .map(|d| d.join(format!("plugin-{plugin_id}.log")))
    }

    pub async fn test_connection(
        &self,
        plugin_id: &str,
        config: ConnectionConfig,
    ) -> Result<(), PluginError> {
        let proc = self.process(plugin_id).await?;
        proc.request("test", json!({ "config": config })).await?;
        Ok(())
    }

    pub async fn open_connection(
        &self,
        plugin_id: &str,
        config: ConnectionConfig,
    ) -> Result<ConnectionId, PluginError> {
        let proc = self.process(plugin_id).await?;
        let id = ConnectionId::new();
        proc.request("connect", json!({ "connectionId": id, "config": config }))
            .await?;
        self.routes.lock().await.insert(id, plugin_id.to_string());
        tracing::info!("opened connection {id:?} on plugin '{plugin_id}'");
        Ok(id)
    }

    pub async fn close_connection(&self, id: ConnectionId) -> Result<(), PluginError> {
        let plugin_id = self.routes.lock().await.remove(&id);
        if let Some(plugin_id) = plugin_id {
            tracing::info!("closing connection {id:?} on plugin '{plugin_id}'");
            // Best-effort: if the process is gone the connection is already gone.
            if let Some(proc) = self.processes.lock().await.get(&plugin_id).cloned() {
                let _ = proc.request("close", json!({ "connectionId": id })).await;
            }
        } else {
            tracing::debug!("close_connection for unknown connection {id:?} (no-op)");
        }
        Ok(())
    }

    /// Route an opaque capability call to the plugin that owns `connection_id`.
    pub async fn plugin_call(
        &self,
        connection_id: ConnectionId,
        op: String,
        params: Value,
    ) -> Result<Value, PluginError> {
        let plugin_id = self
            .routes
            .lock()
            .await
            .get(&connection_id)
            .cloned()
            .ok_or_else(|| PluginError::NotFound(format!("connection {connection_id:?}")))?;
        let proc = self.process(&plugin_id).await?;
        // Record this call as the connection's cancellable in-flight request, so
        // `cancel` can target it; clear it once the call settles.
        let id = proc.reserve_id();
        tracing::debug!(
            "plugin_call '{op}' (plugin '{plugin_id}', conn {connection_id:?}, req {id})"
        );
        self.in_flight
            .lock()
            .await
            .insert(connection_id, (plugin_id.clone(), id));
        let res = proc
            .request_with_id(
                id,
                "call",
                json!({ "connectionId": connection_id, "op": &op, "params": params }),
            )
            .await;
        self.in_flight.lock().await.remove(&connection_id);
        if let Err(e) = &res {
            tracing::warn!("plugin_call '{op}' failed (plugin '{plugin_id}', req {id}): {e}");
        }
        res
    }

    /// Cancel the in-flight call for `connection_id`, if any. Sends a `cancel`
    /// to the owning plugin, which aborts the request task (dropping its query
    /// future closes the connection so the database terminates the statement).
    pub async fn cancel(&self, connection_id: ConnectionId) -> Result<(), PluginError> {
        let target = self.in_flight.lock().await.get(&connection_id).cloned();
        let Some((plugin_id, id)) = target else {
            return Ok(());
        };
        tracing::info!(
            "cancelling in-flight req {id} for connection {connection_id:?} (plugin '{plugin_id}')"
        );
        let proc = self.process(&plugin_id).await?;
        proc.request("cancel", json!({ "id": id })).await?;
        Ok(())
    }

    // -- GitHub install ----------------------------------------------------

    /// List the plugins installable from `repo` (`owner/name`) for this app's
    /// release channel: nightly apps see `<plugin>-latest` prereleases, stable
    /// apps see the highest `<plugin>-v<semver>` release. Each entry carries an
    /// install/update [`PluginStatus`] computed against what's installed.
    /// Fetches the release list (and, for installed plugins, nothing further) —
    /// nothing is downloaded or executed.
    pub async fn list_github_plugins(&self, repo: &str) -> Result<Vec<AvailablePlugin>, String> {
        let triple = github::target_triple()
            .ok_or_else(|| "unsupported platform: no known target triple".to_string())?;
        let channel = github::Channel::parse(crate::release_channel());
        let releases = github::fetch_releases(repo).await?;
        let selected = github::select_plugin_releases(&releases, triple, channel);

        // Fetch each selected release's `plugin_info.json` once (small, no
        // execution — same safety class as the checksum fetch in preview_github),
        // building a tag -> (id -> metadata) cache. Best-effort: a missing or
        // malformed file just leaves names/descriptions empty.
        let mut meta_by_tag: std::collections::HashMap<
            &str,
            std::collections::HashMap<String, github::PluginMeta>,
        > = std::collections::HashMap::new();
        for pr in &selected {
            if meta_by_tag.contains_key(pr.tag) {
                continue;
            }
            let metas = match releases.iter().find(|r| r.tag_name == pr.tag) {
                Some(r) => match github::find_plugin_info(r) {
                    Some(asset) => match github::download_text(&asset.browser_download_url).await {
                        Ok(text) => github::parse_plugin_info(&text),
                        Err(e) => {
                            tracing::warn!("failed to fetch plugin_info.json for {}: {e}", pr.tag);
                            Default::default()
                        }
                    },
                    None => Default::default(),
                },
                None => Default::default(),
            };
            meta_by_tag.insert(pr.tag, metas);
        }

        Ok(selected
            .into_iter()
            .map(|pr| {
                // Snapshot what's installed for this id (version + provenance).
                let installed = {
                    let plugins = self.plugins.read().unwrap();
                    plugins
                        .get(&pr.id)
                        .map(|p| (p.info.version.clone(), p.source.clone()))
                };
                let installed_version = installed.as_ref().map(|(v, _)| v.clone());
                let status = status_for(&pr, installed.as_ref().map(|(v, s)| (v.as_str(), s)));
                let meta = meta_by_tag.get(pr.tag).and_then(|m| m.get(&pr.id));
                let name = meta.and_then(|m| m.name.clone());
                let description = meta.and_then(|m| m.description.clone());
                // Tag carries the version for stable; fall back to plugin_info.json
                // (the only version source for nightly).
                let available_version = pr
                    .version
                    .clone()
                    .or_else(|| meta.and_then(|m| m.version.clone()));
                AvailablePlugin {
                    id: pr.id,
                    tag: pr.tag.to_string(),
                    channel: pr.channel.as_str().to_string(),
                    name,
                    description,
                    asset_name: pr.asset.name.clone(),
                    size_bytes: pr.asset.size,
                    available_version,
                    published_at: pr.published_at.map(str::to_string),
                    installed_version,
                    status,
                }
            })
            .collect())
    }

    /// Resolve a GitHub release and report which asset would be installed and
    /// its published checksum, WITHOUT downloading the binary or executing
    /// anything. Drives the confirmation step in the UI. `plugin_id` picks the
    /// right asset when a single release publishes several plugins.
    pub async fn preview_github(
        &self,
        repo: &str,
        tag: Option<String>,
        plugin_id: Option<&str>,
    ) -> Result<GithubPreview, String> {
        let triple = github::target_triple()
            .ok_or_else(|| "unsupported platform: no known target triple".to_string())?;
        let release = github::fetch_release(repo, tag.as_deref()).await?;
        let asset = github::select_binary_asset(&release, triple, plugin_id)?;

        // The checksum file is small and safe to fetch (no execution).
        let sha256 = match github::find_checksum(&release, &asset.name) {
            Some(c) => {
                let text = github::download_text(&c.browser_download_url).await?;
                github::parse_sha256sums(&text, &asset.name)
            }
            None => None,
        };

        Ok(GithubPreview {
            repo: repo.to_string(),
            tag: release.tag_name.clone(),
            asset_name: asset.name.clone(),
            size_bytes: asset.size,
            sha256,
            download_url: asset.browser_download_url.clone(),
        })
    }

    /// Download, verify, and install the plugin from the release `tag` resolved
    /// during [`preview_github`]. `expected_sha` is the checksum the user
    /// confirmed; a mismatch is a hard error. `None` means the release published
    /// no checksum and the user opted to install anyway. When installing from a
    /// release with multiple plugins, `plugin_id` must be specified to select
    /// the correct asset.
    pub async fn install_github(
        &self,
        repo: &str,
        tag: &str,
        plugin_id: Option<&str>,
        expected_sha: Option<String>,
    ) -> Result<PluginInfo, String> {
        let triple = github::target_triple()
            .ok_or_else(|| "unsupported platform: no known target triple".to_string())?;
        let release = github::fetch_release(repo, Some(tag)).await?;
        let asset = github::select_binary_asset(&release, triple, plugin_id)?;
        let asset_name = asset.name.clone();

        let bytes = github::download_bytes(&asset.browser_download_url).await?;
        let actual = github::sha256_hex(&bytes);
        if let Some(expected) = &expected_sha {
            if !actual.eq_ignore_ascii_case(expected) {
                tracing::warn!(
                    "checksum mismatch installing from {repo}@{tag}: expected {expected}, got {actual}"
                );
                return Err(format!(
                    "checksum mismatch: expected {expected}, downloaded {actual}"
                ));
            }
        }

        // Place the binary in the plugins dir and make it executable.
        std::fs::create_dir_all(&self.plugins_dir).map_err(|e| e.to_string())?;
        let bin_path = self.plugins_dir.join(&asset_name);
        std::fs::write(&bin_path, &bytes).map_err(|e| e.to_string())?;
        set_executable(&bin_path)?;

        // Generate the manifest from the binary itself (checksum-verified bytes).
        let info = describe_binary(&bin_path).await?;
        if info.protocol_version != PROTOCOL_VERSION {
            let _ = std::fs::remove_file(&bin_path);
            return Err(format!(
                "plugin speaks protocol version {} but host speaks {PROTOCOL_VERSION}",
                info.protocol_version
            ));
        }

        // Record install provenance (channel from the tag, the release's publish
        // date, and the asset's updated_at) so the installer can later detect a
        // newer release. For nightly, asset_updated_at is the freshness signal.
        let asset_updated_at = asset.updated_at.clone();
        let source =
            github::is_plugins_release(&release.tag_name).map(|(channel, _)| InstallSource {
                repo: repo.to_string(),
                tag: release.tag_name.clone(),
                channel: channel.as_str().to_string(),
                version: info.version.clone(),
                published_at: release.published_at.clone(),
                asset_updated_at: asset_updated_at.clone(),
            });

        let manifest_path = self.plugins_dir.join(format!("{}.plugin.json", info.id));
        let info_value = serde_json::to_value(&info).map_err(|e| e.to_string())?;
        let mut manifest =
            json!({ "pluginInfo": info_value, "executable": format!("./{asset_name}") });
        if let Some(src) = &source {
            manifest["source"] = serde_json::to_value(src).map_err(|e| e.to_string())?;
        }
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;

        // Replace any previously-running process for this id so the next call
        // spawns the new binary; register the plugin for discovery.
        self.processes.lock().await.remove(&info.id);
        self.plugins.write().unwrap().insert(
            info.id.clone(),
            DiscoveredPlugin {
                info: info.clone(),
                executable: bin_path,
                source,
            },
        );
        tracing::info!("installed plugin '{}' from {repo}@{tag}", info.id);
        Ok(info)
    }

    /// Uninstall a plugin: stop its process (if running), drop it from the
    /// in-memory registry, and delete its manifest and executable from the
    /// plugins dir. Refuses if the plugin has open connections, so a live
    /// workspace isn't yanked out from under the user. Only files inside the
    /// plugins dir are deleted; an absolute executable path pointing elsewhere
    /// (e.g. a dev symlink) is left alone. Idempotent for an unknown id.
    pub async fn uninstall(&self, plugin_id: &str) -> Result<(), String> {
        // Guard against a path-escaping id reaching `join` below.
        if plugin_id.is_empty()
            || plugin_id == ".."
            || plugin_id.contains('/')
            || plugin_id.contains('\\')
        {
            return Err(format!("invalid plugin id: {plugin_id:?}"));
        }

        // Refuse while connections route to this plugin — closing them is the
        // user's call, not a side effect of uninstall.
        if self.routes.lock().await.values().any(|p| p == plugin_id) {
            return Err(format!(
                "plugin '{plugin_id}' has open connections; close them first"
            ));
        }

        // Drop the registry entry; capture the executable path to delete it.
        let executable = self
            .plugins
            .write()
            .unwrap()
            .remove(plugin_id)
            .map(|p| p.executable);

        // Kill the running process, if any (kill_on_drop fires when the last
        // Arc is dropped here).
        self.processes.lock().await.remove(plugin_id);

        // Delete the manifest.
        let manifest_path = self.plugins_dir.join(format!("{plugin_id}.plugin.json"));
        if manifest_path.exists() {
            std::fs::remove_file(&manifest_path)
                .map_err(|e| format!("failed to remove manifest: {e}"))?;
        }

        // Delete the executable, but only if it lives inside the plugins dir —
        // leave manually-placed/symlinked dev binaries elsewhere untouched.
        if let Some(exe) = executable {
            if exe.starts_with(&self.plugins_dir) && exe.exists() {
                std::fs::remove_file(&exe)
                    .map_err(|e| format!("failed to remove executable: {e}"))?;
            }
        }

        tracing::info!("uninstalled plugin '{plugin_id}'");
        Ok(())
    }
}

/// Compute the install/update status of an available release `pr` against the
/// installed plugin, given as `(installed_version, recorded_source)`. Pure.
fn status_for(
    pr: &github::PluginRelease<'_>,
    installed: Option<(&str, &Option<InstallSource>)>,
) -> PluginStatus {
    let Some((installed_version, source)) = installed else {
        return PluginStatus::NotInstalled;
    };
    match pr.channel {
        // Stable: the version is in the tag; compare semver to what's installed.
        github::Channel::Stable => match (
            pr.version.as_deref().and_then(github::parse_semver),
            github::parse_semver(installed_version),
        ) {
            (Some(avail), Some(inst)) if avail > inst => PluginStatus::UpdateAvailable,
            (Some(_), Some(_)) => PluginStatus::UpToDate,
            _ => PluginStatus::Unknown,
        },
        // Nightly: no version in the tag, and the release's `published_at` is
        // frozen because `plugins-latest` is updated in place. Compare the
        // binary asset's `updated_at` (which GitHub bumps on re-upload). Fall
        // back to `published_at` for plugins installed before we recorded the
        // asset timestamp. (ISO-8601 UTC sorts lexicographically.)
        github::Channel::Nightly => {
            let (avail, inst) = match (
                pr.asset_updated_at,
                source.as_ref().and_then(|s| s.asset_updated_at.as_deref()),
            ) {
                (Some(a), Some(i)) => (Some(a), Some(i)),
                _ => (
                    pr.published_at,
                    source.as_ref().and_then(|s| s.published_at.as_deref()),
                ),
            };
            match (avail, inst) {
                (Some(avail), Some(inst)) if avail > inst => PluginStatus::UpdateAvailable,
                (Some(_), Some(_)) => PluginStatus::UpToDate,
                _ => PluginStatus::Unknown,
            }
        }
    }
}

/// Mark a file executable (no-op on non-Unix).
fn set_executable(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)
            .map_err(|e| e.to_string())?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).map_err(|e| e.to_string())?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Run a plugin binary's `--describe` and parse its `PluginInfo`.
async fn describe_binary(bin_path: &Path) -> Result<PluginInfo, String> {
    let out = Command::new(bin_path)
        .arg("--describe")
        .output()
        .await
        .map_err(|e| format!("failed to run {} --describe: {e}", bin_path.display()))?;
    if !out.status.success() {
        return Err(format!(
            "{} --describe exited with {}: {}",
            bin_path.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    serde_json::from_slice::<PluginInfo>(&out.stdout)
        .map_err(|e| format!("invalid PluginInfo from --describe: {e}"))
}

/// Read and validate one manifest file, resolving its executable path.
fn load_manifest(path: &Path) -> Result<DiscoveredPlugin, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;

    if manifest.plugin_info.protocol_version != PROTOCOL_VERSION {
        return Err(format!(
            "incompatible protocol version {} (host speaks {PROTOCOL_VERSION})",
            manifest.plugin_info.protocol_version
        ));
    }

    let exe = PathBuf::from(&manifest.executable);
    let executable = if exe.is_absolute() {
        exe
    } else {
        path.parent().unwrap_or_else(|| Path::new(".")).join(exe)
    };
    if !executable.exists() {
        return Err(format!("executable not found: {}", executable.display()));
    }

    Ok(DiscoveredPlugin {
        info: manifest.plugin_info,
        executable,
        source: manifest.source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_json(version: u32, executable: &str) -> String {
        format!(
            r#"{{"pluginInfo":{{"id":"x","name":"X","kind":"rdbms","version":"0.1.0",
               "description":"","config_schema":[],"protocol_version":{version}}},
               "executable":"{executable}"}}"#
        )
    }

    #[test]
    fn rejects_incompatible_protocol_version() {
        let dir = std::env::temp_dir().join("polyclient_pm_test_badver");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.plugin.json");
        std::fs::write(&path, manifest_json(PROTOCOL_VERSION + 1, "x")).unwrap();

        let err = load_manifest(&path).unwrap_err();
        assert!(err.contains("incompatible protocol version"), "{err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_missing_executable() {
        let dir = std::env::temp_dir().join("polyclient_pm_test_noexe");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.plugin.json");
        std::fs::write(&path, manifest_json(PROTOCOL_VERSION, "does-not-exist")).unwrap();

        let err = load_manifest(&path).unwrap_err();
        assert!(err.contains("executable not found"), "{err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn discover_skips_bad_manifests_and_returns_empty() {
        let dir = std::env::temp_dir().join("polyclient_pm_test_discover");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.plugin.json"), manifest_json(999, "x")).unwrap();
        std::fs::write(dir.join("ignore.txt"), "not a manifest").unwrap();

        let mgr = PluginManager::discover(&dir, None);
        assert!(mgr.list_plugins().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn uninstall_removes_manifest_and_executable() {
        let dir = std::env::temp_dir().join("polyclient_pm_test_uninstall");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = dir.join("x.plugin.json");
        let exe = dir.join("x-bin");
        std::fs::write(&exe, b"#!/bin/sh\n").unwrap();
        std::fs::write(&manifest, manifest_json(PROTOCOL_VERSION, "./x-bin")).unwrap();

        let mgr = PluginManager::discover(&dir, None);
        assert_eq!(mgr.list_plugins().len(), 1);

        mgr.uninstall("x").await.unwrap();
        assert!(mgr.list_plugins().is_empty());
        assert!(!manifest.exists(), "manifest should be deleted");
        assert!(!exe.exists(), "executable should be deleted");

        // Idempotent: uninstalling an unknown id is a no-op success.
        mgr.uninstall("x").await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn uninstall_rejects_path_escaping_id() {
        let dir = std::env::temp_dir().join("polyclient_pm_test_uninstall_guard");
        std::fs::create_dir_all(&dir).unwrap();
        let mgr = PluginManager::discover(&dir, None);
        for bad in ["", "..", "a/b", "a\\b"] {
            assert!(mgr.uninstall(bad).await.is_err(), "should reject {bad:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_for_covers_all_cases() {
        use github::{Asset, Channel, PluginRelease};
        let asset = Asset {
            name: "bin".into(),
            browser_download_url: "u".into(),
            size: 1,
            ..Default::default()
        };
        let none: Option<InstallSource> = None;

        // Not installed.
        let stable = PluginRelease {
            id: "p".into(),
            tag: "p-v0.2.0",
            channel: Channel::Stable,
            version: Some("0.2.0".into()),
            published_at: None,
            asset_updated_at: None,
            asset: &asset,
        };
        assert!(matches!(
            status_for(&stable, None),
            PluginStatus::NotInstalled
        ));
        // Stable: version compare.
        assert!(matches!(
            status_for(&stable, Some(("0.2.0", &none))),
            PluginStatus::UpToDate
        ));
        assert!(matches!(
            status_for(&stable, Some(("0.1.0", &none))),
            PluginStatus::UpdateAvailable
        ));
        assert!(matches!(
            status_for(&stable, Some(("not-semver", &none))),
            PluginStatus::Unknown
        ));

        // Nightly fallback: no asset_updated_at on either side -> compare the
        // release published_at (legacy installs).
        let nightly = PluginRelease {
            id: "p".into(),
            tag: "p-latest",
            channel: Channel::Nightly,
            version: None,
            published_at: Some("2026-02-01T00:00:00Z"),
            asset_updated_at: None,
            asset: &asset,
        };
        let older = Some(InstallSource {
            published_at: Some("2026-01-01T00:00:00Z".into()),
            ..Default::default()
        });
        let same = Some(InstallSource {
            published_at: Some("2026-02-01T00:00:00Z".into()),
            ..Default::default()
        });
        assert!(matches!(
            status_for(&nightly, Some(("x", &older))),
            PluginStatus::UpdateAvailable
        ));
        assert!(matches!(
            status_for(&nightly, Some(("x", &same))),
            PluginStatus::UpToDate
        ));
        // Installed without provenance -> can't tell.
        assert!(matches!(
            status_for(&nightly, Some(("x", &none))),
            PluginStatus::Unknown
        ));

        // Nightly preferred path: both sides carry asset_updated_at, which is
        // what actually changes per build on the rolling `plugins-latest` tag.
        let nightly_asset = PluginRelease {
            id: "p".into(),
            tag: "p-latest",
            channel: Channel::Nightly,
            version: None,
            // published_at frozen (same on both sides) -> proves we key off
            // the asset timestamp, not the release date.
            published_at: Some("2026-02-01T00:00:00Z"),
            asset_updated_at: Some("2026-03-10T00:00:00Z"),
            asset: &asset,
        };
        let asset_older = Some(InstallSource {
            published_at: Some("2026-02-01T00:00:00Z".into()),
            asset_updated_at: Some("2026-03-01T00:00:00Z".into()),
            ..Default::default()
        });
        let asset_same = Some(InstallSource {
            published_at: Some("2026-02-01T00:00:00Z".into()),
            asset_updated_at: Some("2026-03-10T00:00:00Z".into()),
            ..Default::default()
        });
        assert!(matches!(
            status_for(&nightly_asset, Some(("x", &asset_older))),
            PluginStatus::UpdateAvailable
        ));
        assert!(matches!(
            status_for(&nightly_asset, Some(("x", &asset_same))),
            PluginStatus::UpToDate
        ));
        // Available has asset_updated_at but the install predates it -> fall
        // back to published_at (here both equal -> up to date).
        assert!(matches!(
            status_for(&nightly_asset, Some(("x", &same))),
            PluginStatus::UpToDate
        ));
    }
}

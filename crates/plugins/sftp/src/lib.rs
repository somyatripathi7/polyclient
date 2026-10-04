use async_trait::async_trait;
use polyclient_core::{
    cfg_secret, require_str, ConfigField, ConfigFieldType, Connection, ConnectionConfig, Plugin,
    PluginError, PluginInfo, PluginKind, Result, ShowIf,
};
use polyclient_filemanager_common::{
    cap_entries, dispatch_filemanager, downcast_conn, stream_to_local_file, Cancelled, FileBackend,
    FileEntry, JobState, ListDirResult, ScannedFile, Stat, TRANSFER_CHUNK,
};
use russh::client::{self, Handle};
use russh::keys::{load_secret_key, PrivateKeyWithHashAlg};
use russh_sftp::client::SftpSession;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

// ── russh client handler ─────────────────────────────────────────────────────

/// A russh client handler that accepts the server key. We don't pin host keys
/// yet (no known-hosts UI); accepting the key keeps the file manager usable
/// while leaving room to add verification later.
struct ClientHandler;

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::PublicKey,
    ) -> std::result::Result<bool, Self::Error> {
        Ok(true)
    }
}

// ── Backend handle ─────────────────────────────────────────────────────────

/// A small cloneable handle implementing [`FileBackend`]. `Arc` (not `Mutex`):
/// `SftpSession` methods take `&self` and the session multiplexes concurrent ops
/// over its channel internally, so we can share it freely — including handing a
/// clone to a background transfer task without blocking ordinary calls like
/// `list_dir`.
#[derive(Clone)]
pub struct SftpBackend {
    sftp: Arc<SftpSession>,
}

// ── Connection ───────────────────────────────────────────────────────────────

pub struct SftpConnection {
    backend: SftpBackend,
    // The single current/last transfer job for this connection, or `None` if no
    // transfer has run yet. Lives as long as the connection, so it survives the
    // frontend workspace unmounting on a connection switch; the frontend
    // reattaches by polling `last_transfer_stats`.
    job: Mutex<Option<Arc<JobState>>>,
    // Keep the ssh handle alive for the lifetime of the connection; dropping it
    // closes the underlying channel the SFTP session rides on.
    _handle: Handle<ClientHandler>,
}

impl Connection for SftpConnection {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ── Plugin ───────────────────────────────────────────────────────────────────

pub struct SftpPlugin;

impl SftpPlugin {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SftpPlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Plugin for SftpPlugin {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            id: "sftp".into(),
            name: "SFTP".into(),
            kind: PluginKind::FileManager,
            version: env!("CARGO_PKG_VERSION").into(),
            description: "Browse and manage files on a remote host over SFTP.".into(),
            ui_module: None,
            protocol_version: polyclient_core::PROTOCOL_VERSION,
            config_schema: vec![
                ConfigField {
                    key: "host".into(),
                    label: "Host".into(),
                    field_type: ConfigFieldType::Text,
                    required: true,
                    default: None,
                    placeholder: Some("hostname or IP".into()),
                    show_if: None,
                },
                ConfigField {
                    key: "port".into(),
                    label: "Port".into(),
                    field_type: ConfigFieldType::Number,
                    required: false,
                    default: Some(serde_json::json!(22)),
                    placeholder: None,
                    show_if: None,
                },
                ConfigField {
                    key: "user".into(),
                    label: "User".into(),
                    field_type: ConfigFieldType::Text,
                    required: true,
                    default: None,
                    placeholder: Some("username".into()),
                    show_if: None,
                },
                ConfigField {
                    key: "auth_mode".into(),
                    label: "Auth".into(),
                    field_type: ConfigFieldType::Select {
                        // ssh-agent auth relies on a Unix socket (SSH_AUTH_SOCK)
                        // and is only offered on Unix targets.
                        #[cfg(unix)]
                        options: vec!["password".into(), "key".into(), "agent".into()],
                        #[cfg(not(unix))]
                        options: vec!["password".into(), "key".into()],
                    },
                    required: false,
                    default: Some(serde_json::json!("password")),
                    placeholder: None,
                    show_if: None,
                },
                ConfigField {
                    key: "password".into(),
                    label: "Password".into(),
                    field_type: ConfigFieldType::Password,
                    required: false,
                    default: None,
                    placeholder: None,
                    show_if: Some(ShowIf {
                        field: "auth_mode".into(),
                        equals: "password".into(),
                    }),
                },
                ConfigField {
                    key: "key_path".into(),
                    label: "Private key".into(),
                    field_type: ConfigFieldType::FilePath,
                    required: false,
                    default: None,
                    placeholder: Some("~/.ssh/id_rsa".into()),
                    show_if: Some(ShowIf {
                        field: "auth_mode".into(),
                        equals: "key".into(),
                    }),
                },
                ConfigField {
                    key: "passphrase".into(),
                    label: "Key passphrase".into(),
                    field_type: ConfigFieldType::Password,
                    required: false,
                    default: None,
                    placeholder: None,
                    show_if: Some(ShowIf {
                        field: "auth_mode".into(),
                        equals: "key".into(),
                    }),
                },
            ],
        }
    }

    async fn connect(&self, cfg: ConnectionConfig) -> Result<Arc<dyn Connection>> {
        let host = require_str(&cfg, "host")?.to_owned();
        let user = require_str(&cfg, "user")?.to_owned();
        let port = cfg.get("port").and_then(|v| v.as_u64()).unwrap_or(22) as u16;
        let auth_mode = cfg
            .get("auth_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("password")
            .to_owned();

        tracing::info!("connecting sftp to {user}@{host}:{port} (auth '{auth_mode}')");

        let config = Arc::new(client::Config::default());
        let mut handle = client::connect(config, (host.as_str(), port), ClientHandler)
            .await
            .map_err(|e| {
                tracing::warn!("sftp: cannot reach {host}:{port}: {e}");
                PluginError::Connection(format!("cannot reach {host}:{port}: {e}"))
            })?;

        let authed = match auth_mode.as_str() {
            "key" => {
                let key_path = cfg
                    .get("key_path")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        PluginError::Config("key_path is required for key auth".into())
                    })?;
                let passphrase = cfg_secret(&cfg, "passphrase")?;
                let passphrase = passphrase.as_deref().filter(|s| !s.is_empty());
                let key = load_secret_key(key_path, passphrase)
                    .map_err(|e| PluginError::Connection(format!("cannot load key: {e}")))?;
                let key = PrivateKeyWithHashAlg::new(Arc::new(key), None);
                handle
                    .authenticate_publickey(&user, key)
                    .await
                    .map_err(|e| PluginError::Connection(format!("key auth failed: {e}")))?
                    .success()
            }
            #[cfg(not(unix))]
            "agent" => {
                // `AgentClient::connect_env` connects over the `SSH_AUTH_SOCK`
                // Unix socket and only exists on Unix targets, so ssh-agent
                // auth is unavailable on Windows.
                return Err(PluginError::Connection(
                    "ssh-agent authentication is not supported on this platform; \
                     use key or password auth instead"
                        .into(),
                ));
            }
            #[cfg(unix)]
            "agent" => {
                // Try each identity the running ssh-agent offers, signing the
                // auth request through the agent, until one succeeds.
                let mut agent = russh::keys::agent::client::AgentClient::connect_env()
                    .await
                    .map_err(|e| PluginError::Connection(format!("cannot reach ssh-agent: {e}")))?;
                let identities = agent
                    .request_identities()
                    .await
                    .map_err(|e| PluginError::Connection(format!("ssh-agent error: {e}")))?;
                if identities.is_empty() {
                    return Err(PluginError::Connection(
                        "ssh-agent has no identities loaded".into(),
                    ));
                }
                let mut ok = false;
                for identity in identities {
                    let public_key = identity.public_key().into_owned();
                    let result = handle
                        .authenticate_publickey_with(&user, public_key, None, &mut agent)
                        .await
                        .map_err(|e| PluginError::Connection(format!("agent auth failed: {e}")))?;
                    if result.success() {
                        ok = true;
                        break;
                    }
                }
                ok
            }
            _ => {
                let password = cfg_secret(&cfg, "password")?.unwrap_or_default();
                handle
                    .authenticate_password(&user, password)
                    .await
                    .map_err(|e| PluginError::Connection(format!("password auth failed: {e}")))?
                    .success()
            }
        };

        if !authed {
            tracing::warn!("sftp authentication failed for {user}@{host}:{port}");
            return Err(PluginError::Connection("authentication failed".into()));
        }
        tracing::info!("sftp authenticated as {user}@{host}:{port}");

        let channel = handle
            .channel_open_session()
            .await
            .map_err(|e| PluginError::Connection(e.to_string()))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| PluginError::Connection(format!("sftp subsystem failed: {e}")))?;
        let sftp = SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| PluginError::Connection(format!("sftp session failed: {e}")))?;
        tracing::info!("sftp subsystem ready for {user}@{host}:{port}");

        Ok(Arc::new(SftpConnection {
            backend: SftpBackend {
                sftp: Arc::new(sftp),
            },
            job: Mutex::new(None),
            _handle: handle,
        }))
    }

    /// Reachability check: open a TCP connection to host:port (no SSH auth).
    async fn test(&self, cfg: ConnectionConfig) -> Result<()> {
        let host = require_str(&cfg, "host")?;
        require_str(&cfg, "user")?;
        let port = cfg.get("port").and_then(|v| v.as_u64()).unwrap_or(22) as u16;
        let addr = format!("{host}:{port}");
        let connect = tokio::net::TcpStream::connect(&addr);
        match tokio::time::timeout(Duration::from_secs(10), connect).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(PluginError::Connection(format!("cannot reach {addr}: {e}"))),
            Err(_) => Err(PluginError::Connection(format!(
                "connection to {addr} timed out"
            ))),
        }
    }
}
// ── Dispatcher ───────────────────────────────────────────────────────────────

pub struct SftpDispatcher;

#[async_trait]
impl polyclient_plugin_runtime::Dispatcher for SftpDispatcher {
    async fn dispatch(
        &self,
        op: &str,
        params: serde_json::Value,
        conn: Arc<dyn Connection>,
    ) -> Result<serde_json::Value> {
        let conn = downcast_conn::<SftpConnection>(&conn)?;
        dispatch_filemanager(op, params, conn.backend.clone(), &conn.job).await
    }
}

// ── FileBackend impl ─────────────────────────────────────────────────────────

#[async_trait]
impl FileBackend for SftpBackend {
    async fn home_dir(&self) -> Result<String> {
        // The session's working directory resolved to an absolute path — this is
        // the user's home on most servers, and (unlike "/") a directory they can
        // actually write to. Falls back to "/".
        Ok(self
            .sftp
            .canonicalize(".")
            .await
            .unwrap_or_else(|_| "/".to_owned()))
    }

    async fn list_dir(&self, path: &str) -> Result<ListDirResult> {
        let read_dir = self
            .sftp
            .read_dir(path.to_owned())
            .await
            .map_err(|e| PluginError::Backend(format!("read_dir: {e}")))?;
        // russh-sftp's `read_dir` returns the whole listing in one call, so there
        // is no early-stop to do here (unlike S3's paginated lister); we collect
        // it and let `cap_entries` sort + truncate to LIST_DIR_CAP.
        let entries: Vec<FileEntry> = read_dir
            .map(|entry| {
                let meta = entry.metadata();
                FileEntry {
                    name: entry.file_name(),
                    path: entry.path(),
                    is_dir: meta.is_dir(),
                    size: meta.size.unwrap_or(0),
                    modified: meta.mtime.unwrap_or(0) as i64,
                    permissions: meta.permissions.unwrap_or(0),
                }
            })
            .collect();
        Ok(cap_entries(entries))
    }

    async fn stat(&self, path: &str) -> Result<Stat> {
        match self.sftp.metadata(path.to_owned()).await {
            Ok(meta) => Ok(Stat {
                exists: true,
                is_dir: meta.is_dir(),
            }),
            Err(_) => Ok(Stat {
                exists: false,
                is_dir: false,
            }),
        }
    }

    async fn mkdir(&self, path: &str) -> Result<()> {
        self.sftp
            .create_dir(path.to_owned())
            .await
            .map_err(|e| PluginError::Backend(format!("create_dir: {e}")))
    }

    async fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.sftp
            .rename(from.to_owned(), to.to_owned())
            .await
            .map_err(|e| PluginError::Backend(format!("rename: {e}")))
    }

    async fn delete(&self, path: &str) -> Result<()> {
        remove_recursive(&self.sftp, path).await
    }

    async fn walk(&self, root: &str) -> Result<Vec<ScannedFile>> {
        walk_remote(&self.sftp, root).await
    }

    async fn ensure_dirs(&self, path: &str) -> Result<()> {
        ensure_remote_dirs(&self.sftp, path).await
    }

    async fn download_file(
        &self,
        remote: &str,
        local: &str,
        job: &JobState,
    ) -> Result<Option<Cancelled>> {
        // Bytes never cross the JSON-RPC pipe (the plugin runs locally). The
        // disk side — parent-dir creation, chunked write, cancel cleanup — is the
        // shared `stream_to_local_file`.
        let remote_file = self
            .sftp
            .open(remote.to_owned())
            .await
            .map_err(|e| PluginError::Backend(format!("open {remote}: {e}")))?;
        stream_to_local_file(remote_file, local, job).await
    }

    async fn upload_file(
        &self,
        local: &str,
        remote: &str,
        job: &JobState,
    ) -> Result<Option<Cancelled>> {
        // `SftpSession::write` opens WRITE-only (no CREATE) so it fails on a new
        // file — `create` opens CREATE | TRUNCATE | WRITE, which is what an upload
        // needs. Streamed in `TRANSFER_CHUNK` chunks, checking `job.cancel` between
        // chunks so a large file can be aborted mid-copy; on cancel the partial
        // remote file is removed.
        let mut input = tokio::fs::File::open(local)
            .await
            .map_err(|e| PluginError::Backend(format!("open {local}: {e}")))?;
        let mut file = self
            .sftp
            .create(remote.to_owned())
            .await
            .map_err(|e| PluginError::Backend(format!("create {remote}: {e}")))?;

        let mut buf = vec![0u8; TRANSFER_CHUNK];
        loop {
            if job.cancel.load(Ordering::SeqCst) {
                let _ = file.shutdown().await;
                drop(file);
                let _ = self.sftp.remove_file(remote.to_owned()).await;
                return Ok(Some(Cancelled));
            }
            let n = input
                .read(&mut buf)
                .await
                .map_err(|e| PluginError::Backend(format!("read {local}: {e}")))?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])
                .await
                .map_err(|e| PluginError::Backend(format!("write {remote}: {e}")))?;
        }
        file.shutdown()
            .await
            .map_err(|e| PluginError::Backend(format!("flush {remote}: {e}")))?;
        Ok(None)
    }
}

// ── SFTP-internal helpers ──────────────────────────────────────────────────--

/// Create a remote directory, tolerating "already exists". SFTP has no mkdir -p,
/// so callers create each level in order.
async fn ensure_remote_dir(sftp: &SftpSession, path: &str) -> Result<()> {
    if let Ok(true) = sftp.try_exists(path.to_owned()).await {
        return Ok(());
    }
    // Race-tolerant: if it appeared between the check and now, ignore the error.
    if sftp.create_dir(path.to_owned()).await.is_err()
        && !sftp.try_exists(path.to_owned()).await.unwrap_or(false)
    {
        return Err(PluginError::Backend(format!("mkdir {path}: failed")));
    }
    Ok(())
}

/// Create every level of a remote directory path in order (SFTP has no mkdir -p),
/// tolerating levels that already exist.
async fn ensure_remote_dirs(sftp: &SftpSession, path: &str) -> Result<()> {
    let mut acc = String::new();
    for segment in path.split('/') {
        if segment.is_empty() {
            // Leading "/" (absolute path): keep building from root.
            acc.push('/');
            continue;
        }
        if acc.is_empty() || acc == "/" {
            acc.push_str(segment);
        } else {
            acc.push('/');
            acc.push_str(segment);
        }
        ensure_remote_dir(sftp, &acc).await?;
    }
    Ok(())
}

/// Walk a remote tree rooted at `root`, returning every file under it. Iterative
/// (async recursion would need boxing), mirroring `remove_recursive`'s BFS.
async fn walk_remote(sftp: &SftpSession, root: &str) -> Result<Vec<ScannedFile>> {
    let mut out = Vec::new();
    // Stack of (remote_dir, rel_prefix) to process.
    let mut stack: Vec<(String, String)> = vec![(root.to_owned(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let read_dir = sftp
            .read_dir(dir.clone())
            .await
            .map_err(|e| PluginError::Backend(format!("read_dir {dir}: {e}")))?;
        for entry in read_dir {
            let name = entry.file_name();
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if entry.metadata().is_dir() {
                stack.push((entry.path(), rel));
            } else {
                out.push(ScannedFile {
                    src: entry.path(),
                    rel,
                });
            }
        }
    }
    Ok(out)
}

/// Recursively delete a remote path. SFTP `rmdir` only removes *empty*
/// directories, so for a directory we first gather the whole subtree (BFS),
/// then unlink every file and rmdir every directory deepest-first. Walks
/// iteratively — async recursion would need boxing.
async fn remove_recursive(sftp: &SftpSession, path: &str) -> Result<()> {
    let meta = sftp
        .metadata(path.to_owned())
        .await
        .map_err(|e| PluginError::Backend(format!("stat {path}: {e}")))?;
    if !meta.is_dir() {
        return sftp
            .remove_file(path.to_owned())
            .await
            .map_err(|e| PluginError::Backend(format!("remove_file {path}: {e}")));
    }

    // BFS the tree, recording dirs (in discovery order) and files.
    let mut dirs: Vec<String> = vec![path.to_owned()];
    let mut files: Vec<String> = Vec::new();
    let mut queue: Vec<String> = vec![path.to_owned()];
    while let Some(dir) = queue.pop() {
        let read_dir = sftp
            .read_dir(dir.clone())
            .await
            .map_err(|e| PluginError::Backend(format!("read_dir {dir}: {e}")))?;
        for entry in read_dir {
            let child = entry.path();
            if entry.metadata().is_dir() {
                dirs.push(child.clone());
                queue.push(child);
            } else {
                files.push(child);
            }
        }
    }

    // Files first, then dirs deepest-first (reverse of discovery order, which is
    // breadth-first, so parents precede children — reversing empties children
    // before their parents).
    for f in files {
        sftp.remove_file(f.clone())
            .await
            .map_err(|e| PluginError::Backend(format!("remove_file {f}: {e}")))?;
    }
    for d in dirs.into_iter().rev() {
        sftp.remove_dir(d.clone())
            .await
            .map_err(|e| PluginError::Backend(format!("remove_dir {d}: {e}")))?;
    }
    Ok(())
}

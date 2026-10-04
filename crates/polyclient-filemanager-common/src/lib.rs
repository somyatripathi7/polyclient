use async_trait::async_trait;
use polyclient_core::{Connection, PluginError, Result};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

/// A single directory listing returns at most this many entries to the UI. A
/// directory with more files than this is truncated (entries beyond the cap are
/// dropped) and the response carries `truncated: true` so the frontend can warn.
/// This keeps a directory with hundreds of thousands of entries from flooding
/// the JSON-RPC pipe and the renderer.
pub const LIST_DIR_CAP: usize = 10_000;

/// Per-file copies are streamed in chunks of this size so the cooperative
/// `cancel` flag can be observed *mid-file* (not just between files) and so a
/// large file isn't buffered whole in memory.
pub const TRANSFER_CHUNK: usize = 64 * 1024;

/// Returned by the per-file copy helpers when the cooperative cancel flag was
/// observed mid-file. The caller stops the loop and leaves the phase at
/// `Cancelled`; the partially-written destination has already been removed.
pub struct Cancelled;

// ── Types shared between plugin and frontend ─────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: i64,
    pub permissions: u32,
}

/// Response of the `filemanager.list_dir` op (mirrored in `api.ts`). `truncated`
/// is true when the directory held more than [`LIST_DIR_CAP`] entries and the
/// extra entries were dropped, so the UI can show a "showing first N" warning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListDirResult {
    pub entries: Vec<FileEntry>,
    pub truncated: bool,
}

// ── Background transfer jobs ─────────────────────────────────────────────────
//
// Uploads and downloads run as background tasks *inside the plugin process*, so
// they survive the frontend workspace unmounting on a connection switch (the
// plugin process outlives the React component, like the host's PTY). The
// frontend kicks a job off with `transfer_start`, polls `transfer_stats` for
// progress, and can `transfer_cancel` it. State is in-memory only — it lives as
// long as the connection (a full disconnect drops the connection and its jobs).

/// Where a transfer job is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobPhase {
    /// Walking the remote tree to count files (download only); `total` not final.
    Scanning,
    Running,
    Done,
    Cancelled,
    Error,
}

/// Live, shared state of one transfer job. Counters are atomics so the running
/// task updates them while `transfer_stats` reads them without locking the SFTP
/// session (which the transfer itself holds for the duration).
pub struct JobState {
    /// Files completed so far.
    pub done: AtomicU64,
    /// Total files to transfer; 0 while still scanning (download).
    pub total: AtomicU64,
    /// `JobPhase` encoded as a small int (atomic so reads never block).
    pub phase: AtomicU64,
    /// Cooperative cancel flag, checked between files.
    pub cancel: AtomicBool,
    /// The file currently being transferred (relative path), for display.
    pub current: Mutex<String>,
    /// Error message once `phase == Error`.
    pub error: Mutex<Option<String>>,
}

impl Default for JobState {
    fn default() -> Self {
        Self::new()
    }
}

impl JobState {
    pub fn new() -> Self {
        Self {
            done: AtomicU64::new(0),
            total: AtomicU64::new(0),
            phase: AtomicU64::new(JobPhase::Scanning as u64),
            cancel: AtomicBool::new(false),
            current: Mutex::new(String::new()),
            error: Mutex::new(None),
        }
    }

    pub fn set_phase(&self, p: JobPhase) {
        self.phase.store(p as u64, Ordering::SeqCst);
    }

    pub fn phase(&self) -> JobPhase {
        match self.phase.load(Ordering::SeqCst) {
            x if x == JobPhase::Scanning as u64 => JobPhase::Scanning,
            x if x == JobPhase::Running as u64 => JobPhase::Running,
            x if x == JobPhase::Done as u64 => JobPhase::Done,
            x if x == JobPhase::Cancelled as u64 => JobPhase::Cancelled,
            _ => JobPhase::Error,
        }
    }
}

/// The progress snapshot returned by `transfer_stats` (mirrored in `api.ts`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferStats {
    pub phase: JobPhase,
    pub done: u64,
    pub total: u64,
    pub current: String,
    pub error: Option<String>,
}

/// Direction of a background transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransferKind {
    Upload,
    Download,
}

/// One file/directory to transfer. For an upload, `local_path` is the source and
/// `remote_path` the destination; for a download it's the reverse. A directory
/// is mirrored recursively.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferItem {
    pub local_path: String,
    pub remote_path: String,
}

/// Params for `filemanager.start_transfer` (mirrored in `api.ts`).
#[derive(Debug, Clone, Deserialize)]
pub struct StartTransfer {
    pub kind: TransferKind,
    pub items: Vec<TransferItem>,
}

// ── Backend-neutral metadata ─────────────────────────────────────────────────

/// The slice of file metadata `run_transfer` and recursive delete need, kept
/// backend-neutral so neither russh's `Metadata` nor S3's `HeadObject` leaks
/// through the [`FileBackend`] trait.
#[derive(Debug, Clone, Copy)]
pub struct Stat {
    pub exists: bool,
    pub is_dir: bool,
}

/// One file discovered while scanning a transfer source: its source path and the
/// path relative to the transfer root (used to mirror the tree at the destination).
pub struct ScannedFile {
    /// Absolute source path. For a download this is the remote path (an object
    /// key for S3); for an upload it is the local path.
    pub src: String,
    /// Path relative to the item root, e.g. `sub/dir/file.txt`. Empty for a
    /// single top-level file.
    pub rel: String,
}

// ── FileBackend ──────────────────────────────────────────────────────────────

/// The backend-specific primitives a file-manager plugin must provide. Everything
/// generic — the op dispatch, the background transfer driver, tree-walk of the
/// local filesystem, and path helpers — lives in this crate and is shared by all
/// `PluginKind::FileManager` plugins (SFTP, S3, …). A plugin implements this trait
/// for a small cloneable handle (e.g. an `Arc<SftpSession>` or an S3 `Client` +
/// bucket) and routes its ops through [`dispatch_filemanager`].
///
/// `Clone + 'static` is required because [`dispatch_filemanager`] clones the
/// backend into the spawned transfer task so a transfer outlives the request that
/// started it.
#[async_trait]
pub trait FileBackend: Send + Sync + Clone + 'static {
    /// A writable starting directory (the SFTP home, or `""` = bucket root for S3).
    async fn home_dir(&self) -> Result<String>;

    /// List one directory, returning at most [`LIST_DIR_CAP`] entries. A backend
    /// that paginates (e.g. S3) MUST stop fetching once it has enough entries
    /// rather than materializing the whole directory and letting the caller
    /// truncate — otherwise a directory with hundreds of thousands of objects
    /// triggers an unbounded number of API calls. Entries must be returned
    /// sorted dirs-first then case-insensitively by name (see [`sort_entries`]);
    /// `truncated` is true when entries were dropped to honor the cap. Use
    /// [`cap_entries`] to apply the cap+sort+flag uniformly.
    async fn list_dir(&self, path: &str) -> Result<ListDirResult>;

    /// Backend-neutral metadata for a path.
    async fn stat(&self, path: &str) -> Result<Stat>;

    /// Create a single directory (the parent is assumed to exist).
    async fn mkdir(&self, path: &str) -> Result<()>;

    /// Move/rename a path. Atomic on SFTP; copy-then-delete on S3.
    async fn rename(&self, from: &str, to: &str) -> Result<()>;

    /// Recursively delete a file or a directory and all its contents.
    async fn delete(&self, path: &str) -> Result<()>;

    /// Walk a remote tree rooted at `root`, returning every file under it
    /// (directories are implied by file paths).
    async fn walk(&self, root: &str) -> Result<Vec<ScannedFile>>;

    /// Create every level of a directory path in order (`mkdir -p`), tolerating
    /// levels that already exist. A no-op-ish marker write for object stores.
    async fn ensure_dirs(&self, path: &str) -> Result<()>;

    /// Stream one remote file straight to `local` on disk. Checks `job.cancel`
    /// between chunks; on cancel removes the partial local file and returns
    /// `Ok(Some(Cancelled))`. Bytes never cross the JSON-RPC pipe (the plugin
    /// runs locally). See [`stream_to_local_file`] for a reusable disk-side helper.
    async fn download_file(
        &self,
        remote: &str,
        local: &str,
        job: &JobState,
    ) -> Result<Option<Cancelled>>;

    /// Stream one local file up to `remote`, creating/truncating the destination.
    /// Checks `job.cancel` where the backend allows (between chunks/parts); on
    /// cancel removes the partial destination and returns `Ok(Some(Cancelled))`.
    async fn upload_file(
        &self,
        local: &str,
        remote: &str,
        job: &JobState,
    ) -> Result<Option<Cancelled>>;
}

// ── Shared op dispatch ───────────────────────────────────────────────────────

/// Downcast a `Arc<dyn Connection>` to a plugin's concrete connection type.
pub fn downcast_conn<T: 'static>(conn: &Arc<dyn Connection>) -> Result<&T> {
    conn.as_any()
        .downcast_ref::<T>()
        .ok_or_else(|| PluginError::Backend("connection type mismatch".into()))
}

/// Sort directory entries dirs-first, then case-insensitively by name. Shared so
/// every backend's `list_dir` presents the same order to the UI.
pub fn sort_entries(entries: &mut [FileEntry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// Sort `entries`, then cap them at [`LIST_DIR_CAP`], producing the
/// [`ListDirResult`] a backend's `list_dir` returns. `truncated` is true when
/// entries were dropped. Backends that paginate should also stop fetching once
/// they hold more than `LIST_DIR_CAP` entries (so the over-cap entry is enough
/// to set the flag) — this helper only handles the final sort/cap.
pub fn cap_entries(mut entries: Vec<FileEntry>) -> ListDirResult {
    sort_entries(&mut entries);
    let truncated = entries.len() > LIST_DIR_CAP;
    if truncated {
        entries.truncate(LIST_DIR_CAP);
    }
    ListDirResult { entries, truncated }
}

fn require_path(params: &serde_json::Value, key: &str) -> Result<String> {
    params[key]
        .as_str()
        .map(|s| s.to_owned())
        .ok_or_else(|| PluginError::Config(format!("{key} is required")))
}

/// Resolve the target paths of a `filemanager.delete` request from the `paths`
/// array (the delete op is always batch — a single delete is an array of one).
/// At least one path is required.
fn delete_paths(params: &serde_json::Value) -> Result<Vec<String>> {
    let paths: Vec<String> = params["paths"]
        .as_array()
        .ok_or_else(|| PluginError::Config("paths is required".into()))?
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_owned()))
        .collect();
    if paths.is_empty() {
        return Err(PluginError::Config("paths must not be empty".into()));
    }
    Ok(paths)
}

/// Serve one `filemanager.*` op against `backend`. This is the entire op surface
/// every file-manager plugin exposes; the plugin's `Dispatcher::dispatch` only
/// downcasts its connection and forwards here. `job_slot` is the single
/// current/last transfer job, owned by the connection so it survives the frontend
/// workspace unmounting on a connection switch.
pub async fn dispatch_filemanager<B: FileBackend>(
    op: &str,
    params: serde_json::Value,
    backend: B,
    job_slot: &Mutex<Option<Arc<JobState>>>,
) -> Result<serde_json::Value> {
    match op {
        "filemanager.home_dir" => {
            let home = backend.home_dir().await?;
            Ok(serde_json::Value::String(home))
        }
        "filemanager.list_dir" => {
            let path = params["path"].as_str().unwrap_or("/").to_owned();
            let result = backend.list_dir(&path).await?;
            if result.truncated {
                tracing::warn!(
                    "listing '{path}' truncated to {LIST_DIR_CAP} entries (directory has more)"
                );
            }
            serde_json::to_value(result).map_err(|e| PluginError::Backend(e.to_string()))
        }
        "filemanager.start_transfer" => {
            // Kick off an upload or download as a background task *inside the
            // plugin process*. Returns immediately; the frontend polls
            // `last_transfer_stats` for progress and can `cancel_last_transfer`.
            // The job lives on the connection, so it survives the frontend
            // workspace unmounting on a connection switch.
            let req: StartTransfer = serde_json::from_value(params)
                .map_err(|e| PluginError::Config(format!("invalid params: {e}")))?;
            if req.items.is_empty() {
                return Err(PluginError::Config("no transfer items".into()));
            }

            let mut slot = job_slot.lock().await;
            if let Some(existing) = slot.as_ref() {
                if matches!(existing.phase(), JobPhase::Scanning | JobPhase::Running) {
                    return Err(PluginError::Backend("a transfer is already running".into()));
                }
            }
            let job = Arc::new(JobState::new());
            *slot = Some(job.clone());
            drop(slot);

            let kind = req.kind;
            let items = req.items;
            tracing::info!("starting {:?} transfer of {} item(s)", kind, items.len());
            tokio::spawn(async move {
                let result = run_transfer(&backend, kind, &items, &job).await;
                match result {
                    Ok(()) => {
                        // A cooperative cancel leaves the phase at Cancelled;
                        // don't overwrite it with Done.
                        if job.phase() == JobPhase::Running {
                            job.set_phase(JobPhase::Done);
                            tracing::info!("{kind:?} transfer completed");
                        } else {
                            tracing::info!("{kind:?} transfer cancelled");
                        }
                    }
                    Err(e) => {
                        tracing::warn!("{kind:?} transfer failed: {e}");
                        *job.error.lock().await = Some(e.to_string());
                        job.set_phase(JobPhase::Error);
                    }
                }
            });
            Ok(serde_json::Value::Null)
        }
        "filemanager.last_transfer_stats" => {
            // Snapshot the current/last job's progress, or null if none ran.
            let slot = job_slot.lock().await;
            match slot.as_ref() {
                None => Ok(serde_json::Value::Null),
                Some(job) => {
                    let stats = TransferStats {
                        phase: job.phase(),
                        done: job.done.load(Ordering::SeqCst),
                        total: job.total.load(Ordering::SeqCst),
                        current: job.current.lock().await.clone(),
                        error: job.error.lock().await.clone(),
                    };
                    serde_json::to_value(stats).map_err(|e| PluginError::Backend(e.to_string()))
                }
            }
        }
        "filemanager.cancel_last_transfer" => {
            // Set the cooperative cancel flag; the running task observes it
            // between files and transitions to Cancelled.
            let slot = job_slot.lock().await;
            if let Some(job) = slot.as_ref() {
                tracing::info!("cancelling in-progress transfer");
                job.cancel.store(true, Ordering::SeqCst);
            }
            Ok(serde_json::Value::Null)
        }
        "filemanager.delete" => {
            // Deletes the given `paths` recursively, in order; the first failure
            // aborts and surfaces, leaving the remaining paths untouched.
            let paths = delete_paths(&params)?;
            tracing::info!("deleting {} path(s) (recursive)", paths.len());
            for path in &paths {
                backend.delete(path).await?;
            }
            Ok(serde_json::Value::Null)
        }
        "filemanager.mkdir" => {
            let path = require_path(&params, "path")?;
            tracing::info!("creating directory '{path}'");
            backend.mkdir(&path).await?;
            Ok(serde_json::Value::Null)
        }
        "filemanager.rename" => {
            let from = params["from"]
                .as_str()
                .ok_or_else(|| PluginError::Config("from is required".into()))?
                .to_owned();
            let to = params["to"]
                .as_str()
                .ok_or_else(|| PluginError::Config("to is required".into()))?
                .to_owned();
            tracing::info!("renaming '{from}' -> '{to}'");
            backend.rename(&from, &to).await?;
            Ok(serde_json::Value::Null)
        }
        _ => Err(PluginError::Backend(format!("unknown op: {op}"))),
    }
}

// ── Background transfer driver ───────────────────────────────────────────────

/// Drive a whole transfer (every item) to completion, updating `job` as it goes.
/// Runs in a spawned task so it outlives the frontend workspace. Phases:
/// `Scanning` while building the file list, `Running` while copying, then the
/// caller flips to `Done`. Honors the cooperative `cancel` flag between files.
///
/// Backend-agnostic: the download source is walked via [`FileBackend::walk`]
/// while the upload source is the local filesystem (walked with [`walk_local`]).
pub async fn run_transfer<B: FileBackend>(
    backend: &B,
    kind: TransferKind,
    items: &[TransferItem],
    job: &JobState,
) -> Result<()> {
    // Phase 1 — scan: build the flat (src, dest) work list across all items and
    // learn the total file count up front, so the progress bar is accurate.
    job.set_phase(JobPhase::Scanning);
    // (src_path, dest_path, display_rel)
    let mut work: Vec<(String, String, String)> = Vec::new();
    for item in items {
        if job.cancel.load(Ordering::SeqCst) {
            job.set_phase(JobPhase::Cancelled);
            return Ok(());
        }
        match kind {
            TransferKind::Download => {
                let meta = backend.stat(&item.remote_path).await?;
                if meta.is_dir {
                    for f in backend.walk(&item.remote_path).await? {
                        let dest = join_rel(&item.local_path, &f.rel);
                        work.push((f.src, dest, f.rel));
                    }
                } else {
                    let name = file_name_of(&item.remote_path);
                    work.push((item.remote_path.clone(), item.local_path.clone(), name));
                }
            }
            TransferKind::Upload => {
                let meta = std::fs::metadata(&item.local_path)
                    .map_err(|e| PluginError::Backend(format!("stat {}: {e}", item.local_path)))?;
                if meta.is_dir() {
                    // Mirror the destination root so an uploaded folder exists
                    // even when empty; nested dirs are created lazily as each
                    // file's parent chain is ensured during copy.
                    backend.ensure_dirs(&item.remote_path).await?;
                    for f in walk_local(&item.local_path)? {
                        let dest = join_rel(&item.remote_path, &f.rel);
                        work.push((f.src, dest, f.rel));
                    }
                } else {
                    let name = file_name_of(&item.local_path);
                    work.push((item.local_path.clone(), item.remote_path.clone(), name));
                }
            }
        }
    }

    job.total.store(work.len() as u64, Ordering::SeqCst);
    job.set_phase(JobPhase::Running);

    // Phase 2 — copy each file. Cancel is checked both between files (here) and
    // mid-file (inside the backend copy methods, between chunks), so a large
    // in-progress file is aborted promptly rather than running to completion first.
    for (src, dest, rel) in work {
        if job.cancel.load(Ordering::SeqCst) {
            job.set_phase(JobPhase::Cancelled);
            return Ok(());
        }
        *job.current.lock().await = rel;
        let cancelled = match kind {
            TransferKind::Download => backend.download_file(&src, &dest, job).await?,
            TransferKind::Upload => {
                // Mirror the parent directory chain on the remote before writing.
                if let Some(parent) = dest.rsplit_once('/').map(|(p, _)| p) {
                    if !parent.is_empty() {
                        backend.ensure_dirs(parent).await?;
                    }
                }
                backend.upload_file(&src, &dest, job).await?
            }
        };
        if cancelled.is_some() {
            // The current file was aborted mid-copy and its partial destination
            // removed; stop here without counting it as done.
            job.set_phase(JobPhase::Cancelled);
            return Ok(());
        }
        job.done.fetch_add(1, Ordering::SeqCst);
    }
    Ok(())
}

// ── Shared helpers ───────────────────────────────────────────────────────────

/// The last path segment of a "/"-separated path (the file name).
pub fn file_name_of(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_owned()
}

/// Join a destination root with a relative path using "/". Remote paths always
/// use it; local paths on macOS/Linux accept it, and the `create_dir_all` /
/// `write` calls normalize either way.
pub fn join_rel(root: &str, rel: &str) -> String {
    if rel.is_empty() {
        root.to_owned()
    } else {
        format!("{root}/{rel}")
    }
}

/// Walk a local tree rooted at `root`, returning every file under it. Iterative
/// (async recursion would need boxing). Symlinks / special files are skipped.
pub fn walk_local(root: &str) -> Result<Vec<ScannedFile>> {
    let mut out = Vec::new();
    let mut stack: Vec<(std::path::PathBuf, String)> =
        vec![(std::path::PathBuf::from(root), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|e| PluginError::Backend(format!("read_dir {dir:?}: {e}")))?;
        for entry in entries {
            let entry = entry.map_err(|e| PluginError::Backend(format!("dir entry: {e}")))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let file_type = entry
                .file_type()
                .map_err(|e| PluginError::Backend(format!("file type {name}: {e}")))?;
            if file_type.is_dir() {
                stack.push((entry.path(), rel));
            } else if file_type.is_file() {
                out.push(ScannedFile {
                    src: entry.path().to_string_lossy().into_owned(),
                    rel,
                });
            }
        }
    }
    Ok(out)
}

/// Stream an `AsyncRead` source straight to `local` on disk, creating parent
/// directories as needed, in [`TRANSFER_CHUNK`] chunks. Checks `job.cancel`
/// between chunks; on cancel removes the partial local file and returns
/// `Ok(Some(Cancelled))`. The disk side of every backend's `download_file`.
pub async fn stream_to_local_file<R: AsyncRead + Unpin>(
    mut reader: R,
    local: &str,
    job: &JobState,
) -> Result<Option<Cancelled>> {
    let path = std::path::Path::new(local);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| PluginError::Backend(format!("create local dir {parent:?}: {e}")))?;
    }
    let mut out = tokio::fs::File::create(path)
        .await
        .map_err(|e| PluginError::Backend(format!("create {local}: {e}")))?;

    let mut buf = vec![0u8; TRANSFER_CHUNK];
    loop {
        if job.cancel.load(Ordering::SeqCst) {
            drop(out);
            let _ = tokio::fs::remove_file(path).await;
            return Ok(Some(Cancelled));
        }
        let n = reader
            .read(&mut buf)
            .await
            .map_err(|e| PluginError::Backend(format!("read {local}: {e}")))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])
            .await
            .map_err(|e| PluginError::Backend(format!("write {local}: {e}")))?;
    }
    out.flush()
        .await
        .map_err(|e| PluginError::Backend(format!("flush {local}: {e}")))?;
    Ok(None)
}

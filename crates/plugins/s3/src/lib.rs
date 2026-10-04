use async_trait::async_trait;
use aws_config::{BehaviorVersion, Region};
use aws_sdk_s3::config::Credentials;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart, Delete, ObjectIdentifier};
use aws_sdk_s3::Client;
use polyclient_core::{
    cfg_secret, require_str, ConfigField, ConfigFieldType, Connection, ConnectionConfig, Plugin,
    PluginError, PluginInfo, PluginKind,
};
use polyclient_filemanager_common::{
    cap_entries, dispatch_filemanager, downcast_conn, Cancelled, FileBackend, FileEntry, JobState,
    ListDirResult, ScannedFile, Stat, LIST_DIR_CAP,
};
use polyclient_plugin_runtime::Dispatcher;
use serde_json::Value;
use std::any::Any;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

// S3 multipart uploads require every part except the last to be at least 5 MiB.
// We buffer the local file into parts of this size before issuing `upload_part`.
const MULTIPART_PART_SIZE: usize = 8 * 1024 * 1024;
// Files at or below this size are uploaded in a single `put_object` call; larger
// files go through a multipart upload (which is also where mid-file cancel lives).
const MULTIPART_THRESHOLD: u64 = 8 * 1024 * 1024;

// ── Backend handle ─────────────────────────────────────────────────────────--

/// A small cloneable handle implementing [`FileBackend`]. `Client` is internally
/// `Arc`-like (cheap to clone) but we keep it behind an `Arc` to share one client
/// across the connection and any spawned transfer task.
#[derive(Clone)]
pub struct S3Backend {
    client: Arc<Client>,
    bucket: String,
}

// ── Connection ───────────────────────────────────────────────────────────────

pub struct S3Connection {
    backend: S3Backend,
    // The single current/last transfer job for this connection (mirrors the SFTP
    // plugin). Lives as long as the connection so it survives the frontend
    // workspace unmounting on a connection switch.
    job: Mutex<Option<Arc<JobState>>>,
}

impl Connection for S3Connection {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

// ── Plugin ───────────────────────────────────────────────────────────────────

pub struct S3Plugin;

impl S3Plugin {
    pub fn new() -> Self {
        Self
    }
}

impl Default for S3Plugin {
    fn default() -> Self {
        Self::new()
    }
}

/// Build an S3 client from the connection config. Shared by `connect` and `test`.
async fn build_client(config: &ConnectionConfig) -> polyclient_core::Result<Client> {
    let region = config
        .get("region")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("us-east-1")
        .to_owned();
    let endpoint = config
        .get("custom_endpoint")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_owned());

    let secret_key = cfg_secret(config, "secret_key")?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| PluginError::Config("secret_key is required".into()))?;

    let static_credentials = Credentials::new(
        require_str(config, "access_key")?,
        secret_key,
        None,
        None,
        "POLYCLIENT",
    );

    let mut loader = aws_config::defaults(BehaviorVersion::latest())
        .credentials_provider(static_credentials)
        .region(Region::new(region));
    if let Some(endpoint) = &endpoint {
        loader = loader.endpoint_url(endpoint);
    }
    let sdk_config = loader.load().await;

    // For custom endpoints (MinIO, localstack, …) virtual-host-style addressing
    // (`bucket.host`) usually isn't available, so force path-style (`host/bucket`).
    let mut s3_config = aws_sdk_s3::config::Builder::from(&sdk_config);
    if endpoint.is_some() {
        s3_config = s3_config.force_path_style(true);
    }
    Ok(Client::from_conf(s3_config.build()))
}

#[async_trait]
impl Plugin for S3Plugin {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            id: "s3".to_string(),
            name: "AWS S3".to_string(),
            kind: PluginKind::FileManager,
            version: env!("CARGO_PKG_VERSION").into(),
            description: "Browse and manage files on an AWS S3 bucket.".to_string(),
            ui_module: None,
            protocol_version: polyclient_core::PROTOCOL_VERSION,
            config_schema: vec![
                ConfigField {
                    key: "access_key".to_string(),
                    label: "Access Key".to_string(),
                    field_type: ConfigFieldType::Text,
                    required: true,
                    default: None,
                    placeholder: None,
                    show_if: None,
                },
                ConfigField {
                    key: "secret_key".to_string(),
                    label: "Secret Key".to_string(),
                    field_type: ConfigFieldType::Password,
                    required: true,
                    default: None,
                    placeholder: None,
                    show_if: None,
                },
                ConfigField {
                    key: "bucket".to_string(),
                    label: "Bucket Name".to_string(),
                    field_type: ConfigFieldType::Text,
                    required: true,
                    default: None,
                    placeholder: None,
                    show_if: None,
                },
                ConfigField {
                    key: "region".to_string(),
                    label: "Region".to_string(),
                    field_type: ConfigFieldType::Text,
                    required: false,
                    default: Some(serde_json::json!("us-east-1")),
                    placeholder: Some("us-east-1".to_string()),
                    show_if: None,
                },
                ConfigField {
                    key: "custom_endpoint".to_string(),
                    label: "Custom Endpoint".to_string(),
                    field_type: ConfigFieldType::Text,
                    required: false,
                    default: None,
                    placeholder: Some("http://localhost:9000".to_string()),
                    show_if: None,
                },
            ],
        }
    }

    async fn connect(
        &self,
        config: ConnectionConfig,
    ) -> polyclient_core::Result<Arc<dyn Connection>> {
        let bucket = require_str(&config, "bucket")?.to_owned();
        let client = build_client(&config).await?;
        tracing::info!("connecting to s3 bucket '{bucket}'");
        // Verify the bucket is reachable with these credentials up front, so a
        // bad config surfaces at connect time rather than on the first list.
        client
            .head_bucket()
            .bucket(&bucket)
            .send()
            .await
            .map_err(|e| {
                PluginError::Connection(format!("cannot reach bucket '{bucket}': {}", svc_err(&e)))
            })?;
        tracing::info!("s3 bucket '{bucket}' ready");

        Ok(Arc::new(S3Connection {
            backend: S3Backend {
                client: Arc::new(client),
                bucket,
            },
            job: Mutex::new(None),
        }))
    }

    async fn test(&self, config: ConnectionConfig) -> polyclient_core::Result<()> {
        let bucket = require_str(&config, "bucket")?.to_owned();
        let client = build_client(&config).await?;
        client
            .head_bucket()
            .bucket(&bucket)
            .send()
            .await
            .map_err(|e| {
                PluginError::Connection(format!("cannot reach bucket '{bucket}': {}", svc_err(&e)))
            })?;
        Ok(())
    }
}

// ── Dispatcher ───────────────────────────────────────────────────────────────

pub struct S3Dispatcher;

#[async_trait]
impl Dispatcher for S3Dispatcher {
    async fn dispatch(
        &self,
        op: &str,
        params: Value,
        conn: Arc<dyn Connection>,
    ) -> polyclient_core::Result<Value> {
        let conn = downcast_conn::<S3Connection>(&conn)?;
        dispatch_filemanager(op, params, conn.backend.clone(), &conn.job).await
    }
}

// ── Key/prefix helpers ─────────────────────────────────────────────────────--
//
// S3 has no real directories: keys are flat and folders are emulated via the "/"
// delimiter. Keys never start with "/", so the frontend's paths are normalized
// here. A "directory" is a key prefix ending in "/" (root = ""); a file is a bare
// key. `mkdir` writes a zero-byte "prefix/" marker so an empty folder is visible.

/// Normalize a path coming from the frontend into a bare S3 key: trim any leading
/// or trailing slashes. Root ("" or "/") becomes "".
fn to_key(path: &str) -> String {
    path.trim_matches('/').to_owned()
}

/// Turn a directory path into a listing prefix: "" for root, else "key/".
fn to_prefix(path: &str) -> String {
    let key = to_key(path);
    if key.is_empty() {
        String::new()
    } else {
        format!("{key}/")
    }
}

/// The last "/"-separated segment of a key (used as the display name).
fn segment_name(key: &str) -> String {
    key.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(key)
        .to_owned()
}

/// Stringify an AWS SDK error, preferring the service message when present.
fn svc_err<E: std::error::Error>(err: &aws_sdk_s3::error::SdkError<E>) -> String {
    match err {
        aws_sdk_s3::error::SdkError::ServiceError(se) => se.err().to_string(),
        other => other.to_string(),
    }
}

// ── FileBackend impl ─────────────────────────────────────────────────────────

#[async_trait]
impl FileBackend for S3Backend {
    async fn home_dir(&self) -> polyclient_core::Result<String> {
        // The bucket root is trivially writable; "" lists the top level.
        Ok(String::new())
    }

    async fn list_dir(&self, path: &str) -> polyclient_core::Result<ListDirResult> {
        let prefix = to_prefix(path);
        let mut entries: Vec<FileEntry> = Vec::new();
        let mut continuation: Option<String> = None;
        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(&prefix)
                .delimiter("/");
            if let Some(token) = &continuation {
                req = req.continuation_token(token);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| PluginError::Backend(format!("list_objects_v2: {}", svc_err(&e))))?;

            // CommonPrefixes are the emulated sub-folders.
            for cp in resp.common_prefixes() {
                if let Some(p) = cp.prefix() {
                    let key = p.trim_end_matches('/');
                    entries.push(FileEntry {
                        name: segment_name(key),
                        path: key.to_owned(),
                        is_dir: true,
                        size: 0,
                        modified: 0,
                        permissions: 0,
                    });
                }
            }
            // Objects are files. Skip the "prefix/" folder marker and any key that
            // ends in "/" (defensive — markers shouldn't otherwise appear here).
            for obj in resp.contents() {
                let key = match obj.key() {
                    Some(k) => k,
                    None => continue,
                };
                if key == prefix || key.ends_with('/') {
                    continue;
                }
                entries.push(FileEntry {
                    name: segment_name(key),
                    path: key.to_owned(),
                    is_dir: false,
                    size: obj.size().unwrap_or(0).max(0) as u64,
                    modified: obj.last_modified().map(|t| t.secs()).unwrap_or(0),
                    permissions: 0,
                });
            }

            // Stop paginating once we hold more than the cap: a directory with
            // hundreds of thousands of objects would otherwise drive an unbounded
            // number of list_objects_v2 calls just to throw the extras away.
            // One entry over the cap is enough for `cap_entries` to flag it.
            if entries.len() > LIST_DIR_CAP {
                break;
            }
            match resp.next_continuation_token() {
                Some(token) => continuation = Some(token.to_owned()),
                None => break,
            }
        }
        Ok(cap_entries(entries))
    }

    async fn stat(&self, path: &str) -> polyclient_core::Result<Stat> {
        let key = to_key(path);
        if key.is_empty() {
            // Bucket root is always a directory.
            return Ok(Stat {
                exists: true,
                is_dir: true,
            });
        }
        // Prefer an exact object (a file) over a folder of the same name.
        if self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
            .is_ok()
        {
            return Ok(Stat {
                exists: true,
                is_dir: false,
            });
        }
        // Otherwise, if anything lives under "key/", it's a folder.
        let prefix = format!("{key}/");
        let resp = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(&prefix)
            .max_keys(1)
            .send()
            .await
            .map_err(|e| PluginError::Backend(format!("stat {key}: {}", svc_err(&e))))?;
        let is_dir = resp.key_count().unwrap_or(0) > 0 || !resp.contents().is_empty();
        Ok(Stat {
            exists: is_dir,
            is_dir,
        })
    }

    async fn mkdir(&self, path: &str) -> polyclient_core::Result<()> {
        let key = to_key(path);
        if key.is_empty() {
            return Ok(());
        }
        // A zero-byte "key/" marker object makes the empty folder visible.
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(format!("{key}/"))
            .body(ByteStream::from_static(b""))
            .send()
            .await
            .map_err(|e| PluginError::Backend(format!("mkdir {key}: {}", svc_err(&e))))?;
        Ok(())
    }

    async fn rename(&self, from: &str, to: &str) -> polyclient_core::Result<()> {
        // S3 has no native move: copy then delete. Non-atomic, and O(n) round
        // trips for a folder (one copy+delete per object under it).
        let from_key = to_key(from);
        let to_key_s = to_key(to);
        let st = self.stat(from).await?;
        if st.is_dir {
            let to_prefix = format!("{to_key_s}/");
            for sf in self.walk(from).await? {
                // sf.src is the full source key; sf.rel is relative to "from/".
                let dest = format!("{to_prefix}{}", sf.rel);
                self.copy_key(&sf.src, &dest).await?;
            }
            // Ensure the destination folder is visible even if it was empty, then
            // delete the whole source tree (including its marker).
            self.mkdir(to).await?;
            self.delete(from).await?;
        } else {
            self.copy_key(&from_key, &to_key_s).await?;
            self.delete_object(&from_key).await?;
        }
        Ok(())
    }

    async fn delete(&self, path: &str) -> polyclient_core::Result<()> {
        let key = to_key(path);
        if key.is_empty() {
            return Err(PluginError::Backend(
                "refusing to delete bucket root".into(),
            ));
        }
        let st = self.stat(path).await?;
        if !st.is_dir {
            return self.delete_object(&key).await;
        }
        // Folder: delete every object under "key/" (plus the marker) in batches of
        // up to 1000 (the `delete_objects` limit).
        let prefix = format!("{key}/");
        let mut continuation: Option<String> = None;
        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(&prefix);
            if let Some(token) = &continuation {
                req = req.continuation_token(token);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| PluginError::Backend(format!("list for delete: {}", svc_err(&e))))?;

            let ids: Vec<ObjectIdentifier> = resp
                .contents()
                .iter()
                .filter_map(|o| o.key())
                .map(|k| {
                    ObjectIdentifier::builder()
                        .key(k)
                        .build()
                        .map_err(|e| PluginError::Backend(format!("object id: {e}")))
                })
                .collect::<polyclient_core::Result<Vec<_>>>()?;
            if !ids.is_empty() {
                let delete = Delete::builder()
                    .set_objects(Some(ids))
                    .build()
                    .map_err(|e| PluginError::Backend(format!("delete batch: {e}")))?;
                self.client
                    .delete_objects()
                    .bucket(&self.bucket)
                    .delete(delete)
                    .send()
                    .await
                    .map_err(|e| {
                        PluginError::Backend(format!("delete_objects: {}", svc_err(&e)))
                    })?;
            }

            match resp.next_continuation_token() {
                Some(token) => continuation = Some(token.to_owned()),
                None => break,
            }
        }
        Ok(())
    }

    async fn walk(&self, root: &str) -> polyclient_core::Result<Vec<ScannedFile>> {
        // List every object under the prefix (no delimiter = fully recursive).
        let prefix = to_prefix(root);
        let mut out = Vec::new();
        let mut continuation: Option<String> = None;
        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(&prefix);
            if let Some(token) = &continuation {
                req = req.continuation_token(token);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| PluginError::Backend(format!("walk list: {}", svc_err(&e))))?;
            for obj in resp.contents() {
                let key = match obj.key() {
                    Some(k) => k,
                    None => continue,
                };
                // Skip folder markers (keys ending in "/", including the prefix).
                if key.ends_with('/') {
                    continue;
                }
                let rel = key.strip_prefix(&prefix).unwrap_or(key).to_owned();
                if rel.is_empty() {
                    continue;
                }
                out.push(ScannedFile {
                    src: key.to_owned(),
                    rel,
                });
            }
            match resp.next_continuation_token() {
                Some(token) => continuation = Some(token.to_owned()),
                None => break,
            }
        }
        Ok(out)
    }

    async fn ensure_dirs(&self, path: &str) -> polyclient_core::Result<()> {
        // Object stores need no parent chain; a single folder marker keeps an
        // uploaded empty folder visible (parity with the SFTP plugin).
        self.mkdir(path).await
    }

    async fn download_file(
        &self,
        remote: &str,
        local: &str,
        job: &JobState,
    ) -> polyclient_core::Result<Option<Cancelled>> {
        let key = to_key(remote);
        let resp = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
            .map_err(|e| PluginError::Backend(format!("get_object {key}: {}", svc_err(&e))))?;

        let local_path = std::path::Path::new(local);
        if let Some(parent) = local_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| PluginError::Backend(format!("create local dir {parent:?}: {e}")))?;
        }
        let mut out = tokio::fs::File::create(local_path)
            .await
            .map_err(|e| PluginError::Backend(format!("create {local}: {e}")))?;

        // Stream the body chunk by chunk so a large object isn't buffered whole
        // and the cooperative cancel flag is observed mid-file.
        let mut body = resp.body;
        loop {
            if job.cancel.load(Ordering::SeqCst) {
                drop(out);
                let _ = tokio::fs::remove_file(local_path).await;
                return Ok(Some(Cancelled));
            }
            match body
                .try_next()
                .await
                .map_err(|e| PluginError::Backend(format!("read {key}: {e}")))?
            {
                Some(chunk) => {
                    out.write_all(&chunk)
                        .await
                        .map_err(|e| PluginError::Backend(format!("write {local}: {e}")))?;
                }
                None => break,
            }
        }
        out.flush()
            .await
            .map_err(|e| PluginError::Backend(format!("flush {local}: {e}")))?;
        Ok(None)
    }

    async fn upload_file(
        &self,
        local: &str,
        remote: &str,
        job: &JobState,
    ) -> polyclient_core::Result<Option<Cancelled>> {
        let key = to_key(remote);
        let size = std::fs::metadata(local)
            .map_err(|e| PluginError::Backend(format!("stat {local}: {e}")))?
            .len();

        if size <= MULTIPART_THRESHOLD {
            // Single-shot upload. NOTE: a small `put_object` cannot be cancelled
            // mid-file — only the multipart path below checks cancel between parts.
            if job.cancel.load(Ordering::SeqCst) {
                return Ok(Some(Cancelled));
            }
            let body = ByteStream::from_path(local)
                .await
                .map_err(|e| PluginError::Backend(format!("read {local}: {e}")))?;
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(&key)
                .body(body)
                .send()
                .await
                .map_err(|e| PluginError::Backend(format!("put_object {key}: {}", svc_err(&e))))?;
            return Ok(None);
        }

        self.upload_multipart(local, &key, job).await
    }
}

// ── S3-internal helpers ────────────────────────────────────────────────────--

impl S3Backend {
    /// Copy one object within the bucket. `CopySource` must be URL-form
    /// `bucket/key`; the SDK handles encoding of the value we pass.
    async fn copy_key(&self, from: &str, to: &str) -> polyclient_core::Result<()> {
        let source = format!("{}/{}", self.bucket, from);
        self.client
            .copy_object()
            .bucket(&self.bucket)
            .copy_source(source)
            .key(to)
            .send()
            .await
            .map_err(|e| PluginError::Backend(format!("copy {from} -> {to}: {}", svc_err(&e))))?;
        Ok(())
    }

    async fn delete_object(&self, key: &str) -> polyclient_core::Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| PluginError::Backend(format!("delete {key}: {}", svc_err(&e))))?;
        Ok(())
    }

    /// Upload a large local file via a multipart upload, buffering the file into
    /// parts of at least 5 MiB. Checks `job.cancel` between parts; on cancel the
    /// multipart upload is aborted so no partial object is left behind.
    async fn upload_multipart(
        &self,
        local: &str,
        key: &str,
        job: &JobState,
    ) -> polyclient_core::Result<Option<Cancelled>> {
        let created = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| {
                PluginError::Backend(format!("create_multipart_upload {key}: {}", svc_err(&e)))
            })?;
        let upload_id = created
            .upload_id()
            .ok_or_else(|| PluginError::Backend("missing upload id".into()))?
            .to_owned();

        let result = self.multipart_parts(local, key, &upload_id, job).await;

        match result {
            Ok(Some(parts)) => {
                let completed = CompletedMultipartUpload::builder()
                    .set_parts(Some(parts))
                    .build();
                self.client
                    .complete_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(&upload_id)
                    .multipart_upload(completed)
                    .send()
                    .await
                    .map_err(|e| {
                        PluginError::Backend(format!("complete_multipart {key}: {}", svc_err(&e)))
                    })?;
                Ok(None)
            }
            // Cancelled or errored: abort the upload to avoid orphaned parts.
            other => {
                let _ = self
                    .client
                    .abort_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(&upload_id)
                    .send()
                    .await;
                match other {
                    Ok(None) => Ok(Some(Cancelled)),
                    Ok(Some(_)) => unreachable!(),
                    Err(e) => Err(e),
                }
            }
        }
    }

    /// Read the local file in chunks of at least `MULTIPART_PART_SIZE` and upload
    /// each as a part. Returns `Ok(Some(parts))` on success, `Ok(None)` if cancelled.
    async fn multipart_parts(
        &self,
        local: &str,
        key: &str,
        upload_id: &str,
        job: &JobState,
    ) -> polyclient_core::Result<Option<Vec<CompletedPart>>> {
        use tokio::io::AsyncReadExt;

        let mut input = tokio::fs::File::open(local)
            .await
            .map_err(|e| PluginError::Backend(format!("open {local}: {e}")))?;
        let mut parts: Vec<CompletedPart> = Vec::new();
        let mut part_number = 1i32;
        let mut buf = vec![0u8; MULTIPART_PART_SIZE];

        loop {
            if job.cancel.load(Ordering::SeqCst) {
                return Ok(None);
            }
            // Fill a full part (read may return short reads; accumulate).
            let mut filled = 0usize;
            while filled < buf.len() {
                let n = input
                    .read(&mut buf[filled..])
                    .await
                    .map_err(|e| PluginError::Backend(format!("read {local}: {e}")))?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                break;
            }
            let body = ByteStream::from(buf[..filled].to_vec());
            let resp = self
                .client
                .upload_part()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(upload_id)
                .part_number(part_number)
                .body(body)
                .send()
                .await
                .map_err(|e| {
                    PluginError::Backend(format!("upload_part {part_number}: {}", svc_err(&e)))
                })?;
            parts.push(
                CompletedPart::builder()
                    .part_number(part_number)
                    .set_e_tag(resp.e_tag().map(|s| s.to_owned()))
                    .build(),
            );
            part_number += 1;
            if filled < buf.len() {
                // Last (short) part.
                break;
            }
        }
        Ok(Some(parts))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polyclient_core::SecretField;

    /// `test` with `bucket` and `access_key` present but `secret_key` missing
    /// should fail config validation before any network call, surfacing the
    /// missing required field.
    #[tokio::test]
    async fn test_connection_invalid_config_missing_secret_key() {
        let mut config = ConnectionConfig::new();
        config.insert("bucket".to_string(), serde_json::json!("my-bucket"));
        config.insert("access_key".to_string(), serde_json::json!("AKIA..."));

        let err = S3Plugin::new()
            .test(config)
            .await
            .expect_err("missing secret_key should be rejected");

        match err {
            PluginError::Config(msg) => assert_eq!(msg, "secret_key is required"),
            other => panic!("expected PluginError::Config, got {other:?}"),
        }
    }

    /// The frontend stores `Password`-kind fields as a wrapped `SecretField`
    /// (`{type, value}`), not a bare string. `build_client` must read it with
    /// `cfg_secret`; reading it with `require_str` would (wrongly) report
    /// `secret_key is required` for a filled-in secret. This guards that
    /// regression: a wrapped secret must build a client without a config error.
    #[tokio::test]
    async fn build_client_accepts_wrapped_secret_key() {
        let mut config = ConnectionConfig::new();
        config.insert("bucket".to_string(), serde_json::json!("my-bucket"));
        config.insert("access_key".to_string(), serde_json::json!("AKIA..."));
        config.insert(
            "secret_key".to_string(),
            serde_json::to_value(SecretField::plaintext("super-secret")).unwrap(),
        );
        // Point at a never-contacted endpoint so no real network call is made;
        // build_client only constructs the client, it doesn't reach S3.
        config.insert(
            "custom_endpoint".to_string(),
            serde_json::json!("http://127.0.0.1:1"),
        );

        build_client(&config)
            .await
            .expect("wrapped secret_key should be accepted");
    }
}

//! Core abstractions shared by all polyclient plugins.
//!
//! A plugin is anything that lets the user connect to an external system
//! (database, queue, cache, ...). Plugins declare a `kind` so the UI knows
//! what surface to render, and provide a serializable config schema so the
//! UI can build a connection form generically.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use uuid::Uuid;

pub mod protocol;

pub use protocol::PROTOCOL_VERSION;

#[derive(Debug, Error)]
pub enum PluginError {
    #[error("connection failed: {0}")]
    Connection(String),
    #[error("invalid config: {0}")]
    Config(String),
    #[error("operation not supported by plugin")]
    Unsupported,
    #[error("plugin not found: {0}")]
    NotFound(String),
    #[error("backend error: {0}")]
    Backend(String),
}

pub type Result<T> = std::result::Result<T, PluginError>;

/// What kind of system a plugin talks to. The UI uses this to pick which
/// shared component renders the workspace for the connection.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum PluginKind {
    Rdbms,
    Document,
    Rabbitmq,
    Cli,
    FileManager,
    Http,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigField {
    pub key: String,
    pub label: String,
    #[serde(rename = "type")]
    pub field_type: ConfigFieldType,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<serde_json::Value>,
    #[serde(default)]
    pub placeholder: Option<String>,
    /// When set, the UI only shows this field while the field named
    /// `show_if.field` holds the value `show_if.equals`. Lets a plugin offer
    /// e.g. a "connection string" vs "individual settings" mode within one form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_if: Option<ShowIf>,
}

/// Conditional-visibility rule for a [`ConfigField`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShowIf {
    /// Key of the controlling field.
    pub field: String,
    /// The controlling field's value that makes this field visible.
    pub equals: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "kind")]
pub enum ConfigFieldType {
    Text,
    Password,
    Number,
    Boolean,
    Select {
        options: Vec<String>,
    },
    /// A file-path field: the UI shows a text input + "Browse…" button that
    /// opens a native file picker. The stored value is the absolute path string.
    FilePath,
    /// A string-to-string map editor (e.g. HTTP environment variables).
    KeyValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub kind: PluginKind,
    pub version: String,
    pub description: String,
    pub config_schema: Vec<ConfigField>,
    /// UI module hint (`rdbms`, `document`, `rabbitmq`, or plugin id).
    #[serde(default)]
    pub ui_module: Option<String>,
    /// Wire-protocol version the plugin speaks. Set by the plugin runtime when
    /// answering `describe`; the host refuses plugins whose version it doesn't
    /// support. Defaults so manifests written before this field deserialize.
    #[serde(default = "protocol::default_protocol_version")]
    pub protocol_version: u32,
}

pub type ConnectionConfig = HashMap<String, serde_json::Value>;

/// A credential stored in a [`ConnectionConfig`]. Every `Password`-kind config
/// field carries one of these instead of a bare string, so a secret is
/// self-describing about *how* it is stored.
///
/// Only `PlainText` exists today (the value is stored verbatim, in plaintext —
/// the same as before this type existed). The `type` tag is the stable wire
/// discriminant, so future storage strategies (OS keychain, env var, encrypted
/// blob, …) can be added as new variants without changing the field shape.
///
/// Wire/disk form: `{ "type": "PLAIN_TEXT", "value": "hunter2" }`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "value")]
pub enum SecretField {
    #[serde(rename = "PLAIN_TEXT")]
    PlainText(String),
}

impl SecretField {
    /// A plaintext secret (the only kind supported today).
    pub fn plaintext(value: impl Into<String>) -> Self {
        Self::PlainText(value.into())
    }

    /// The resolved plaintext secret. For `PlainText` this is the stored value.
    /// Future variants resolve their value here (e.g. read from a keychain).
    pub fn reveal(&self) -> &str {
        match self {
            SecretField::PlainText(v) => v,
        }
    }
}

pub fn require_str<'a>(cfg: &'a ConnectionConfig, key: &str) -> Result<&'a str> {
    cfg.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| PluginError::Config(format!("{key} is required")))
}

/// Read a [`SecretField`] config value as plaintext. Returns `None` when the
/// key is absent; errors if present but not a valid `SecretField`.
pub fn cfg_secret(cfg: &ConnectionConfig, key: &str) -> Result<Option<String>> {
    match cfg.get(key) {
        None => Ok(None),
        Some(v) => {
            let secret: SecretField = serde_json::from_value(v.clone())
                .map_err(|e| PluginError::Config(format!("field {key}: {e}")))?;
            Ok(Some(secret.reveal().to_owned()))
        }
    }
}

/// Render the free-form `options` key-value map (the `ConfigFieldType::KeyValue`
/// field a plugin can expose in its config schema, conventionally under the
/// `options` key) as a URL query-string suffix, e.g.
/// `connect_timeout=10&application_name=polyclient`, and append it to `url`. Lets any
/// parameter the underlying driver/server's URL parser understands pass
/// straight through without the plugin needing to know about it individually.
pub fn append_options(mut url: String, cfg: &ConnectionConfig) -> String {
    let Some(map) = cfg.get("options").and_then(|v| v.as_object()) else {
        return url;
    };
    for (k, v) in map {
        let Some(v) = v.as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str(&url_encode(k));
        url.push('=');
        url.push_str(&url_encode(v));
    }
    url
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// How a `cli`-kind plugin tells the host to launch its terminal process.
///
/// The host knows nothing about ssh/telnet/etc.; it asks the plugin (via the
/// `cli.spawn_spec` op) for this spec, then runs `program` with `args`/`env` in
/// a PTY and streams the I/O to the frontend. Keeping command construction in
/// the plugin means all backend-specific knowledge (flags, auth handling,
/// prompt detection) stays out of the generic host.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PtySpawnSpec {
    /// Executable to run (e.g. `"ssh"`).
    pub program: String,
    /// Arguments, in order.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables to set for the child (`(key, value)`).
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Optional one-shot auto-answer to an interactive prompt (e.g. feeding a
    /// saved password when the server prints `password:`). The host watches the
    /// PTY output for `pattern` and, on the first match, writes `send` followed
    /// by a newline — then stops watching.
    #[serde(default)]
    pub prompt_response: Option<PtyPromptResponse>,
}

/// A one-shot prompt auto-answer for a [`PtySpawnSpec`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PtyPromptResponse {
    /// Case-insensitive substring that identifies the prompt (e.g. `"password:"`).
    pub pattern: String,
    /// Text to send (a trailing newline is added by the host).
    pub send: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ConnectionId(pub Uuid);

impl ConnectionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ConnectionId {
    fn default() -> Self {
        Self::new()
    }
}

/// Marker trait for an open connection. Plugins downcast via `as_any`.
pub trait Connection: Send + Sync + std::any::Any {
    fn as_any(&self) -> &dyn std::any::Any;
}

#[async_trait]
pub trait Plugin: Send + Sync {
    fn info(&self) -> PluginInfo;

    async fn connect(&self, config: ConnectionConfig) -> Result<Arc<dyn Connection>>;

    async fn test(&self, config: ConnectionConfig) -> Result<()> {
        let _ = self.connect(config).await?;
        Ok(())
    }
}

pub mod test_utils {
    use crate::ConnectionConfig;
    use serde::Deserialize;
    use std::path::Path;

    /// A saved connection profile as persisted by the host at
    /// `<app_dir>/connections/<plugin_id>/connections.json`. Mirrors the host's
    /// `SavedConnection` (camelCase on the wire); only the fields this test
    /// needs are declared.
    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct SavedConnection {
        id: String,
        #[allow(dead_code)]
        name: String,
        plugin_id: String,
        config: ConnectionConfig,
    }

    /// Scan `<app_dir>/connections/*/connections.json` and return the config of
    /// the saved connection whose id matches `connection_id`.
    pub fn find_connection_config(app_dir: &Path, connection_id: &str) -> ConnectionConfig {
        let connections_dir = app_dir.join("connections");
        let entries = std::fs::read_dir(&connections_dir).unwrap_or_else(|e| {
            panic!(
                "cannot read connections dir {}: {e}",
                connections_dir.display()
            )
        });

        for entry in entries.flatten() {
            let file = entry.path().join("connections.json");
            let bytes = match std::fs::read(&file) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => panic!("cannot read {}: {e}", file.display()),
            };
            let conns: Vec<SavedConnection> = serde_json::from_slice(&bytes)
                .unwrap_or_else(|e| panic!("invalid connections file {}: {e}", file.display()));
            if let Some(conn) = conns.into_iter().find(|c| c.id == connection_id) {
                eprintln!(
                    "found connection '{}' (plugin '{}') in {}",
                    connection_id,
                    conn.plugin_id,
                    file.display()
                );
                return conn.config;
            }
        }

        panic!(
            "no saved connection with id '{connection_id}' under {}",
            connections_dir.display()
        );
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn secret_field_serializes_with_type_and_value() {
        let secret = SecretField::plaintext("hunter2");
        assert_eq!(
            serde_json::to_value(&secret).unwrap(),
            json!({ "type": "PLAIN_TEXT", "value": "hunter2" }),
        );
    }

    #[test]
    fn cfg_secret_round_trips_a_plain_text_secret() {
        let mut cfg = ConnectionConfig::new();
        cfg.insert(
            "password".into(),
            serde_json::to_value(SecretField::plaintext("hunter2")).unwrap(),
        );
        assert_eq!(
            cfg_secret(&cfg, "password").unwrap().as_deref(),
            Some("hunter2")
        );
    }

    #[test]
    fn cfg_secret_is_none_when_absent() {
        let cfg = ConnectionConfig::new();
        assert_eq!(cfg_secret(&cfg, "password").unwrap(), None);
    }

    #[test]
    fn cfg_secret_rejects_a_bare_string() {
        let mut cfg = ConnectionConfig::new();
        cfg.insert("password".into(), json!("hunter2"));
        assert!(cfg_secret(&cfg, "password").is_err());
    }
}

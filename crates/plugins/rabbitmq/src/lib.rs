//! RabbitMQ plugin backed by the HTTP **Management API** (the same data source
//! the rabbitmq_management web UI uses), not AMQP. The broker must have the
//! `rabbitmq_management` plugin enabled (listening on port 15672 by default).
//!
//! The connection handle holds a [`reqwest::Client`] plus the management base
//! URL and HTTP Basic credentials. Live broker objects never cross the pipe —
//! every capability op is a single authenticated HTTP request whose JSON result
//! deserializes straight into the structs the frontend mirrors.

use async_trait::async_trait;
use polyclient_core::{
    cfg_secret, ConfigField, ConfigFieldType, Connection, ConnectionConfig, Plugin, PluginError,
    PluginInfo, PluginKind, Result,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub struct RabbitMqPlugin;

impl RabbitMqPlugin {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RabbitMqPlugin {
    fn default() -> Self {
        Self::new()
    }
}

/// A live management-API session: an HTTP client, the base URL (e.g.
/// `http://localhost:15672`, no trailing slash), Basic-auth credentials, and
/// the default vhost used when a request doesn't name its own.
pub struct RabbitMqConnection {
    client: reqwest::Client,
    base: String,
    user: String,
    password: String,
    pub default_vhost: String,
}

impl Connection for RabbitMqConnection {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// --- Wire structs (deserialized from the API, serialized back to the host) ---
// Fields are a deliberate subset of the rich Management API payloads; `default`
// keeps us forward/backward compatible across broker versions.

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ObjectTotals {
    #[serde(default)]
    pub queues: u64,
    #[serde(default)]
    pub exchanges: u64,
    #[serde(default)]
    pub connections: u64,
    #[serde(default)]
    pub channels: u64,
    #[serde(default)]
    pub consumers: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueueTotals {
    #[serde(default)]
    pub messages: u64,
    #[serde(default)]
    pub messages_ready: u64,
    #[serde(default)]
    pub messages_unacknowledged: u64,
}

/// The `*_details` sub-objects in the API carry a `rate` (per second). We keep
/// only the rate and flatten it on serialize-out.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RateDetails {
    #[serde(default)]
    pub rate: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageStats {
    #[serde(default)]
    pub publish_details: RateDetails,
    #[serde(default)]
    pub deliver_get_details: RateDetails,
    #[serde(default)]
    pub ack_details: RateDetails,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Overview {
    #[serde(default)]
    pub rabbitmq_version: String,
    #[serde(default)]
    pub erlang_version: String,
    #[serde(default)]
    pub cluster_name: String,
    #[serde(default)]
    pub node: String,
    #[serde(default)]
    pub object_totals: ObjectTotals,
    #[serde(default)]
    pub queue_totals: QueueTotals,
    #[serde(default)]
    pub message_stats: MessageStats,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Queue {
    pub name: String,
    #[serde(default)]
    pub vhost: String,
    #[serde(default)]
    pub state: String,
    #[serde(default, rename = "type")]
    pub queue_type: String,
    #[serde(default)]
    pub durable: bool,
    #[serde(default)]
    pub auto_delete: bool,
    #[serde(default)]
    pub messages: u64,
    #[serde(default)]
    pub messages_ready: u64,
    #[serde(default)]
    pub messages_unacknowledged: u64,
    #[serde(default)]
    pub consumers: u64,
    #[serde(default)]
    pub memory: u64,
    #[serde(default)]
    pub message_stats: MessageStats,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Exchange {
    /// The default exchange comes back with an empty name.
    pub name: String,
    #[serde(default)]
    pub vhost: String,
    #[serde(default, rename = "type")]
    pub exchange_type: String,
    #[serde(default)]
    pub durable: bool,
    #[serde(default)]
    pub auto_delete: bool,
    #[serde(default)]
    pub internal: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConnectionRow {
    pub name: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub channels: u64,
    #[serde(default)]
    pub protocol: String,
    #[serde(default)]
    pub peer_host: String,
    #[serde(default)]
    pub peer_port: u64,
    #[serde(default)]
    pub vhost: String,
    #[serde(default)]
    pub ssl: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChannelRow {
    pub name: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub vhost: String,
    #[serde(default)]
    pub number: u64,
    #[serde(default)]
    pub consumer_count: u64,
    #[serde(default)]
    pub messages_unacknowledged: u64,
    #[serde(default)]
    pub prefetch_count: u64,
}

/// One message returned by the `get` endpoint (peek/consume).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GotMessage {
    #[serde(default)]
    pub payload: String,
    #[serde(default)]
    pub payload_bytes: u64,
    #[serde(default)]
    pub redelivered: bool,
    #[serde(default)]
    pub routing_key: String,
    #[serde(default)]
    pub exchange: String,
    /// Messages still in the queue after this fetch.
    #[serde(default)]
    pub message_count: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PublishResult {
    /// Whether the broker routed the message to at least one queue.
    #[serde(default)]
    pub routed: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PurgeResult {
    pub queue: String,
}

fn cfg_str(cfg: &ConnectionConfig, key: &str) -> Result<String> {
    cfg.get(key)
        .and_then(|v| v.as_str().map(str::to_string))
        .ok_or_else(|| PluginError::Config(format!("missing field: {key}")))
}

fn cfg_str_opt(cfg: &ConnectionConfig, key: &str) -> Option<String> {
    cfg.get(key).and_then(|v| v.as_str().map(str::to_string))
}

fn cfg_u16(cfg: &ConnectionConfig, key: &str, default: u16) -> u16 {
    cfg.get(key)
        .and_then(|v| v.as_u64())
        .map(|n| n as u16)
        .unwrap_or(default)
}

fn cfg_bool(cfg: &ConnectionConfig, key: &str) -> bool {
    cfg.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Percent-encode a URI path segment (RFC 3986 unreserved set kept). The vhost
/// `/` becomes `%2F`, which is how the Management API names the default vhost.
fn urlencode(s: &str) -> String {
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

fn downcast(conn: &Arc<dyn Connection>) -> Result<&RabbitMqConnection> {
    conn.as_any()
        .downcast_ref::<RabbitMqConnection>()
        .ok_or_else(|| PluginError::Backend("connection type mismatch".into()))
}

#[async_trait]
impl Plugin for RabbitMqPlugin {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            id: "rabbitmq".into(),
            name: "RabbitMQ".into(),
            kind: PluginKind::Rabbitmq,
            version: env!("CARGO_PKG_VERSION").into(),
            description:
                "Manage a RabbitMQ broker via its HTTP Management API (rabbitmq_management plugin)."
                    .into(),
            ui_module: Some("rabbitmq".into()),
            protocol_version: polyclient_core::PROTOCOL_VERSION,
            config_schema: vec![
                ConfigField {
                    key: "host".into(),
                    label: "Host".into(),
                    field_type: ConfigFieldType::Text,
                    required: true,
                    default: Some(serde_json::json!("localhost")),
                    placeholder: Some("localhost".into()),
                    show_if: None,
                },
                ConfigField {
                    key: "port".into(),
                    label: "Management port".into(),
                    field_type: ConfigFieldType::Number,
                    required: false,
                    default: Some(serde_json::json!(15672)),
                    placeholder: Some("15672".into()),
                    show_if: None,
                },
                ConfigField {
                    key: "user".into(),
                    label: "User".into(),
                    field_type: ConfigFieldType::Text,
                    required: false,
                    default: Some(serde_json::json!("guest")),
                    placeholder: None,
                    show_if: None,
                },
                ConfigField {
                    key: "password".into(),
                    label: "Password".into(),
                    field_type: ConfigFieldType::Password,
                    required: false,
                    default: Some(serde_json::json!("guest")),
                    placeholder: None,
                    show_if: None,
                },
                ConfigField {
                    key: "vhost".into(),
                    label: "Default virtual host".into(),
                    field_type: ConfigFieldType::Text,
                    required: false,
                    default: Some(serde_json::json!("/")),
                    placeholder: Some("/".into()),
                    show_if: None,
                },
                ConfigField {
                    key: "tls".into(),
                    label: "Use TLS (https)".into(),
                    field_type: ConfigFieldType::Boolean,
                    required: false,
                    default: Some(serde_json::json!(false)),
                    placeholder: None,
                    show_if: None,
                },
            ],
        }
    }

    async fn connect(&self, cfg: ConnectionConfig) -> Result<Arc<dyn Connection>> {
        let host = cfg_str(&cfg, "host")?;
        let port = cfg_u16(&cfg, "port", 15672);
        let user = cfg_str_opt(&cfg, "user").unwrap_or_else(|| "guest".into());
        let password = cfg_secret(&cfg, "password")?.unwrap_or_else(|| "guest".into());
        let default_vhost = cfg_str_opt(&cfg, "vhost").unwrap_or_else(|| "/".into());
        let scheme = if cfg_bool(&cfg, "tls") {
            "https"
        } else {
            "http"
        };
        let base = format!("{scheme}://{host}:{port}");

        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| PluginError::Connection(e.to_string()))?;

        let conn = RabbitMqConnection {
            client,
            base,
            user,
            password,
            default_vhost,
        };
        tracing::info!(
            "connecting to rabbitmq management API at {} (vhost '{}')",
            conn.base,
            conn.default_vhost
        );
        // Validate eagerly so a bad host / disabled management plugin fails at
        // connect time rather than on first capability call.
        conn.get_json::<Overview>("/api/overview")
            .await
            .map_err(|e| {
                tracing::warn!("rabbitmq connect validation failed: {e}");
                e
            })?;
        tracing::info!("rabbitmq management API reachable");
        Ok(Arc::new(conn))
    }
}

impl RabbitMqConnection {
    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{}", self.base, path))
            .basic_auth(&self.user, Some(&self.password))
    }

    /// Map a non-2xx HTTP response into a helpful [`PluginError`].
    async fn check(resp: reqwest::Response) -> Result<reqwest::Response> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.text().await.unwrap_or_default();
        tracing::warn!("management API {} {}", status, body.trim());
        let hint = if status == reqwest::StatusCode::NOT_FOUND {
            " (is the rabbitmq_management plugin enabled?)"
        } else if status == reqwest::StatusCode::UNAUTHORIZED {
            " (check user/password and that the user has the 'management' tag)"
        } else {
            ""
        };
        Err(PluginError::Backend(format!(
            "management API returned {status}{hint}: {}",
            body.trim()
        )))
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let resp = self
            .req(reqwest::Method::GET, path)
            .send()
            .await
            .map_err(|e| PluginError::Connection(e.to_string()))?;
        let resp = Self::check(resp).await?;
        resp.json::<T>()
            .await
            .map_err(|e| PluginError::Backend(e.to_string()))
    }
}

impl RabbitMqPlugin {
    pub async fn overview(&self, conn: Arc<dyn Connection>) -> Result<Overview> {
        downcast(&conn)?.get_json("/api/overview").await
    }

    pub async fn list_queues(&self, conn: Arc<dyn Connection>) -> Result<Vec<Queue>> {
        downcast(&conn)?.get_json("/api/queues").await
    }

    pub async fn list_exchanges(&self, conn: Arc<dyn Connection>) -> Result<Vec<Exchange>> {
        downcast(&conn)?.get_json("/api/exchanges").await
    }

    pub async fn list_connections(&self, conn: Arc<dyn Connection>) -> Result<Vec<ConnectionRow>> {
        downcast(&conn)?.get_json("/api/connections").await
    }

    pub async fn list_channels(&self, conn: Arc<dyn Connection>) -> Result<Vec<ChannelRow>> {
        downcast(&conn)?.get_json("/api/channels").await
    }

    /// Fetch up to `count` messages from a queue. `ackmode` is the Management
    /// API verb: `ack_requeue_true` peeks (requeues), `ack_requeue_false`
    /// removes them.
    pub async fn get_messages(
        &self,
        conn: Arc<dyn Connection>,
        vhost: &str,
        queue: &str,
        count: u32,
        ackmode: &str,
    ) -> Result<Vec<GotMessage>> {
        let conn = downcast(&conn)?;
        let path = format!("/api/queues/{}/{}/get", urlencode(vhost), urlencode(queue));
        let body = serde_json::json!({
            "count": count,
            "ackmode": ackmode,
            "encoding": "auto",
            "truncate": 50000,
        });
        let resp = conn
            .req(reqwest::Method::POST, &path)
            .json(&body)
            .send()
            .await
            .map_err(|e| PluginError::Connection(e.to_string()))?;
        let resp = RabbitMqConnection::check(resp).await?;
        resp.json::<Vec<GotMessage>>()
            .await
            .map_err(|e| PluginError::Backend(e.to_string()))
    }

    /// Publish a message through `exchange` with `routing_key`. Use an empty
    /// exchange name + the queue name as the routing key to target a queue
    /// directly (the default exchange).
    pub async fn publish(
        &self,
        conn: Arc<dyn Connection>,
        vhost: &str,
        exchange: &str,
        routing_key: &str,
        payload: &str,
    ) -> Result<PublishResult> {
        let conn = downcast(&conn)?;
        tracing::info!(
            "publishing to exchange '{exchange}' (vhost '{vhost}', key '{routing_key}')"
        );
        let path = format!(
            "/api/exchanges/{}/{}/publish",
            urlencode(vhost),
            // The default exchange is named "amq.default" in the HTTP API.
            urlencode(if exchange.is_empty() {
                "amq.default"
            } else {
                exchange
            })
        );
        let body = serde_json::json!({
            "properties": {},
            "routing_key": routing_key,
            "payload": payload,
            "payload_encoding": "string",
        });
        let resp = conn
            .req(reqwest::Method::POST, &path)
            .json(&body)
            .send()
            .await
            .map_err(|e| PluginError::Connection(e.to_string()))?;
        let resp = RabbitMqConnection::check(resp).await?;
        resp.json::<PublishResult>()
            .await
            .map_err(|e| PluginError::Backend(e.to_string()))
    }

    pub async fn purge_queue(
        &self,
        conn: Arc<dyn Connection>,
        vhost: &str,
        queue: &str,
    ) -> Result<PurgeResult> {
        let conn = downcast(&conn)?;
        tracing::info!("purging queue '{queue}' (vhost '{vhost}')");
        let path = format!(
            "/api/queues/{}/{}/contents",
            urlencode(vhost),
            urlencode(queue)
        );
        let resp = conn
            .req(reqwest::Method::DELETE, &path)
            .send()
            .await
            .map_err(|e| PluginError::Connection(e.to_string()))?;
        RabbitMqConnection::check(resp).await?;
        Ok(PurgeResult {
            queue: queue.to_string(),
        })
    }

    pub async fn declare_queue(
        &self,
        conn: Arc<dyn Connection>,
        vhost: &str,
        queue: &str,
        durable: bool,
    ) -> Result<Queue> {
        let conn = downcast(&conn)?;
        tracing::info!("declaring queue '{queue}' (vhost '{vhost}', durable {durable})");
        let path = format!("/api/queues/{}/{}", urlencode(vhost), urlencode(queue));
        let body = serde_json::json!({
            "durable": durable,
            "auto_delete": false,
            "arguments": {},
        });
        let resp = conn
            .req(reqwest::Method::PUT, &path)
            .json(&body)
            .send()
            .await
            .map_err(|e| PluginError::Connection(e.to_string()))?;
        RabbitMqConnection::check(resp).await?;
        // PUT returns 201/204 with no useful body; read the queue back.
        conn.get_json(&path).await
    }

    pub async fn delete_queue(
        &self,
        conn: Arc<dyn Connection>,
        vhost: &str,
        queue: &str,
    ) -> Result<()> {
        let conn = downcast(&conn)?;
        tracing::info!("deleting queue '{queue}' (vhost '{vhost}')");
        let path = format!("/api/queues/{}/{}", urlencode(vhost), urlencode(queue));
        let resp = conn
            .req(reqwest::Method::DELETE, &path)
            .send()
            .await
            .map_err(|e| PluginError::Connection(e.to_string()))?;
        RabbitMqConnection::check(resp).await?;
        Ok(())
    }
}

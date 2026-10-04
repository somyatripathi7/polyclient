//! HTTP Client (`curlui`) plugin.
//!
//! This plugin only describes the connection form and opens a (stateless)
//! connection. The actual HTTP requests — and curl import — are performed by
//! the frontend via Tauri's HTTP plugin, so this plugin exposes no `call` ops
//! and holds no client/session.

use async_trait::async_trait;
use polyclient_core::{
    ConfigField, ConfigFieldType, Connection, ConnectionConfig, Plugin, PluginInfo, PluginKind,
    Result,
};
use std::sync::Arc;

pub struct CurlUiPlugin;

impl CurlUiPlugin {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CurlUiPlugin {
    fn default() -> Self {
        Self::new()
    }
}

/// A live connection. Holds no state: requests are issued by the frontend, so
/// there is no client or session to keep here. The connection exists only so
/// the host can track an open profile and route the `curlui` workspace to it.
pub struct CurlUiConnection;

impl Connection for CurlUiConnection {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[async_trait]
impl Plugin for CurlUiPlugin {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            id: "curlui".into(),
            name: "HTTP Client".into(),
            kind: PluginKind::Http,
            version: env!("CARGO_PKG_VERSION").into(),
            description:
                "Send HTTP requests with collections, environment variables, and curl import."
                    .into(),
            ui_module: Some("curlui".into()),
            protocol_version: polyclient_core::PROTOCOL_VERSION,
            // These settings are read by the frontend (which performs the
            // request); they still drive the connection form here.
            config_schema: vec![
                ConfigField {
                    key: "verify_tls".into(),
                    label: "Verify TLS certificates".into(),
                    field_type: ConfigFieldType::Boolean,
                    required: false,
                    default: Some(serde_json::json!(true)),
                    placeholder: None,
                    show_if: None,
                },
                ConfigField {
                    key: "follow_redirects".into(),
                    label: "Follow redirects".into(),
                    field_type: ConfigFieldType::Boolean,
                    required: false,
                    default: Some(serde_json::json!(true)),
                    placeholder: None,
                    show_if: None,
                },
                ConfigField {
                    key: "timeout_secs".into(),
                    label: "Timeout (seconds)".into(),
                    field_type: ConfigFieldType::Number,
                    required: false,
                    default: Some(serde_json::json!(30)),
                    placeholder: Some("30".into()),
                    show_if: None,
                },
            ],
        }
    }

    async fn connect(&self, _config: ConnectionConfig) -> Result<Arc<dyn Connection>> {
        Ok(Arc::new(CurlUiConnection))
    }

    async fn test(&self, _config: ConnectionConfig) -> Result<()> {
        Ok(())
    }
}

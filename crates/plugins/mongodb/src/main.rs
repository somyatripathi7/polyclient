//! MongoDB sidecar plugin entry point.
//!
//! Routes the `document.*` capability ops the host forwards onto the inherent
//! methods of [`MongoPlugin`].

use std::sync::Arc;

use async_trait::async_trait;
use polyclient_core::{Connection, PluginError, Result};
use polyclient_plugin_mongodb::MongoPlugin;
use polyclient_plugin_runtime::Dispatcher;
use serde::Deserialize;
use serde_json::Value;

struct MongoDispatcher(MongoPlugin);

#[derive(Deserialize)]
struct ListCollectionsParams {
    database: String,
}

#[derive(Deserialize)]
struct RunCommandParams {
    database: String,
    /// The command document as extended JSON (e.g. `{"find":"users",...}`).
    command: String,
}

fn parse<T: for<'de> Deserialize<'de>>(params: Value) -> Result<T> {
    serde_json::from_value(params).map_err(|e| PluginError::Config(format!("invalid params: {e}")))
}

fn to_value<T: serde::Serialize>(value: T) -> Result<Value> {
    serde_json::to_value(value).map_err(|e| PluginError::Backend(e.to_string()))
}

#[async_trait]
impl Dispatcher for MongoDispatcher {
    async fn dispatch(&self, op: &str, params: Value, conn: Arc<dyn Connection>) -> Result<Value> {
        match op {
            "document.list_databases" => to_value(self.0.list_databases(conn).await?),
            "document.list_collections" => {
                let p: ListCollectionsParams = parse(params)?;
                to_value(self.0.list_collections(conn, &p.database).await?)
            }
            "document.run_command" => {
                let p: RunCommandParams = parse(params)?;
                to_value(self.0.run_command(conn, &p.database, &p.command).await?)
            }
            _ => Err(PluginError::Unsupported),
        }
    }
}

fn main() -> anyhow::Result<()> {
    polyclient_plugin_runtime::run(MongoPlugin::new(), MongoDispatcher(MongoPlugin::new()))
}

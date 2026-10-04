//! RabbitMQ sidecar plugin entry point.
//!
//! Routes the `rabbitmq.*` capability ops the host forwards onto the inherent
//! methods of [`RabbitMqPlugin`], which speak the HTTP Management API.

use std::sync::Arc;

use async_trait::async_trait;
use polyclient_core::{Connection, PluginError, Result};
use polyclient_plugin_rabbitmq::RabbitMqPlugin;
use polyclient_plugin_runtime::Dispatcher;
use serde::Deserialize;
use serde_json::Value;

struct RabbitMqDispatcher(RabbitMqPlugin);

#[derive(Deserialize)]
struct GetMessagesParams {
    vhost: String,
    queue: String,
    count: u32,
    ackmode: String,
}

#[derive(Deserialize)]
struct PublishParams {
    vhost: String,
    exchange: String,
    routing_key: String,
    payload: String,
}

#[derive(Deserialize)]
struct QueueParams {
    vhost: String,
    queue: String,
}

#[derive(Deserialize)]
struct DeclareQueueParams {
    vhost: String,
    queue: String,
    #[serde(default = "default_true")]
    durable: bool,
}

fn default_true() -> bool {
    true
}

fn parse<T: for<'de> Deserialize<'de>>(params: Value) -> Result<T> {
    serde_json::from_value(params).map_err(|e| PluginError::Config(format!("invalid params: {e}")))
}

fn to_value<T: serde::Serialize>(value: T) -> Result<Value> {
    serde_json::to_value(value).map_err(|e| PluginError::Backend(e.to_string()))
}

#[async_trait]
impl Dispatcher for RabbitMqDispatcher {
    async fn dispatch(&self, op: &str, params: Value, conn: Arc<dyn Connection>) -> Result<Value> {
        match op {
            "rabbitmq.overview" => to_value(self.0.overview(conn).await?),
            "rabbitmq.list_queues" => to_value(self.0.list_queues(conn).await?),
            "rabbitmq.list_exchanges" => to_value(self.0.list_exchanges(conn).await?),
            "rabbitmq.list_connections" => to_value(self.0.list_connections(conn).await?),
            "rabbitmq.list_channels" => to_value(self.0.list_channels(conn).await?),
            "rabbitmq.get_messages" => {
                let p: GetMessagesParams = parse(params)?;
                to_value(
                    self.0
                        .get_messages(conn, &p.vhost, &p.queue, p.count, &p.ackmode)
                        .await?,
                )
            }
            "rabbitmq.publish" => {
                let p: PublishParams = parse(params)?;
                to_value(
                    self.0
                        .publish(conn, &p.vhost, &p.exchange, &p.routing_key, &p.payload)
                        .await?,
                )
            }
            "rabbitmq.purge_queue" => {
                let p: QueueParams = parse(params)?;
                to_value(self.0.purge_queue(conn, &p.vhost, &p.queue).await?)
            }
            "rabbitmq.declare_queue" => {
                let p: DeclareQueueParams = parse(params)?;
                to_value(
                    self.0
                        .declare_queue(conn, &p.vhost, &p.queue, p.durable)
                        .await?,
                )
            }
            "rabbitmq.delete_queue" => {
                let p: QueueParams = parse(params)?;
                self.0.delete_queue(conn, &p.vhost, &p.queue).await?;
                Ok(Value::Null)
            }
            _ => Err(PluginError::Unsupported),
        }
    }
}

fn main() -> anyhow::Result<()> {
    polyclient_plugin_runtime::run(
        RabbitMqPlugin::new(),
        RabbitMqDispatcher(RabbitMqPlugin::new()),
    )
}

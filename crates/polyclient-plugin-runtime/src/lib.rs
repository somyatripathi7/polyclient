//! Plugin SDK: the stdio JSON-RPC server loop that turns an in-process
//! [`polyclient_core::Plugin`] into a standalone sidecar executable.
//!
//! A plugin binary's `main` is just:
//!
//! ```ignore
//! fn main() -> anyhow::Result<()> {
//!     polyclient_plugin_runtime::run(MyPlugin::new(), MyDispatcher::new())
//! }
//! ```
//!
//! [`run`] handles the `--describe` CLI flag (printing the plugin's
//! [`PluginInfo`] manifest) and otherwise drives [`serve`]: it reads
//! line-delimited [`Request`]s from stdin, dispatches them concurrently, and
//! writes [`Response`]s to stdout. The live connection handles
//! (`PgPool`, `mongodb::Client`, ...) stay inside this process, keyed by
//! [`ConnectionId`]; only serializable values ever cross the pipe.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use polyclient_core::protocol::{Request, Response, PROTOCOL_VERSION};
use polyclient_core::{Connection, ConnectionConfig, ConnectionId, Plugin, PluginError, Result};
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, Mutex};

/// Maps an opaque capability `op` (e.g. `"rdbms.execute"`) plus its JSON
/// `params` to a result, given the live connection it targets. The host treats
/// `op` as opaque; each plugin family (rdbms/document/rabbitmq) provides its
/// own dispatcher. See `polyclient_rdbms_common::dispatch_rdbms`.
#[async_trait]
pub trait Dispatcher: Send + Sync + 'static {
    async fn dispatch(&self, op: &str, params: Value, conn: Arc<dyn Connection>) -> Result<Value>;
}

/// The plugin's live connections, keyed by the id the host assigned at
/// `connect` time. Held only inside the plugin process.
type Connections = Arc<Mutex<HashMap<ConnectionId, Arc<dyn Connection>>>>;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConnectParams {
    connection_id: ConnectionId,
    config: ConnectionConfig,
}

#[derive(Deserialize)]
struct TestParams {
    config: ConnectionConfig,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CloseParams {
    connection_id: ConnectionId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CallParams {
    connection_id: ConnectionId,
    op: String,
    #[serde(default)]
    params: Value,
}

fn parse<T: for<'de> Deserialize<'de>>(params: Value) -> Result<T> {
    serde_json::from_value(params).map_err(|e| PluginError::Config(format!("invalid params: {e}")))
}

fn to_value<T: serde::Serialize>(value: T) -> Result<Value> {
    serde_json::to_value(value).map_err(|e| PluginError::Backend(e.to_string()))
}

/// Resolve the plugin's [`PluginInfo`], stamping in the protocol version this
/// runtime speaks (so plugin authors never have to set it themselves).
fn describe(plugin: &dyn Plugin) -> polyclient_core::PluginInfo {
    let mut info = plugin.info();
    info.protocol_version = PROTOCOL_VERSION;
    info
}

/// Handle one request, returning the JSON to put in the `ok` field.
async fn handle<P, D>(
    plugin: &P,
    dispatcher: &D,
    conns: &Connections,
    req: Request,
) -> Result<Value>
where
    P: Plugin,
    D: Dispatcher,
{
    match req.method.as_str() {
        "describe" => to_value(describe(plugin)),
        "connect" => {
            let p: ConnectParams = parse(req.params)?;
            let conn_id = p.connection_id;
            let conn = plugin.connect(p.config).await?;
            conns.lock().await.insert(conn_id, conn);
            tracing::info!("connected {conn_id:?}");
            Ok(Value::Null)
        }
        "test" => {
            let p: TestParams = parse(req.params)?;
            plugin.test(p.config).await?;
            tracing::debug!("test connection ok");
            Ok(Value::Null)
        }
        "close" => {
            let p: CloseParams = parse(req.params)?;
            conns.lock().await.remove(&p.connection_id);
            tracing::info!("closed {:?}", p.connection_id);
            Ok(Value::Null)
        }
        "call" => {
            let p: CallParams = parse(req.params)?;
            let op = p.op;
            let conn = conns
                .lock()
                .await
                .get(&p.connection_id)
                .cloned()
                .ok_or_else(|| {
                    PluginError::NotFound(format!("connection {:?}", p.connection_id))
                })?;
            tracing::debug!("dispatch '{op}' for {:?}", p.connection_id);
            let res = dispatcher.dispatch(&op, p.params, conn).await;
            if let Err(e) = &res {
                tracing::warn!("dispatch '{op}' failed: {e}");
            }
            res
        }
        other => {
            tracing::warn!("unknown method: {other}");
            Err(PluginError::Backend(format!("unknown method: {other}")))
        }
    }
}

/// Run the stdio JSON-RPC server loop until stdin reaches EOF (the host closed
/// the pipe / is shutting the plugin down).
///
/// Each request is handled on its own task so a slow query doesn't block other
/// concurrent calls; a single writer task owns stdout so response lines never
/// interleave.
pub async fn serve<P, D>(plugin: P, dispatcher: D) -> anyhow::Result<()>
where
    P: Plugin + 'static,
    D: Dispatcher,
{
    let plugin = Arc::new(plugin);
    let dispatcher = Arc::new(dispatcher);
    let conns: Connections = Arc::new(Mutex::new(HashMap::new()));
    // Cancellation channels for in-flight request tasks, keyed by request id, so
    // a `cancel` can stop one cooperatively (we drop the query future at an await
    // point inside the task — never via task abort, which can turn a panicking
    // destructor into a process abort).
    let in_flight: Arc<Mutex<HashMap<u64, oneshot::Sender<()>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(line) = out_rx.recv().await {
            if stdout.write_all(line.as_bytes()).await.is_err()
                || stdout.write_all(b"\n").await.is_err()
                || stdout.flush().await.is_err()
            {
                break;
            }
        }
    });

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("received malformed request line: {e}");
                // We can't recover the request id, so reply under id 0.
                let resp = Response::err(
                    0,
                    PluginError::Backend(format!("invalid request: {e}")).into(),
                );
                if let Ok(s) = serde_json::to_string(&resp) {
                    let _ = out_tx.send(s);
                }
                continue;
            }
        };

        // `cancel` is handled inline (not spawned): signal the target request's
        // task to cancel; it drops its query future and replies on its own. We
        // just ack the cancel here.
        if req.method == "cancel" {
            if let Some(target) = req.params.get("id").and_then(|v| v.as_u64()) {
                tracing::info!("cancel requested for req {target}");
                if let Some(tx) = in_flight.lock().await.remove(&target) {
                    let _ = tx.send(());
                }
            }
            if let Ok(s) = serde_json::to_string(&Response::ok(req.id, Value::Null)) {
                let _ = out_tx.send(s);
            }
            continue;
        }

        let plugin = plugin.clone();
        let dispatcher = dispatcher.clone();
        let conns = conns.clone();
        let out_tx = out_tx.clone();
        let in_flight_task = in_flight.clone();
        let req_id = req.id;
        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let id = req.id;
            // Race the work against a cancel signal. Picking the cancel branch
            // drops the `handle(..)` future at its current await point — a
            // cancellation-safe drop that closes any in-flight query connection.
            let resp = tokio::select! {
                r = handle(&*plugin, &*dispatcher, &conns, req) => match r {
                    Ok(v) => Response::ok(id, v),
                    Err(e) => Response::err(id, (&e).into()),
                },
                _ = cancel_rx => {
                    Response::err(id, PluginError::Backend("cancelled".into()).into())
                }
            };
            in_flight_task.lock().await.remove(&id);
            if let Ok(s) = serde_json::to_string(&resp) {
                let _ = out_tx.send(s);
            }
        });
        in_flight.lock().await.insert(req_id, cancel_tx);
    }

    drop(out_tx);
    let _ = writer.await;
    Ok(())
}

/// Entry point for a plugin binary.
///
/// With `--describe` on the command line, prints the plugin's [`PluginInfo`]
/// as pretty JSON and exits (used to generate the install manifest). Otherwise
/// builds a Tokio runtime and runs [`serve`].
pub fn run<P, D>(plugin: P, dispatcher: D) -> anyhow::Result<()>
where
    P: Plugin + 'static,
    D: Dispatcher,
{
    // Logs MUST go to stderr; stdout is the protocol channel.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    if std::env::args().skip(1).any(|a| a == "--describe") {
        println!("{}", serde_json::to_string_pretty(&describe(&plugin))?);
        return Ok(());
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(serve(plugin, dispatcher))
}

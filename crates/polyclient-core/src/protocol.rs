//! Line-delimited JSON-RPC envelope spoken between the host and out-of-process
//! plugins over stdio.
//!
//! The host writes one [`Request`] per line to a plugin's stdin and reads one
//! [`Response`] per line from its stdout. Requests carry a monotonic `id` so the
//! host can multiplex concurrent calls over a single pipe.

use crate::PluginError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Wire-protocol version. Bumped on any breaking change to the envelope or the
/// host↔plugin method set. The host refuses plugins whose version differs.
pub const PROTOCOL_VERSION: u32 = 1;

/// serde default for [`crate::PluginInfo::protocol_version`].
pub fn default_protocol_version() -> u32 {
    PROTOCOL_VERSION
}

/// A call from the host to a plugin.
///
/// `method` is one of `describe` / `connect` / `test` / `close` / `call`; the
/// shape of `params` depends on the method (see the host `PluginManager`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// A plugin's reply to a [`Request`], correlated by `id`. Exactly one of `ok`
/// or `err` is set on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub err: Option<RpcError>,
}

impl Response {
    pub fn ok(id: u64, value: Value) -> Self {
        Self {
            id,
            ok: Some(value),
            err: None,
        }
    }

    pub fn err(id: u64, err: RpcError) -> Self {
        Self {
            id,
            ok: None,
            err: Some(err),
        }
    }
}

/// Serializable form of [`PluginError`]. `kind` mirrors the `PluginError`
/// variant so the frontend's existing error handling keeps working; `message`
/// carries the variant's payload (without the `Display` prefix) so a clean
/// round-trip back into a `PluginError` is possible.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub kind: String,
    pub message: String,
}

impl PluginError {
    /// Stable tag identifying the variant, used as [`RpcError::kind`].
    pub fn kind_str(&self) -> &'static str {
        match self {
            PluginError::Connection(_) => "connection",
            PluginError::Config(_) => "config",
            PluginError::Unsupported => "unsupported",
            PluginError::NotFound(_) => "notfound",
            PluginError::Backend(_) => "backend",
        }
    }
}

impl From<&PluginError> for RpcError {
    fn from(e: &PluginError) -> Self {
        let message = match e {
            PluginError::Connection(m)
            | PluginError::Config(m)
            | PluginError::NotFound(m)
            | PluginError::Backend(m) => m.clone(),
            PluginError::Unsupported => e.to_string(),
        };
        RpcError {
            kind: e.kind_str().to_string(),
            message,
        }
    }
}

impl From<PluginError> for RpcError {
    fn from(e: PluginError) -> Self {
        (&e).into()
    }
}

impl From<RpcError> for PluginError {
    fn from(e: RpcError) -> Self {
        match e.kind.as_str() {
            "connection" => PluginError::Connection(e.message),
            "config" => PluginError::Config(e.message),
            "unsupported" => PluginError::Unsupported,
            "notfound" => PluginError::NotFound(e.message),
            _ => PluginError::Backend(e.message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_ok_round_trips() {
        let r = Response::ok(7, serde_json::json!({"a": 1}));
        let line = serde_json::to_string(&r).unwrap();
        let back: Response = serde_json::from_str(&line).unwrap();
        assert_eq!(back.id, 7);
        assert_eq!(back.ok, Some(serde_json::json!({"a": 1})));
        assert!(back.err.is_none());
    }

    #[test]
    fn null_ok_is_a_success_not_an_error() {
        // A `null` success (connect/test/close) round-trips such that it is
        // distinguishable from an error: `err` stays `None`, and the consumer
        // treats a missing/null `ok` as `Value::Null`.
        let line = serde_json::to_string(&Response::ok(1, Value::Null)).unwrap();
        let back: Response = serde_json::from_str(&line).unwrap();
        assert!(back.err.is_none());
        assert_eq!(back.ok.unwrap_or(Value::Null), Value::Null);
    }

    #[test]
    fn plugin_error_round_trips_through_rpc_error() {
        for e in [
            PluginError::Connection("down".into()),
            PluginError::Config("bad".into()),
            PluginError::Unsupported,
            PluginError::NotFound("nope".into()),
            PluginError::Backend("boom".into()),
        ] {
            let display = e.to_string();
            let rpc: RpcError = (&e).into();
            let back: PluginError = rpc.into();
            assert_eq!(back.to_string(), display, "round-trip changed Display");
        }
    }
}

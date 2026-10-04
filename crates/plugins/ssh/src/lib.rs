use async_trait::async_trait;
use polyclient_core::{
    cfg_secret, ConfigField, ConfigFieldType, Connection, ConnectionConfig, Plugin, PluginError,
    PluginInfo, PluginKind, PtyPromptResponse, PtySpawnSpec, Result, ShowIf,
};
use std::sync::Arc;

pub struct SshPlugin;

impl SshPlugin {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SshPlugin {
    fn default() -> Self {
        Self::new()
    }
}

/// A validated SSH connection config — no live socket here; the PTY is owned
/// by the Tauri host which reads these values from the stored config directly.
pub struct SshConnection {
    pub config: ConnectionConfig,
}

impl Connection for SshConnection {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[async_trait]
impl Plugin for SshPlugin {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            id: "ssh".into(),
            name: "SSH".into(),
            kind: PluginKind::Cli,
            version: env!("CARGO_PKG_VERSION").into(),
            description: "Connect to a remote host over SSH and open a terminal.".into(),
            ui_module: Some("cli".into()),
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
            ],
        }
    }

    async fn connect(&self, cfg: ConnectionConfig) -> Result<Arc<dyn Connection>> {
        // Validate required fields; the actual SSH process is spawned by the
        // Tauri host when the CLI workspace mounts (using pty_spawn).
        let host = cfg
            .get("host")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| PluginError::Config("host is required".into()))?;
        let user = cfg
            .get("user")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| PluginError::Config("user is required".into()))?;
        let _ = (host, user);
        tracing::info!("ssh connection configured for {user}@{host}");
        Ok(Arc::new(SshConnection { config: cfg }))
    }

    /// Test reachability by opening a TCP connection to `host:port`. This
    /// verifies the host resolves, is reachable, and something is listening on
    /// the SSH port — without performing (non-interactive) SSH authentication,
    /// which the live PTY session handles separately.
    async fn test(&self, cfg: ConnectionConfig) -> Result<()> {
        let host = cfg
            .get("host")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| PluginError::Config("host is required".into()))?;
        cfg.get("user")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| PluginError::Config("user is required".into()))?;
        let port = cfg.get("port").and_then(|v| v.as_u64()).unwrap_or(22) as u16;

        let addr = format!("{host}:{port}");
        tracing::info!("ssh reachability test to {addr}");
        // Resolve + connect with a bounded timeout so an unreachable host fails
        // fast instead of hanging the test.
        let connect = tokio::net::TcpStream::connect(&addr);
        match tokio::time::timeout(std::time::Duration::from_secs(10), connect).await {
            Ok(Ok(_stream)) => Ok(()),
            Ok(Err(e)) => Err(PluginError::Connection(format!("cannot reach {addr}: {e}"))),
            Err(_) => Err(PluginError::Connection(format!(
                "connection to {addr} timed out"
            ))),
        }
    }
}

/// Build the PTY spawn spec (program + args + optional prompt auto-answer) for
/// an SSH connection from its config. All ssh-specific command knowledge lives
/// here so the host stays backend-agnostic.
fn build_spawn_spec(cfg: &ConnectionConfig) -> Result<PtySpawnSpec> {
    let host = cfg
        .get("host")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| PluginError::Config("host is required".into()))?;
    let user = cfg
        .get("user")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| PluginError::Config("user is required".into()))?;
    let port = cfg.get("port").and_then(|v| v.as_u64()).unwrap_or(22);
    let auth_mode = cfg
        .get("auth_mode")
        .and_then(|v| v.as_str())
        .unwrap_or("password");
    let password = cfg_secret(cfg, "password")?;
    let key_path = cfg.get("key_path").and_then(|v| v.as_str());

    let mut args: Vec<String> = vec![
        "-p".into(),
        port.to_string(),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
        "-o".into(),
        "ControlMaster=no".into(),
        // Give up after one failed password rather than re-prompting.
        "-o".into(),
        "NumberOfPasswordPrompts=1".into(),
    ];

    let mut prompt_response = None;
    match auth_mode {
        "key" => {
            if let Some(kp) = key_path.filter(|s| !s.is_empty()) {
                args.push("-i".into());
                args.push(kp.into());
            }
            // Prefer the key; don't silently fall back to a password prompt.
            args.push("-o".into());
            args.push("PasswordAuthentication=no".into());
        }
        _ => {
            // Password mode. Restrict to a SINGLE auth method so ssh emits at
            // most one `password:` prompt — offering both `password` and
            // `keyboard-interactive` makes ssh prompt once per method, which
            // produced a duplicate prompt.
            args.push("-o".into());
            args.push("PubkeyAuthentication=no".into());
            args.push("-o".into());
            args.push("PreferredAuthentications=password".into());
            args.push("-o".into());
            args.push("KbdInteractiveAuthentication=no".into());
            // If a password is saved, auto-answer the server's prompt.
            if let Some(pw) = password.filter(|p| !p.is_empty()) {
                prompt_response = Some(PtyPromptResponse {
                    pattern: "password:".into(),
                    send: pw,
                });
            }
        }
    }

    args.push(format!("{user}@{host}"));

    tracing::info!(
        "built ssh spawn spec for {user}@{host}:{port} (auth '{auth_mode}', password auto-answer: {})",
        prompt_response.is_some()
    );
    Ok(PtySpawnSpec {
        program: "ssh".into(),
        args,
        env: Vec::new(),
        prompt_response,
    })
}

/// Routes the SSH plugin's `call` ops. The only op is `cli.spawn_spec`, which
/// returns the [`PtySpawnSpec`] the host uses to launch the terminal process.
pub struct SshDispatcher;

#[async_trait]
impl polyclient_plugin_runtime::Dispatcher for SshDispatcher {
    async fn dispatch(
        &self,
        op: &str,
        _params: serde_json::Value,
        conn: Arc<dyn Connection>,
    ) -> Result<serde_json::Value> {
        match op {
            "cli.spawn_spec" => {
                let conn = conn
                    .as_any()
                    .downcast_ref::<SshConnection>()
                    .ok_or_else(|| PluginError::Backend("connection type mismatch".into()))?;
                let spec = build_spawn_spec(&conn.config)?;
                serde_json::to_value(spec).map_err(|e| PluginError::Backend(e.to_string()))
            }
            _ => Err(PluginError::Backend(format!("unknown op: {op}"))),
        }
    }
}

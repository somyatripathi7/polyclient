# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`polyclient` is a cross-platform desktop database/message-broker/integration client built with **Tauri 2** (Rust backend) and **React 18 + TypeScript + Vite** (frontend). One window connects to relational DBs, document stores, queues, remote shells, file stores, and HTTP APIs through a **plugin architecture**. Ships with **10 bundled plugins**:

| Plugin | `PluginKind` | Talks to |
| --- | --- | --- |
| `postgres` | `Rdbms` | PostgreSQL 12+ (via `sqlx`) |
| `mysql` | `Rdbms` | MySQL & MariaDB |
| `mssql` | `Rdbms` | Microsoft SQL Server (via Tiberius) |
| `snowflake` | `Rdbms` | Snowflake warehouses |
| `mongodb` | `Document` | MongoDB & Atlas |
| `rabbitmq` | `Rabbitmq` | brokers via the HTTP Management API |
| `ssh` | `Cli` | remote hosts via a PTY terminal + script runner |
| `sftp` | `FileManager` | file transfer over SFTP |
| `s3` | `FileManager` | S3 & S3-compatible buckets |
| `curlui` | `Http` | REST APIs (collections + curl import/export) |

## Commands

```bash
npm install                              # frontend deps (Rust deps fetch on first build)

# Run the app end-to-end. Plugins are out-of-process executables the host
# discovers at runtime, so build + install them first, then point the host at them:
npm run plugins:dev                      # builds the 10 bundled plugins into dev-plugins/ (debug)
POLYCLIENT_PLUGINS_DIR=$PWD/dev-plugins npm run tauri dev
# Or use the convenience script:
./start-dev.sh              # uses dev-plugins/ as-is
./start-dev.sh --build      # rebuild plugins first
./start-dev.sh --debug      # set RUST_LOG=debug
# Without plugins installed the connection form is empty — plugins:dev is required.

npm run dev                              # frontend-only Vite dev server (:1420)
npm run build                            # type-check (tsc) + build frontend
npm run tauri build                      # release bundle -> src-tauri/target/release/bundle/

cargo build                              # build full Rust workspace
cargo test                              # all Rust tests across workspace
cargo test -p polyclient-core                   # single crate
cargo test -p polyclient-core protocol          # single test/module by name filter
```

There is no frontend test runner or linter configured beyond `tsc` (run via `npm run build`).

## Architecture: the host is a generic pipe

The single most important thing to understand: **the Tauri host knows nothing about SQL, documents, or queues.** It ships with only `polyclient-core` (no DB drivers) and acts as a transport between the frontend and out-of-process plugin executables.

```
React frontend  ──@tauri-apps/api invoke()──▶  Tauri host (src-tauri)
                                                     │
                                          line-delimited JSON-RPC over stdio
                                                     ▼
                                          plugin sidecar executables
                                          (postgres / mysql / mssql / snowflake /
                                           mongodb / rabbitmq / ssh / sftp / s3 / curlui)
```

### Three layers, three boundaries

1. **Frontend → host** (`src/api.ts` ⇄ `src-tauri/src/commands.rs`): ~**27 Tauri commands** —
   plugin lifecycle (`list_plugins`, `test_connection`, `open_connection`, `close_connection`,
   `plugin_call`, `cancel_last_plugin_call`), GitHub install (`list_github_plugins`,
   `preview_github_plugin`, `install_github_plugin`, `uninstall_plugin`), app self-update
   (`check_update`, `install_update`, `app_channel`), persistence (`load_connections`/`save_connections`,
   `load_config`/`save_config`), per-connection workspace files (`read_workspace_file`,
   `write_workspace_file_at`, `list_workspace_dir`, `delete_workspace_path`), and PTY commands for CLI
   workspaces (`pty_spawn`, `pty_write`, `pty_resize`, `pty_close`, `pty_close_connection`, `pty_snapshot`).
   Every DB/queue/terminal/file/HTTP capability funnels through `plugin_call` with an **opaque `op` string**
   (e.g. `"rdbms.execute"`, `"cli.spawn_spec"`) + JSON `params`. The host passes
   `op`/`params` through untouched.

2. **Host → plugin** (`crates/polyclient-core/src/protocol.rs`): line-delimited JSON-RPC over stdio. The host
   writes one `Request {id, method, params}` per line to the plugin's stdin and reads one
   `Response {id, ok|err}` per line from stdout. `method` is one of `describe`/`connect`/`test`/`close`/`call`.
   A monotonic `id` multiplexes concurrent calls over the single pipe. `PROTOCOL_VERSION` (currently `1`)
   is checked at discovery/install — version-mismatched plugins are refused.

3. **Plugin internals** (`crates/polyclient-plugin-runtime`): the SDK that turns an in-process `polyclient_core::Plugin`
   into a sidecar. `polyclient_plugin_runtime::run(plugin, dispatcher)` handles the `--describe` CLI flag and runs
   the stdio server loop. Each plugin's `call` ops are routed by a `Dispatcher` (e.g.
   `polyclient_rdbms_common::RdbmsDispatcher` maps `"rdbms.*"` ops onto `RdbmsPlugin` trait methods).

### Live connection handles never cross any boundary

`PgPool`, `mongodb::Client`, and the RabbitMQ management HTTP client stay **inside the plugin process**, keyed by a
`ConnectionId` (a UUID). The host's `PluginManager` only tracks routes (`ConnectionId → plugin_id`);
the plugin's runtime holds the actual handles (`ConnectionId → Arc<dyn Connection>`). Never try to
serialize a pool/client across the pipe.

### Process lifecycle (`src-tauri/src/plugin_manager.rs`)

- `discover()` scans the plugins dir for `*.plugin.json` manifests at startup — **no process spawn**.
- A plugin process is spawned **lazily on first use** and stays warm (holding its pools) until the app exits.
- If a plugin crashes, its in-flight requests fail and it is **respawned on next use** (the `alive` flag).
- Plugins dir resolution: `$POLYCLIENT_PLUGINS_DIR` if set, else `<app-data>/plugins`.

## The crate workspace

| Crate | Role |
| --- | --- |
| `crates/polyclient-core` | Backend-agnostic foundation: `Plugin`/`Connection` traits, `PluginKind` (`Rdbms`, `Document`, `Rabbitmq`, `Cli`, `FileManager`, `Http`, `Other`), config-schema types the UI builds forms from (`PluginInfo`, `ConfigField`, `ShowIf`, `ConfigFieldType` including `FilePath`), `ConnectionId`, `PluginError`, the wire `protocol`, and PTY contract types (`PtySpawnSpec`, `PtyPromptResponse`). |
| `crates/polyclient-plugin-runtime` | Plugin SDK: the stdio JSON-RPC server loop (`run`/`serve`) + the `Dispatcher` trait. |
| `crates/polyclient-rdbms-common` | `RdbmsPlugin` trait + shared relational types (`Schema`, `Table`, `Column`, `QueryResult`, `RowChanges`, `ApplyResult`), and `dispatch_rdbms`/`RdbmsDispatcher` mapping `"rdbms.*"` ops to trait methods. |
| `crates/plugins/{postgres,mysql,mssql,snowflake,mongodb,rabbitmq,ssh,sftp,s3,curlui}` | One binary crate per backend. `lib.rs` = the `Plugin` impl; `main.rs` = `polyclient_plugin_runtime::run(plugin, dispatcher)`. The four relational plugins implement `RdbmsPlugin`; `ssh` is `PluginKind::Cli` (returns a `PtySpawnSpec` from `cli.spawn_spec`, PTY managed by the host); `sftp`/`s3` are `PluginKind::FileManager`; `curlui` is `PluginKind::Http`. |
| `src-tauri` | The Tauri host: `lib.rs` (registry/commands), `commands.rs` (Tauri command surface), `plugin_manager.rs` (discovery + multiplexing), `pty.rs` (PTY lifecycle for CLI workspaces), `logging.rs` (log file setup), `github.rs` (release fetch/verify), `persistence.rs` (saved profiles). |

## The type contract is mirrored in two places

Every cross-boundary struct exists twice and **both must change together**:
- **Rust**: `Serialize`/`Deserialize` structs in `polyclient-core`, `polyclient-rdbms-common`, or a plugin.
- **TypeScript**: `src/api.ts` mirrors them, and the typed `api.*` wrappers are the only place that knows
  the `op` strings.

serde casing conventions to match:
- **enums** use `rename_all = "lowercase"` (e.g. `PluginKind` → `"rdbms"`, `TableKind` → `"materializedview"`).
- **saved-connection persistence** uses `camelCase` on the wire (`SavedConnection`, `GithubPreview`).
- some config-schema fields use explicit renames (`field_type` → `"type"`).

## Adding a plugin

1. New binary crate under `crates/plugins/<name>` depending on `polyclient-core`, `polyclient-plugin-runtime`, and
   (for relational backends) `polyclient-rdbms-common`. Add it to the workspace `members` in `Cargo.toml`.
2. Implement `polyclient_core::Plugin`: return a `PluginInfo` with the right `kind` and a `config_schema`
   describing the connection form; implement `connect` returning `Arc<dyn Connection>`.
3. For relational backends, also implement `RdbmsPlugin` (`list_schemas`, `list_tables`, `describe_table`,
   `execute`, optionally `apply_changes` for editing — others default to `PluginError::Unsupported`).
4. `main.rs` calls `polyclient_plugin_runtime::run(plugin, dispatcher)`. For RDBMS use `RdbmsDispatcher(plugin)`.
5. Add the crate to `scripts/dev-plugins.sh` (`PLUGINS`/`CRATES` arrays) so `npm run plugins:dev` builds it.

Non-relational backends pick a different `PluginKind` (`Document`, `Rabbitmq`, `Cli`, `FileManager`, `Http`, `Other`); the frontend
renders the matching workspace component (`src/components/workspaces/`) based on `kind`/`ui_module`.
`Cli`-kind plugins must handle the `cli.spawn_spec` op and return a `PtySpawnSpec`; the host spawns
that process in a PTY and streams I/O to the `CliWorkspace` component via Tauri events.

There is **no `build_registry()`** anymore — plugins are not statically linked. Discovery is purely from
manifests at runtime.

## Plugin distribution & install

- A plugin = a standalone executable + a generated `<id>.plugin.json` manifest (`{pluginInfo, executable}`).
  The manifest is produced by running the binary's `--describe` (prints its `PluginInfo` as JSON).
- **In-app GitHub install** (`github.rs` + `InstallPluginDialog.tsx`): pick a repo/tag → the host selects the
  asset matching the OS/arch **target triple**, shows the published SHA-256, then on confirm downloads,
  verifies the checksum, runs `--describe` to generate the manifest, and registers it live (no rebuild).
  Release convention: one binary per platform with the target triple in the asset name, plus a
  `SHA256SUMS`/`checksums.txt`/per-asset `.sha256`.

## Saved connections & a security caveat

Profiles persist per-plugin at `<app_data_dir>/connections/<plugin_id>/connections.json` (human-readable
JSON; `persistence.rs` validates plugin ids against path traversal). Profiles whose owning plugin is no
longer installed are skipped on load but left on disk.

⚠️ Connection configs — **including passwords** — are stored in **plaintext**. No secure-credential store
is implemented yet.

## Docs

- `README.md` — user + contributor guide; accurate to the sidecar model.
- `plugin-architecture.md` — design doc for the (now-implemented) runtime-loaded
  sidecar model. Trust the code (`plugin_manager.rs`, `polyclient-plugin-runtime`,
  `scripts/dev-plugins.sh`) if anything ever drifts.

## License

MIT (see `LICENSE`). `Cargo.toml` uses `license = "MIT"`; all crates inherit via
`license.workspace = true`.

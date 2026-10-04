# Plugin Architecture: Runtime-Installable Out-of-Process Plugins

## Context

Today every plugin (postgres, mongodb, rabbitmq) is a workspace crate that is
**statically linked** into the Tauri host (`src-tauri/Cargo.toml` depends on
each, and `build_registry()` in `src-tauri/src/lib.rs:17` hardcodes
`registry.register(...)`). Shipping the app ships every DB driver (sqlx,
mongodb, lapin) whether or not the user needs it, and adding a plugin requires
recompiling the app.

**Goal:** ship the host with *only `polyclient-core`* (no DB drivers). A user who wants
Postgres installs the Postgres plugin separately, and the host loads it at
runtime.

**Decided approach** (confirmed with the user):
- **Mechanism:** out-of-process *sidecar* plugins — each plugin is a standalone
  executable; the host talks to it over **line-delimited JSON-RPC on stdio**.
  No Rust-ABI fragility; crash isolation; plugins keep using sqlx/mongodb/lapin
  unchanged.
- **Host coupling:** *generic pipe* — the host knows nothing about
  SQL/documents/queues. It discovers plugins, spawns them, and routes opaque
  JSON calls by `connectionId → plugin`. A new plugin *kind* needs zero host
  changes.

**Open items (my recommendations, to confirm):**
- **Scope:** *framework + Postgres first* (build SDK/host-manager/protocol,
  convert Postgres as the reference plugin; convert mongodb/rabbitmq in a
  follow-up using the identical pattern).
- **Install model:** host scans `<app-data>/polyclient/plugins/`; each plugin is an
  executable plus a generated `*.plugin.json` manifest (so listing plugins
  doesn't spawn N processes). Dev override via `POLYCLIENT_PLUGINS_DIR`.

## Why this is feasible (key facts from exploration)

- Every method's args and results across all three plugins are already
  `Serialize`/`Deserialize` (`PluginInfo`, `ConnectionConfig`, `QueryResult`,
  `ColumnValue`, `RowChanges`, `Schema`, `Column`, `FindResult`,
  `MongoCollection`, `QueueInfo`, `ConsumedMessage`, ...).
- The **only** non-serializable values are the live connection handles
  (`PgPool`, `mongodb::Client`, `lapin::Connection`). In the sidecar model these
  **stay inside the plugin process** and are referenced everywhere by
  `ConnectionId` (a UUID — serializable). This is the usual hard part of
  out-of-process plugins, and it's already solved by the existing design.
- The frontend already carries all kind-specific typing (in `api.ts` + the
  workspace components), so the host doesn't need it.

## Target crate layout

```
polyclient-core                 (shipped in host AND used by plugins)
  - protocol envelope: Request/Response/RpcError (NEW)
  - PluginInfo, ConfigField, PluginKind, ConnectionId, ConnectionConfig
  - Connection trait, Plugin trait
  (no DB drivers; pure types + traits)

polyclient-plugin-runtime       (NEW — plugin SDK; used by plugin binaries only)
  - serve() stdio JSON-RPC loop: reads stdin, writes stdout
  - owns HashMap<ConnectionId, Arc<dyn Connection>> (the live connections)
  - handles describe/connect/test/close generically
  - delegates `call {op, params}` to a capability dispatcher

polyclient-rdbms-common         (used by RDBMS plugin binaries only — NOT the host)
  - RdbmsPlugin trait + Schema/Table/Column/QueryResult/RowChanges/...
  - dispatch_rdbms(op, params, &dyn RdbmsPlugin, conn) -> Result<Value>
    maps "rdbms.execute" | "rdbms.apply_changes" | ... to trait methods
  (mongodb/rabbitmq get analogous *-common dispatchers later)

polyclient-plugin-postgres      (CHANGES from lib crate -> binary crate)
  - src/main.rs: build PostgresPlugin, call runtime serve() with the
    rdbms dispatcher; depends on sqlx as before
  - `--describe` flag prints its PluginInfo manifest JSON and exits

src-tauri (host)         (depends ONLY on polyclient-core + tauri + tokio)
  - PluginManager: discovery, process spawn, request multiplexing, routing
  - generic Tauri commands (list_plugins / open_connection /
    close_connection / test_connection / plugin_call)
  - NO sqlx / mongodb / lapin / rdbms-common dependencies
```

## Wire protocol (host → plugin, over stdio)

Line-delimited JSON. Requests carry a monotonic `id` so the host can multiplex
concurrent Tauri commands over one stdio pipe.

```
Request : {"id": <u64>, "method": <string>, "params": <json>}\n
Response: {"id": <u64>, "ok": <json>}\n
        | {"id": <u64>, "err": {"kind": <string>, "message": <string>}}\n
```

Methods the host calls on a plugin:
- `describe` → `PluginInfo`  (also reachable via the `--describe` CLI flag)
- `connect` `{connectionId, config}` → `null`  (plugin opens + stores the conn)
- `test` `{config}` → `null`
- `close` `{connectionId}` → `null`
- `call` `{connectionId, op, params}` → `<json>`  (op is opaque to the host,
  e.g. `"rdbms.execute"`, `"rdbms.apply_changes"`, `"document.find"`)

`RpcError.kind` mirrors `PluginError` variants (Connection/Config/Unsupported/
NotFound/Backend) so the existing frontend error handling keeps working.

## Host PluginManager

- **Discovery:** scan `POLYCLIENT_PLUGINS_DIR` (default `<app-data>/polyclient/plugins/`) for
  `*.plugin.json` manifests. Each manifest = `{ pluginInfo, executable }`.
  `list_plugins` returns the cached `PluginInfo`s with no process spawn.
- **Process model:** spawn a plugin's process **lazily** on first
  `open_connection`, one process per plugin (it can hold many connections), keep
  it **warm** until app exit. Kill children on shutdown.
- **Multiplexing:** per child, a writer + a reader task. The reader matches
  responses to `oneshot` channels by `id`, so concurrent commands don't block
  each other. State: `HashMap<plugin_id, ChildHandle>` and
  `HashMap<ConnectionId, plugin_id>`.
- **Failure handling:** if a child dies, fail its in-flight requests and mark
  its connections dead with a `Backend` error surfaced to the UI (existing
  `status-line error`).

## Tauri command surface (host)

Replaces today's typed `rdbms_*`/`doc_*`/`mq_*` commands with five generic ones:

```rust
list_plugins() -> Vec<PluginInfo>
test_connection(plugin_id, config) -> Result<(), String>
open_connection(plugin_id, config) -> Result<ConnectionId, String>   // spawns/uses child, sends connect
close_connection(connection_id)   -> Result<(), String>
plugin_call(connection_id, op, params: Value) -> Result<Value, String> // routed to owning plugin
```

## Frontend changes (`src/api.ts` only)

The workspace components (`RdbmsWorkspace.tsx`, etc.) and all the editing work
done earlier are **unchanged**. Only the transport under `api.ts` shifts: the
typed wrappers keep their signatures but funnel through one helper.

```ts
const pluginCall = (connectionId, op, params) =>
  invoke("plugin_call", { connectionId, op, params });

// e.g.
rdbmsExecute:      (id, sql)            => pluginCall(id, "rdbms.execute",       { sql }),
rdbmsApplyChanges: (id, schema, table, changes)
                                        => pluginCall(id, "rdbms.apply_changes", { schema, table, changes }),
rdbmsListSchemas:  (id)                 => pluginCall(id, "rdbms.list_schemas",  {}),
docFind:           (id, db, coll, f, n) => pluginCall(id, "document.find",       { database: db, collection: coll, filter: f, limit: n }),
// ...mongodb + rabbitmq wrappers likewise
```

`PluginInfo`/`ConfigField` types and the generic connection form are unchanged.

## Installation / packaging (v1)

- A plugin ships as: one executable + one `<id>.plugin.json` manifest.
- Manifest is generated by the plugin itself: `polyclient-plugin-postgres --describe`
  prints `PluginInfo`; an installer (or the host on first sight of a bare
  executable) writes it next to the binary. Keeps schema DRY — one source.
- "Install" for v1 = drop both files into the plugins dir. (An in-app
  "install from file/URL" flow is a straightforward follow-up.)

## Implementation steps (framework + Postgres first)

1. **polyclient-core**: add protocol envelope (`Request`/`Response`/`RpcError`),
   make `PluginError`↔`RpcError` convertible. Keep `Plugin`/`Connection`/
   `PluginInfo`. Remove the host-only `PluginRegistry` (or move it; the host no
   longer uses in-process registration).
2. **polyclient-plugin-runtime** (new): stdio serve loop + connection registry +
   describe/connect/test/close handling; takes a capability-dispatch closure.
3. **polyclient-rdbms-common**: add `dispatch_rdbms(op, params, plugin, conn)` mapping
   op strings to the existing `RdbmsPlugin` methods (incl. `apply_changes`).
4. **polyclient-plugin-postgres**: convert lib → binary (`main.rs`); add `--describe`;
   wire `serve(PostgresPlugin, dispatch_rdbms)`. sqlx logic unchanged.
5. **src-tauri**: add `PluginManager` (discovery/spawn/mux/route); replace
   `commands.rs` with the 5 generic commands; drop all plugin + rdbms-common +
   driver deps from `Cargo.toml`; update `lib.rs` `run()` to build the manager.
6. **Frontend**: refactor `api.ts` transport to `plugin_call`; no component
   changes.
7. **Build/dev ergonomics**: a dev script that builds `polyclient-plugin-postgres`,
   runs `--describe` into `<plugins-dir>/postgres.plugin.json`, and copies the
   binary — so `npm run tauri dev` finds it.
8. **(Follow-up)** Convert mongodb + rabbitmq with the same pattern: add
   `dispatch_document`/`dispatch_rabbitmq`, flip each crate to a binary.

## Verification

- **Unit**: round-trip the protocol envelope; `dispatch_rdbms` maps each op to
  the right method and rejects unknown ops; `PluginError`↔`RpcError`.
- **Plugin standalone**: `polyclient-plugin-postgres --describe` prints valid
  `PluginInfo`; pipe a hand-written `connect`+`call rdbms.execute` sequence into
  the binary's stdin and inspect stdout.
- **End-to-end** (needs a local Postgres): build + install the postgres plugin
  into the plugins dir, `npm run tauri dev`, confirm: plugin appears in the
  sidebar, connect works, schema tree loads, `SELECT` runs, and the editing
  flow (staged multi-cell edit + Save/Cancel) still commits — proving the
  generic pipe preserves behavior.
- **Decoupling check**: `cargo tree -p polyclient-app` shows no `sqlx`/`mongodb`/
  `lapin`; deleting the plugin from the dir makes it vanish from the UI with no
  host rebuild.

## Risks / tradeoffs

- **IPC overhead**: one JSON round-trip per call. Negligible for an interactive
  client; multiplexing keeps concurrency.
- **Process lifecycle**: must handle child crash/timeout cleanly (covered
  above). Warm processes hold DB pools open as today.
- **Trust**: plugins are native executables with full host privileges (same as
  any installed binary). Note for users; sandboxing is out of scope for v1.
- **Versioning**: include a `protocolVersion` in `describe`; host refuses
  incompatible plugins with a clear message.

## Follow-up: plugin-provided UI

This doc decoupled the plugin **backend** (the host ships no drivers). The
**frontend** is still frozen at build time — `App.tsx` hardcodes a `switch` over
three compiled-in workspace components, so a plugin cannot yet ship its own UI.
See [`plugin-ui-architecture.md`](./plugin-ui-architecture.md) for the design that
closes that gap (declarative workspace schemas first, a sandboxed web-asset escape
hatch later).

## Follow-up: plugin-provided UI

This doc decoupled the plugin **backend** (the host ships no drivers). The
**frontend** is still frozen at build time — `App.tsx` hardcodes a `switch` over
three compiled-in workspace components, so a plugin cannot yet ship its own UI.
See [`plugin-ui-architecture.md`](./plugin-ui-architecture.md) for the design that
closes that gap (declarative workspace schemas first, a sandboxed web-asset escape
hatch later).

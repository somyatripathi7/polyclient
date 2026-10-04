# polyclient.

A cross-platform desktop client for databases and message brokers, built with
[Tauri](https://tauri.app) (Rust backend) and React + TypeScript (frontend).
One app to connect to relational databases, document stores, and queues through
a unified, plugin-based architecture.

Ships today with four plugins:

| Plugin | Kind | What it talks to |
| --- | --- | --- |
| **PostgreSQL** | `rdbms` | PostgreSQL 12+ databases (via `sqlx`) |
| **MySQL** | `rdbms` | MySQL 8.0+ and MariaDB databases (via `sqlx`) |
| **SQL Server** | `rdbms` | Microsoft SQL Server 2012+ and Azure SQL (via `tiberius`) |
| **MongoDB** | `document` | MongoDB and Atlas clusters |
| **RabbitMQ** | `rabbitmq` | RabbitMQ brokers via the HTTP Management API (requires the `rabbitmq_management` plugin) |
| **SSH** | `cli` | Remote hosts over SSH; opens a full PTY terminal + script runner |

---

## Highlights

- **Unified workspace** — one window, a sidebar of saved connections, and a
  workspace per connection rendered according to its plugin `kind`.
- **Plugin architecture** — each backend is a self-contained crate implementing
  a small set of Rust traits. The frontend renders connection forms and
  workspaces generically from a plugin's declared config schema and `kind`.
- **RDBMS editing** — browse schemas/tables, run SQL, and stage multi-cell
  edits (inserts/updates/deletes) that commit atomically in a single
  transaction.
- **Document & RabbitMQ surfaces** — query MongoDB collections; browse a
  RabbitMQ broker's overview, queues, exchanges, connections, and channels
  (management-UI style), and publish/get/purge messages.
- **CLI / SSH workspace** — full PTY terminal for SSH connections, with a
  side-by-side script editor (saved `.sh` files per profile) and run
  selection/line/all shortcuts. The session survives workspace remounts.
- **Saved connections** — connection profiles persist across restarts as
  human-readable JSON in the OS app-data directory.

---

## Architecture

```
┌──────────────────────────────────────────────┐
│ Frontend (React + TypeScript, Vite)           │
│   src/                                         │
│   - App.tsx, Sidebar, ConnectionForm          │
│   - workspaces/{Rdbms,Document,RabbitMq}       │
│   - api.ts  ← typed bridge to Tauri commands   │
└───────────────┬────────────────────────────────┘
                │  @tauri-apps/api  invoke(...)
┌───────────────▼────────────────────────────────┐
│ Tauri host (Rust)  —  src-tauri/  (no drivers)  │
│   - lib.rs           discovers plugins on start │
│   - commands.rs      generic #[tauri::command]s │
│   - plugin_manager   spawns + multiplexes procs │
│   - github.rs        fetch/verify/install        │
│   - persistence.rs   saved connection profiles  │
└───────────────┬────────────────────────────────┘
                │  line-delimited JSON-RPC over stdio
┌───────────────▼────────────────────────────────┐
│ Plugin sidecars (standalone executables)        │
│   crates/polyclient-core            core types/traits  │
│   crates/polyclient-plugin-runtime  stdio JSON-RPC SDK │
│   crates/polyclient-rdbms-common    RDBMS trait + types│
│   crates/plugins/postgres    sqlx               │
│   crates/plugins/mongodb     mongodb            │
│   crates/plugins/rabbitmq    reqwest (mgmt API) │
│   crates/plugins/ssh         PTY / SSH          │
└─────────────────────────────────────────────────┘
```

The host knows nothing about SQL/documents/queues. It discovers plugins from
`*.plugin.json` manifests, spawns each plugin's executable lazily on first use,
and routes opaque JSON `call`s to the plugin that owns a given `ConnectionId`
over line-delimited JSON-RPC on stdio.

### Crates

- **`polyclient-core`** — backend-agnostic foundation. Defines `Plugin` and
  `Connection` traits, `PluginKind` (`Rdbms`, `Document`, `Rabbitmq`, `Cli`,
  `Other`), the serializable config-schema types (`PluginInfo`, `ConfigField`,
  `ConfigFieldType` including `FilePath` for native file-picker fields, …) the
  UI builds forms from, `ConnectionId`, `PluginError`, the line-delimited
  JSON-RPC `protocol` (`Request`/`Response`, `PROTOCOL_VERSION`) the host and
  plugins speak, and the PTY contract types (`PtySpawnSpec`,
  `PtyPromptResponse`) used by `cli`-kind plugins.
- **`polyclient-plugin-runtime`** — the plugin SDK. `run(plugin, dispatcher)` handles
  the `--describe` flag and runs the stdio JSON-RPC server loop that turns an
  in-process `polyclient_core::Plugin` into a standalone sidecar; the `Dispatcher` trait
  routes opaque capability `op`s to the plugin's methods.
- **`polyclient-rdbms-common`** — the `RdbmsPlugin` trait and shared relational types
  (`Schema`, `Table`, `Column`, `QueryResult`, `RowChanges`, `ApplyResult`, …),
  plus `RdbmsDispatcher`/`dispatch_rdbms` mapping `rdbms.*` ops onto the trait.
  Any relational plugin implements this in addition to `polyclient_core::Plugin` and
  reuses the same `rdbms` UI module.
- **`crates/plugins/*`** — one binary crate per backend, each implementing
  `Plugin` (and `RdbmsPlugin` for relational backends) and shipping a `main`
  that calls `polyclient_plugin_runtime::run`. Includes `postgres`, `mongodb`,
  `rabbitmq`, and `ssh` (the `Cli`-kind plugin).

> **The plugin model:** plugins are **out-of-process sidecar executables** the
> host discovers and loads at runtime over line-delimited JSON-RPC on stdio. The
> shipped host binary contains no DB drivers. See
> [`plugin-architecture.md`](./plugin-architecture.md) for the full design.

### Key types (Rust ⇄ TypeScript)

All cross-boundary types are `Serialize`/`Deserialize` in Rust and mirrored in
`src/api.ts`. A `ConnectionId` is a UUID that serializes as a string; live
connection handles (`PgPool`, `mongodb::Client`, the RabbitMQ HTTP client) stay in
the backend and are referenced everywhere by that id.

---

## Project layout

```
polyclient/
├── Cargo.toml              Rust workspace (core, rdbms-common, 3 plugins, host)
├── package.json            Frontend deps + Vite/Tauri scripts
├── vite.config.ts          Vite config (dev server on :1420)
├── index.html              Frontend entry
├── plugin-architecture.md  Design doc: runtime-loaded sidecar plugins
├── crates/
│   ├── polyclient-core/           Plugin/Connection traits, JSON-RPC protocol, shared types
│   ├── polyclient-plugin-runtime/ Plugin SDK: stdio JSON-RPC server loop + Dispatcher
│   ├── polyclient-rdbms-common/   RdbmsPlugin trait + relational types + dispatch
│   └── plugins/
│       ├── postgres/       PostgreSQL plugin (sqlx)
│       ├── mongodb/        MongoDB plugin
│       ├── rabbitmq/       RabbitMQ plugin (HTTP management API)
│       └── ssh/            SSH plugin (cli kind; PTY spawned by host)
├── scripts/
│   └── dev-plugins.sh      Build bundled plugins + manifests into dev-plugins/
├── docs/                   Marketing/docs site (GitHub Pages) + install.sh
│   └── install.sh          Install the released desktop app (macOS/Linux)
├── src/                    React + TypeScript frontend
│   ├── App.tsx             Top-level app shell + connection lifecycle
│   ├── api.ts              Typed bridge to Tauri commands
│   ├── store.ts            Saved-connection persistence (frontend side)
│   ├── components/
│   │   ├── Sidebar.tsx
│   │   ├── ConnectionForm.tsx
│   │   ├── InstallPluginDialog.tsx
│   │   └── workspaces/     Rdbms / Document / RabbitMq workspaces
│   └── styles.css
└── src-tauri/              Tauri host (ships with no DB drivers)
    ├── Cargo.toml
    ├── tauri.conf.json     App/window/bundle config
    └── src/
        ├── lib.rs          Discovers plugins on startup, registers commands
        ├── commands.rs     Generic #[tauri::command] surface
        ├── plugin_manager.rs  Plugin discovery, process spawning, multiplexing
        ├── pty.rs          PTY manager: spawn/write/resize/close for CLI workspaces
        ├── logging.rs      Log file setup
        ├── github.rs       GitHub release fetch / checksum / install
        ├── persistence.rs  Saved connection profiles (per-plugin JSON)
        └── main.rs
```

---

## Install

The quickest way to get the desktop app on **macOS or Linux** is the install
script — it downloads the release build matching your OS/arch and installs it
(macOS: copies `polyclient.app` into `/Applications`; Linux: places the AppImage at
`~/.local/bin/polyclient`):

```bash
curl -fsSL https://somyatripathi7.github.io/polyclient/install.sh | bash

# rolling nightly build instead of the latest stable release:
curl -fsSL https://somyatripathi7.github.io/polyclient/install.sh | bash -s -- --nightly
```

Flags: `--nightly` (rolling prerelease), `--tag v0.1.2` (a specific release),
`--dir <path>` (install location). Prefer to read before you pipe to a shell?
Download [`docs/install.sh`](docs/install.sh) and run it directly.

**Windows**, or **Linux on arm64**, aren't covered by the script — grab an
installer from the [latest release](https://github.com/somyatripathi7/polyclient/releases/latest)
(`.msi`/`.exe` on Windows, `.AppImage`/`.deb`/`.rpm` on Linux).

The script installs only the app. Once it's running, add database/queue plugins
from the sidebar's **⤓ Install plugin** dialog — see [Plugins](#plugins). The app
auto-updates from GitHub releases.

> ⚠️ On macOS the build is unsigned; the script clears the Gatekeeper quarantine
> flag after copying so it launches, but you may still need to right-click →
> **Open** the first time.

---

## Getting started

> The steps below are for **building and running from source** (contributors).
> To just use the app, see [Install](#install) above.

### Prerequisites

- **Rust** (stable) and Cargo — https://rustup.rs
- **Node.js** 18+ and npm
- **Tauri 2 system dependencies** for your OS — see the
  [Tauri prerequisites guide](https://tauri.app/start/prerequisites/)
  (on macOS: Xcode Command Line Tools).

### Install

```bash
npm install
```

(Rust dependencies are fetched automatically on first build.)

### Run in development

Plugins are out-of-process executables the host discovers at runtime, so build
and install them first, then point the host at the same directory:

```bash
npm run plugins:dev                     # builds the 4 bundled plugins + manifests
POLYCLIENT_PLUGINS_DIR=$PWD/dev-plugins npm run tauri dev
```

Or use the convenience script, which handles the `$PWD` resolution for you:

```bash
./start-dev.sh              # launch with already-built plugins
./start-dev.sh --build      # rebuild plugins first, then launch
./start-dev.sh --debug      # enable RUST_LOG=debug
./start-dev.sh --build --debug   # combine flags
```

This starts the Vite dev server on `http://localhost:1420` and launches the
Tauri window with hot-reload for both the Rust and frontend code. Without any
plugins installed the connection form will be empty — install one (see
[Plugins](#plugins)).

### Build a release bundle

```bash
npm run tauri build
```

Produces platform installers/bundles under `src-tauri/target/release/bundle/`.

### Useful sub-commands

| Command | Description |
| --- | --- |
| `npm run dev` | Frontend-only Vite dev server |
| `npm run build` | Type-check (`tsc`) and build the frontend |
| `cargo build` | Build the full Rust workspace |
| `cargo test` | Run Rust tests across the workspace |
| `npm run plugins:dev` | Build the bundled plugins and install them into `dev-plugins/` |
| `./start-dev.sh` | Launch the app (uses `dev-plugins/`; pass `--build` to rebuild plugins first, `--debug` for `RUST_LOG=debug`) |

---

## Releasing (CI)

Three GitHub Actions workflows in `.github/workflows/` build and publish
artifacts. All builds are cross-platform (macOS arm64/x64, Linux arm64/x64,
Windows arm64/x64) and the version is **stamped at build time** — never
committed back to the repo, so there is no push → build → push loop.

| Workflow | Trigger | Output |
| --- | --- | --- |
| `publish-app.yml` | push to `main`, `v*` tag, manual | Desktop installers/bundles → a GitHub Release |
| `publish-plugins.yml` | push to `main`, `v*` tag, manual | All plugin binaries + aggregate `SHA256SUMS` → one shared Release (`plugins-latest` / `plugins-v*`) |
| `tag.yml` | manual (`workflow_dispatch`) | Computes and pushes the next `vX.Y.Z` tag |

### Versioning

Every artifact shares one version `<major>.<minor>.<patch>`:

- **`major.minor`** comes from `Cargo.toml` / `src-tauri/tauri.conf.json`. Bump
  these in source when you want a new minor/major line.
- **`patch`** is the **git commit count** (`git rev-list --count HEAD`) for
  nightlies, or the **tag's** patch for tagged releases.

### Nightly (every push to `main`)

Rolling **prereleases**, overwritten each push:

- App → release tagged `latest`, version `<major>.<minor>.<commit-count>`.
- Plugins → a single rolling release tagged `plugins-latest`, holding every
  plugin's binaries as assets (one per target triple).

### Stable (push a `v*` tag)

Immutable releases with the version taken from the tag (`v0.2.0` → `0.2.0`):

- App → release at the tag `v0.2.0`.
- Plugins → a single release tagged `plugins-v0.2.0` holding every plugin's
  binaries as assets (one per target triple, named `polyclient-plugin-<id>-<triple>`),
  plus an aggregate `SHA256SUMS`. The installer picks the asset by plugin id +
  target triple, so all plugins ship in one release.

Cut a release either by hand or via the tag workflow:

```bash
# By hand:
git tag v0.2.0 && git push origin v0.2.0

# Or run the "Tag release" workflow from the Actions tab
# (choose patch/minor/major, or type an explicit version).
```

> **Heads-up:** a tag pushed by the `tag.yml` workflow using the default
> `GITHUB_TOKEN` will **not** trigger the publish workflows (GitHub blocks
> token-pushed events from triggering further workflows). To make tagging
> auto-publish, add a Personal Access Token with `contents: write` as a repo
> secret named **`RELEASE_TOKEN`**. Pushing a tag manually from your machine
> always triggers them.

---

## Plugins

The host ships with only `polyclient-core` and loads plugins at runtime from the
plugins directory (`$POLYCLIENT_PLUGINS_DIR`, else `<app-data>/plugins`). Each plugin
is a standalone executable plus a generated `<id>.plugin.json` manifest; the
host talks to it over line-delimited JSON-RPC on stdio.

### Install from GitHub (in-app)

Click **⤓ Install plugin** in the sidebar, enter a repo (`owner/name`) and
optionally a release tag, then **Fetch release**. The app picks the asset
matching your OS/arch, shows the published SHA-256, and — on confirmation —
downloads, verifies the checksum, and installs it. The plugin appears in the
New-connection form immediately, no rebuild required.

> ⚠️ Plugins are native executables that run with full access to your machine.
> Only install plugins you trust. Downloads are checksum-verified for integrity,
> not authenticity; on macOS an unsigned/un-notarized binary may be blocked by
> Gatekeeper on first launch.

### Publishing a plugin (release convention)

For a plugin repo to be installable, a GitHub Release must include, for each
plugin it ships (one or many can share a release):

- **One binary per platform** whose asset name contains the plugin id and the
  Rust **target triple**, e.g. `polyclient-plugin-mysql-aarch64-apple-darwin`
  (`…-x86_64-pc-windows-msvc.exe` on Windows). The installer picks the asset by
  plugin id + target triple, so several plugins can live in one release.
- **A checksums asset**: `SHA256SUMS` (or `checksums.txt`) with
  `<sha256>  <asset-name>` lines, or a per-asset `<asset-name>.sha256`.

No manifest is shipped — the host generates it by running the verified binary's
`--describe` at install time. Build a plugin as a binary crate whose `main`
calls `polyclient_plugin_runtime::run(plugin, dispatcher)`.

### Local development

`npm run plugins:dev` builds the three bundled plugins, copies the binaries into
`dev-plugins/`, and writes their manifests (via each binary's `--describe`).
Set `POLYCLIENT_PLUGINS_DIR=$PWD/dev-plugins` when launching the app.

---

## Using the app

1. Click **+** in the sidebar to create a connection. Pick a plugin
   (PostgreSQL / MongoDB / RabbitMQ); the form is generated from that plugin's
   declared config schema.
2. **Test** the config, then **Save**. Profiles persist across restarts.
3. Select a saved connection to **connect**. The workspace that opens depends
   on the plugin kind:
   - **RDBMS** — schema/table tree, SQL editor, results grid, and inline cell
     editing with staged Save/Cancel.
   - **Document** — browse databases/collections and run `find` queries.
   - **RabbitMQ** — browse the broker overview, queues, exchanges, and
     connections; declare queues and publish/get/purge messages.
   - **CLI (SSH)** — full PTY terminal for the SSH session, with a script
     editor pane on the right. Save scripts as `.sh` files scoped to the
     profile. Run a selection, the current line (`Cmd/Ctrl+Enter`), or the
     whole script. The SSH session survives switching connections and
     remounts.

### Where data is stored

Saved connection profiles live under `<app_data_dir>/connections/`, one file per
owning plugin: `<app_data_dir>/connections/<plugin_id>/connections.json` (the
exact base path is OS-specific). The files are human-readable JSON. Profiles
whose plugin is no longer installed are skipped on load but left on disk.

> ⚠️ **Security note:** connection configs — **including passwords** — are
> stored in plaintext. Treat the files accordingly. A secure-credential store is
> not yet implemented.

---

## Adding a new plugin

The shared traits make new backends small to add. For a relational backend:

1. Create a **binary** crate under `crates/plugins/<name>` depending on
   `polyclient-core`, `polyclient-plugin-runtime`, and (for relational backends)
   `polyclient-rdbms-common`. Add it to the workspace `members` in `Cargo.toml`.
2. Implement `polyclient_core::Plugin` — return a `PluginInfo` with
   `kind: PluginKind::Rdbms` and a `config_schema` describing the connection
   form, plus a `connect` that opens and returns an `Arc<dyn Connection>`.
3. For relational backends, also implement `RdbmsPlugin`
   (`list_schemas`, `list_tables`, `describe_table`, `execute`, and optionally
   `apply_changes` for editing).
4. In `main`, call `polyclient_plugin_runtime::run(plugin, dispatcher)` — for RDBMS use
   `RdbmsDispatcher(plugin)`. There is **no central registry to edit**; the host
   discovers the plugin from its manifest at runtime.
5. To run it in dev, add the crate to the `PLUGINS`/`CRATES` arrays in
   `scripts/dev-plugins.sh` so `npm run plugins:dev` builds and installs it.

Non-relational backends pick a different `PluginKind` (`Document`, `Rabbitmq`,
`Cli`, or `Other`) and the frontend renders the matching workspace. `Cli`-kind
plugins return a `PtySpawnSpec` from the `cli.spawn_spec` op; the host spawns
that program in a PTY and streams I/O via Tauri events. See the SSH plugin
(`crates/plugins/ssh`) and the existing MongoDB and RabbitMQ plugins as
references.

---

## For AI agents working in this repo

- **Stack:** Tauri 2 + Rust workspace backend; React 18 + TypeScript + Vite
  frontend. Communication is via Tauri `invoke` commands defined in
  `src-tauri/src/commands.rs` and mirrored in `src/api.ts`.
- **The type contract lives in two mirrored places.** Any change to a
  cross-boundary struct in Rust (`polyclient-core`, `polyclient-rdbms-common`, or a plugin)
  must be reflected in `src/api.ts`. serde uses `rename_all = "lowercase"` for
  enums and `camelCase` for saved-connection persistence — match the existing
  casing.
- **The host is a generic pipe.** It exposes a small set of generic
  `#[tauri::command]`s (`list_plugins`, `test_connection`, `open_connection`,
  `close_connection`, `plugin_call`, the GitHub install pair, persistence, and
  PTY commands: `pty_spawn`/`pty_write`/`pty_resize`/`pty_close`/`pty_snapshot`).
  Every capability funnels through `plugin_call` with an opaque `op` string
  (e.g. `"rdbms.execute"`, `"cli.spawn_spec"`) — only the typed wrappers in
  `src/api.ts` know the op names. Plugins are **out-of-process sidecars**
  discovered from `*.plugin.json` manifests at runtime; there is no static
  registry. See `plugin-architecture.md` for the design.
- **Live connection handles never cross the IPC boundary.** They stay inside the
  plugin process keyed by `ConnectionId` (a UUID); the host only tracks the
  `ConnectionId → plugin_id` route. Don't try to serialize a pool or client.
- **Build/run:** `./start-dev.sh --build` (or `npm run plugins:dev` then
  `POLYCLIENT_PLUGINS_DIR=$PWD/dev-plugins npm run tauri dev`) for end-to-end (the form
  is empty without plugins installed); `cargo test` for backend unit tests;
  `npm run build` to type-check the frontend.

---

## License

MIT (see [`LICENSE`](LICENSE)).


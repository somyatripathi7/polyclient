# Plugin-Provided UI: Declarative Workspaces + Web-Asset Escape Hatch

> Companion to [`plugin-architecture.md`](./plugin-architecture.md), which made the
> **backend** of a plugin self-contained (out-of-process sidecars, generic pipe).
> This doc does the same for the **frontend**: today a plugin still cannot ship its
> own workspace UI.

## Context

The transport is already fully generic: `plugin_call(connectionId, op, params)`
(`src-tauri/src/commands.rs`) forwards any opaque `op` to the owning sidecar. The
host needs no per-kind knowledge. But the **frontend is frozen at app-build time**:

- `src/App.tsx:145` `renderWorkspace()` is a hardcoded `switch (ui_module)` over
  three compiled-in components (`RdbmsWorkspace`, `DocumentWorkspace`,
  `RabbitMqWorkspace`).
- `src/api.ts` hardcodes the `op` names (`rdbms.execute`, `document.find`, …).
- `src-tauri/tauri.conf.json` sets `frontendDist: ../dist` — the webview loads one
  bundle baked into the app binary.

So a plugin installed at runtime (`install_github_plugin`) with a novel surface has
**no way to render** — it falls through to the `"No workspace available"`
placeholder. Adding any new surface requires rebuilding and reshipping the host.

**Goal:** adding a plugin — including a runtime-installed one — needs **zero
host-frontend rebuild** for the common case, with an escape hatch for plugins that
genuinely need bespoke UI.

**Decided approach (confirmed with the user): hybrid, declarative-first.**
- **Phase A — Declarative workspace schema.** Extend what `config_schema` already
  does for the connection form to the *whole workspace*. The plugin declares its
  surface in `PluginInfo`; one generic host renderer interprets it and issues raw
  `plugin_call`s named in the spec. No runtime code execution; works with the
  frozen bundle.
- **Phase B — Web-asset escape hatch.** A plugin may instead ship built web assets
  the host serves into a sandboxed iframe, for surfaces the schema can't express.
- `ui_module` becomes the **discriminator** that picks the renderer.

Phase A is the whole removal of the "rebuild the frontend to add a plugin"
coupling for the 80% case. Phase B is a larger, security-sensitive lift taken on
only when a plugin needs a custom surface.

## Why this is feasible (key facts from exploration)

- **The three workspaces are structurally the same shape:** a lazily-loaded
  **navigator tree** on the left, and a **main panel** that is an *input* (SQL
  editor / find filter / publish form) feeding an *op* whose *result* renders as
  one of {table, JSON list, message list}. That shape is small enough to describe
  declaratively.
- **Precedent already exists.** `config_schema` + `ConfigField` + `ShowIf`
  (`crates/polyclient-core/src/lib.rs`) already drive the entire connection form
  generically from a plugin-declared spec, mirrored in `src/api.ts`. Phase A is the
  same pattern applied one level up.
- **No new backend ops needed for Phase A.** The existing Postgres/Mongo/Rabbit
  plugins already expose every op a declarative workspace would call
  (`rdbms.list_schemas`, `document.find`, `rabbitmq.publish`, …). Phase A can
  render them by *writing a `workspace_spec`*, with the plugin's Rust unchanged.
- **The hand-written components are not thrown away.** `ui_module: "rdbms"` keeps
  rendering the polished, compiled `RdbmsWorkspace` (with its staged multi-cell
  editing) — the declarative path is opt-in per plugin.

## `ui_module` as the renderer discriminator

`App.tsx`'s `renderWorkspace()` resolves in this precedence:

1. `ui_module` names a **built-in module** (`"rdbms"` | `"document"` | `"rabbitmq"`)
   → render the existing compiled component (today's behavior, unchanged).
2. `ui_module == "webview"` **and** the plugin ships UI assets → `<PluginWebview>`
   (Phase B).
3. The plugin declares a **`workspace_spec`** → `<SchemaWorkspace spec=…>` (Phase A).
4. Otherwise → the `"No workspace available"` placeholder.

This keeps every current plugin working while letting a new plugin pick declarative
or webview without host changes.

---

# Phase A — Declarative workspace schema

## Vocabulary (new types in `polyclient-core`, mirrored in `api.ts`)

A `WorkspaceSpec` lives on `PluginInfo` (optional, like `ui_module`). It has two
parts: a **navigator** (the left tree) and a **main panel**. Op params are
templated against a small **context** of variables that navigator selection and
form inputs populate (e.g. `{schema}`, `{table}`, `{database}`).

```jsonc
// PluginInfo.workspace_spec  (illustrative — Postgres expressed declaratively)
{
  "navigator": {
    // Ordered levels; each level is fetched by an op, lazily, using the
    // context accumulated from the levels above it.
    "levels": [
      { "var": "database", "listOp": "rdbms.list_databases",
        "onSelectOp": "rdbms.use_database", "param": "database" },
      { "var": "schema",   "listOp": "rdbms.list_schemas",   "labelField": "name" },
      { "var": "table",    "listOp": "rdbms.list_tables",
        "params": { "schema": "{schema}" }, "labelField": "name", "leaf": true }
    ]
  },
  "main": {
    "kind": "query",                       // query | form | tabs of these
    "input":  { "kind": "code", "language": "sql", "param": "sql" },
    "submitOp": "rdbms.execute",
    "result": { "kind": "table" }          // table | json | messages
  }
}
```

### Element kinds (the minimal set that covers the three workspaces)

- **Navigator levels** — each `{ var, listOp, params?, labelField?, leaf?,
  onSelectOp?, param? }`. Selecting a node binds `var` in context (and may fire
  `onSelectOp`, e.g. `use_database`). `params` values are **templates** resolved
  against current context.
- **Input**
  - `code` — a text/code editor (optional `language` hint) bound to a `param`.
  - `form` — **reuse `ConfigField[]`** (the connection-form vocabulary) for typed
    inputs (text/number/select/boolean), each bound to a `param`.
- **submitOp** — the `op` invoked on run/submit; receives `{ ...formParams,
  ...contextVars }`.
- **result** renderer
  - `table` — expects `QueryResult` (`columns` + `rows`).
  - `json` — expects `{ documents: unknown[] }` (Mongo `FindResult`).
  - `messages` — expects `ConsumedMessage[]` / single message.
- **actions** (optional) — buttons bound to an `op` with templated params
  (e.g. RabbitMQ's *Declare* / *Publish* / *Get*).

### Worked example: Document and RabbitMQ

These are the **Phase-A reference migrations** (simpler than RDBMS):

```jsonc
// MongoDB
{ "navigator": { "levels": [
    { "var": "database",   "listOp": "document.list_databases", "leaf": false },
    { "var": "collection", "listOp": "document.list_collections",
      "params": { "database": "{database}" }, "labelField": "name", "leaf": true } ] },
  "main": { "kind": "query",
    "input":  { "kind": "code", "language": "json", "param": "filter" },
    "submitOp": "document.find",
    "params": { "database": "{database}", "collection": "{collection}", "limit": 50 },
    "result": { "kind": "json" } } }

// RabbitMQ — a form + actions, no navigator tree
{ "navigator": null,
  "main": { "kind": "form",
    "input": { "kind": "form", "fields": [ { "key": "queue", "label": "Queue", "type": { "kind": "text" } } ] },
    "actions": [
      { "label": "Declare",   "op": "rabbitmq.declare_queue", "params": { "queue": "{queue}" }, "result": { "kind": "json" } },
      { "label": "Publish",   "op": "rabbitmq.publish",       "params": { "queue": "{queue}", "body": "{body}" } },
      { "label": "Get 10",    "op": "rabbitmq.get_messages",  "params": { "queue": "{queue}", "count": 10 }, "result": { "kind": "messages" } } ] } }
```

## The generic renderer (`<SchemaWorkspace>`)

One new frontend component, `src/components/workspaces/SchemaWorkspace.tsx`:

- Holds **context state** (`Record<var, value>`) and the navigator's loaded nodes.
- Renders the navigator by walking `levels`: each level calls its `listOp` via
  `pluginCall` with templated params; expanding/selecting a node sets context and
  triggers the next level (or `onSelectOp`).
- Renders the main panel from `input` + `submitOp`/`actions`, resolving param
  templates (`{var}`) against context + form state, calling `plugin_call`, and
  dispatching the response to the declared `result` renderer (reusing the existing
  table/JSON/message presentation extracted from the current components).
- Errors funnel through the existing `errString` + status-line path — unchanged.

Template resolution is deliberately tiny: `{var}` → `context[var]`; missing vars
disable the action with a hint. No expressions, no logic — the plugin describes
*what to call*, the renderer owns *how to display*.

## Protocol & host impact (Phase A)

**None.** The declarative renderer issues ordinary `plugin_call`s over the existing
pipe. `PluginInfo` gains an optional `workspace_spec` field (serde-defaulted so old
manifests still deserialize, exactly like `ui_module`/`protocol_version` did). The
host forwards it verbatim — it remains a generic pipe and never interprets the spec.

## Type contract

`WorkspaceSpec` (and its sub-types) are `Serialize`/`Deserialize` in `polyclient-core` and
mirrored in `src/api.ts`, following the same casing rules as `config_schema`
(lowercase enums, explicit renames where needed). This is the usual two-mirrored-
places obligation called out in CLAUDE.md.

## Honest limits of Phase A

The 695-line `RdbmsWorkspace` has a hard tail the schema vocabulary does **not**
attempt to express in v1:

- **Staged, multi-cell inline editing** committing via `rdbms.apply_changes` with
  CAST-typed `ColumnValue`s.

Two ways out, deferred: (a) add an `editableTable` result capability that maps grid
edits to `apply_changes` (grows the vocabulary), or (b) such plugins use Phase B.
Until then, RDBMS stays on its compiled `ui_module: "rdbms"` component. Phase A's
proof is migrating **Document** and **RabbitMQ** to specs and deleting their
bespoke components.

## Implementation steps (Phase A)

1. **`polyclient-core`**: add `WorkspaceSpec` + sub-types; add optional
   `workspace_spec: Option<WorkspaceSpec>` to `PluginInfo` (serde default `None`).
2. **`api.ts`**: mirror the types; expose `workspace_spec` on `PluginInfo`. Keep the
   typed `op` wrappers for now (the built-in components still use them).
3. **`SchemaWorkspace.tsx`**: the generic renderer (navigator + main + result
   renderers extracted/shared from the existing components).
4. **`App.tsx`**: extend `renderWorkspace()` precedence (built-in module → webview →
   `workspace_spec` → placeholder).
5. **Reference migration**: add `workspace_spec` to the **mongodb** and **rabbitmq**
   plugins' `PluginInfo`; confirm `SchemaWorkspace` reproduces their behavior; then
   delete `DocumentWorkspace.tsx` / `RabbitMqWorkspace.tsx` and their `ui_module`
   cases.
6. **Dev loop**: `npm run plugins:dev` already regenerates manifests via
   `--describe`, so the new `workspace_spec` flows into `dev-plugins/` automatically.

## Verification (Phase A)

- **Unit**: `WorkspaceSpec` round-trips through serde; an old manifest with no
  `workspace_spec` still deserializes.
- **Renderer**: with a mock `pluginCall`, navigator levels load lazily, selection
  binds context, templated params resolve, and each `result.kind` renders.
- **End-to-end (local Mongo + Rabbit)**: drive both *entirely* through
  `SchemaWorkspace` with their bespoke components deleted — browse, query/find,
  declare/publish/consume — proving a plugin owns its workspace with **no compiled
  component and no host rebuild**.
- **Decoupling check**: hand-write a 4th plugin manifest with only a
  `workspace_spec` (no matching built-in `ui_module`); confirm it renders a working
  workspace purely from discovery.

---

# Phase B — Web-asset escape hatch (later)

For surfaces the schema can't express (charts, graph/topology views, the full RDBMS
editing grid), a plugin ships **built web assets** and the host renders them in a
**sandboxed iframe**.

- **Packaging**: the plugin bundle/manifest gains a `ui` entry pointing at a
  directory of built static assets (`index.html` + JS/CSS) installed alongside the
  binary. `ui_module: "webview"`.
- **Serving**: the host registers a Tauri **custom URI scheme**
  (`plugin://<id>/…`, via `register_uri_scheme_protocol`) that serves files from the
  plugin's install dir. The webview loads `plugin://<id>/index.html` in an
  `<iframe>`.
- **Bridge**: a tiny injected JS SDK gives the iframe `polyclient.call(op, params)` and
  lifecycle only. It posts messages to the host; a host listener validates and
  forwards to `plugin_call` **scoped to the connection that iframe owns** — a plugin
  UI cannot drive another plugin's connection, and gets no direct `invoke` access.
- **Security (the crux, and why this is Phase B)**: downloaded JS now runs inside
  the app. Requires a strict CSP, no Node/Tauri-API access from the iframe, the
  connection-scoped bridge above, and a clear trust prompt at install (mirroring the
  existing checksum/Gatekeeper warnings for native plugin binaries). Bridge API
  gets its own version, distinct from the stdio `PROTOCOL_VERSION`.

Phase B reuses everything below the bridge unchanged — it still terminates in
`plugin_call` over the same sidecar pipe. It is intentionally **out of scope until a
concrete plugin needs it**.

## Risks / tradeoffs

- **Vocabulary creep (A)**: every "just one more element kind" enlarges the
  renderer. Hold the line: the schema describes *what op to call and how to show the
  result*; anything needing real logic is a Phase-B candidate, not a new element.
- **Two UI paths to maintain**: built-in components, declarative, and (later)
  webview. Mitigated by migrating Document/RabbitMQ *off* bespoke components in
  Phase A, so declarative is the default and built-ins shrink rather than grow.
- **Untrusted code (B)**: native plugin binaries already run with full privilege
  (noted in `plugin-architecture.md`); a sandboxed iframe is *stricter* than that,
  but it widens the in-app attack surface and must be designed deliberately.
- **Versioning**: `workspace_spec` is gated by the same install-time
  `protocol_version` check; the Phase-B bridge carries its own version.

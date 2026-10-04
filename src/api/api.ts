// Typed bridge to the Rust `#[tauri::command]` surface in `src-tauri/src/commands.rs`.
// Types mirror the serde representations of the Rust structs.
//
// The host is a generic pipe: lifecycle commands (list/test/open/close) are
// typed, but every capability call funnels through one `plugin_call` command
// with an opaque `op` string. Only the wrappers below know the op names — the
// workspace components are unchanged.

import { invoke } from "@tauri-apps/api/core";
import { rdbms_api } from "./rdbms.ts";
import { document_api } from "./document.ts";
import { rabbitmq_api } from "./rabbitmq.ts";
import { sftp_api } from "./sftp.ts";

export type PluginKind =
  "rdbms" | "document" | "rabbitmq" | "cli" | "filemanager" | "http" | "other";

export type ConfigFieldType =
  | { kind: "text" }
  | { kind: "password" }
  | { kind: "number" }
  | { kind: "boolean" }
  | { kind: "select"; options: string[] }
  | { kind: "filepath" }
  | { kind: "keyvalue" };

export interface ShowIf {
  field: string;
  equals: string;
}

/** How a secret is stored. Mirrors Rust `SecretField`'s `type` tag. Only
 *  `PLAIN_TEXT` exists today; future variants (keychain/env/encrypted) extend
 *  this without changing the field shape. */
export type SecretType = "PLAIN_TEXT";

/** A credential value in a `ConnectionConfig`. Every `password`-kind field
 *  stores one of these (`{ type, value }`) rather than a bare string. */
export interface SecretField {
  type: SecretType;
  value: string;
}

/** Wrap a plaintext credential as a `SecretField`. */
export const plainSecret = (value: string): SecretField => ({
  type: "PLAIN_TEXT",
  value,
});

export interface ConfigField {
  key: string;
  label: string;
  type: ConfigFieldType;
  required: boolean;
  default?: unknown | null;
  placeholder?: string | null;
  /** When set, only show this field while `values[field]` equals `equals`. */
  show_if?: ShowIf | null;
}

export interface PluginInfo {
  id: string;
  name: string;
  kind: PluginKind;
  version: string;
  description: string;
  config_schema: ConfigField[];
  ui_module?: string | null;
  /** Wire-protocol version the plugin speaks (set by the plugin runtime). */
  protocol_version?: number;
}

/** What the host reports about a GitHub release before installing (see
 *  `preview_github_plugin`). `sha256` is null when no checksum is published. */
export interface GithubPreview {
  repo: string;
  tag: string;
  assetName: string;
  sizeBytes: number;
  sha256: string | null;
  downloadUrl: string;
}

/** Install/update state of an `AvailablePlugin` relative to what's installed. */
export type PluginStatus = "not_installed" | "up_to_date" | "update_available" | "unknown";

/** A plugin installable from the configured GitHub repo (see
 *  `list_github_plugins`). All plugins share one release tag (`plugins-latest`
 *  or `plugins-v<semver>`); `id` is derived from the asset name
 *  (e.g. `polyclient-plugin-postgres-<triple>` -> `postgres`) and equals the installed
 *  plugin's id. `status` is computed by the host against what's installed. */
export interface AvailablePlugin {
  id: string;
  tag: string;
  /** `"nightly"` or `"stable"` — the channel this listing reflects. */
  channel: string;
  /** Human-facing name from the release's `plugin_info.json`, if published. */
  name?: string | null;
  /** Plugin description from `plugin_info.json`, if published. */
  description?: string | null;
  assetName: string;
  sizeBytes: number;
  /** Available version (from the tag for stable, or `plugin_info.json` for nightly). */
  availableVersion: string | null;
  /** Release publish timestamp (informational; `status` is computed host-side). */
  publishedAt: string | null;
  /** The currently-installed version, if installed. */
  installedVersion: string | null;
  status: PluginStatus;
}

/** Serializes as a UUID string. */
export type ConnectionId = string;

export type ConnectionConfig = Record<string, unknown>;

// --- Commands -------------------------------------------------------------

/** Forward an opaque capability call to the plugin owning the connection. */
export const pluginCall = <T>(
  connectionId: ConnectionId,
  op: string,
  params: Record<string, unknown>,
) => invoke<T>("plugin_call", { connectionId, op, params });

export const api = {
  listPlugins: () => invoke<PluginInfo[]>("list_plugins"),

  /** The release channel this app build tracks (`"nightly"` or `"stable"`). */
  appChannel: () => invoke<string>("app_channel"),

  testConnection: (pluginId: string, config: ConnectionConfig) =>
    invoke<void>("test_connection", { pluginId, config }),

  openConnection: (pluginId: string, config: ConnectionConfig) =>
    invoke<ConnectionId>("open_connection", { pluginId, config }),

  closeConnection: (connectionId: ConnectionId) =>
    invoke<void>("close_connection", { connectionId }),

  // Plugin install (from GitHub releases)
  listGithubPlugins: (repo: string) => invoke<AvailablePlugin[]>("list_github_plugins", { repo }),

  previewGithubPlugin: (repo: string, tag?: string | null, pluginId?: string | null) =>
    invoke<GithubPreview>("preview_github_plugin", {
      repo,
      tag: tag ?? null,
      pluginId: pluginId ?? null,
    }),

  installGithubPlugin: (repo: string, tag: string, pluginId: string, expectedSha: string | null) =>
    invoke<PluginInfo>("install_github_plugin", { repo, tag, pluginId, expectedSha }),

  /** Uninstall a plugin (stops its process, deletes its files). Rejects if the
   *  plugin has open connections. */
  uninstallPlugin: (pluginId: string) => invoke<void>("uninstall_plugin", { pluginId }),

  /** Cancel the in-flight plugin call for a connection (aborts it on the server). */
  cancelLastPluginCall: (connectionId: ConnectionId) =>
    invoke<void>("cancel_last_plugin_call", { connectionId }),

  ...rdbms_api,
  ...document_api,
  ...rabbitmq_api,
  ...sftp_api,
};

/** Normalize a thrown Tauri command error into a string. */
export function errString(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof Error) return e.message;
  return JSON.stringify(e);
}

// ---------------------------------------------------------------------------
// PTY (CLI / SSH workspace)
// ---------------------------------------------------------------------------
//
// A CLI connection can host several terminal tabs, so PTYs are keyed by a
// frontend-minted `terminalId` (a UUID), not the connection id. `ptySpawn`
// still needs the connection id to route the `cli.spawn_spec` query to the
// owning plugin; everything else addresses a terminal directly.

/** Spawn the CLI plugin's terminal process in a PTY for `terminalId`. The host
 * asks the owning plugin how to launch it (`cli.spawn_spec`, routed by
 * `connectionId`), so no config is sent from here. Idempotent: a no-op if the
 * terminal's PTY is already running. */
export function ptySpawn(connectionId: ConnectionId, terminalId: string): Promise<void> {
  return invoke("pty_spawn", { connectionId, terminalId });
}

/** Send raw bytes (keystrokes / paste) to a terminal's PTY. */
export function ptyWrite(terminalId: string, data: number[]): Promise<void> {
  return invoke("pty_write", { terminalId, data });
}

/** Notify a terminal's PTY of a resize. */
export function ptyResize(terminalId: string, cols: number, rows: number): Promise<void> {
  return invoke("pty_resize", { terminalId, cols, rows });
}

/** Close and drop a single terminal's PTY. */
export function ptyClose(terminalId: string): Promise<void> {
  return invoke("pty_close", { terminalId });
}

/** Close and drop every terminal PTY owned by a connection (teardown on
 * disconnect / delete). */
export function ptyCloseConnection(connectionId: ConnectionId): Promise<void> {
  return invoke("pty_close_connection", { connectionId });
}

/** Retained scrollback (recent output bytes) for a terminal's PTY, so a
 * remounted terminal can repaint its history. Empty if no live PTY. */
export function ptySnapshot(terminalId: string): Promise<number[]> {
  return invoke("pty_snapshot", { terminalId });
}

// Persistent store for saved connection profiles.
//
// Profiles live in per-plugin JSON files in the OS app-data dir
// (connections/<pluginId>/connections.json), written by the Rust backend (see
// src-tauri/src/persistence.rs). The backend merges them into one list on load
// and splits them back out by plugin on save. This module is the single place
// the UI touches persistence.
//
// NOTE: configs are stored as-is, including any password field, in plaintext.

import { invoke } from "@tauri-apps/api/core";
import type { ConnectionConfig } from "./api.ts";

/** A reusable connection the user has saved. `id` is stable across edits so
 * the sidebar selection and open-connection mapping survive updates. */
export interface SavedConnection {
  id: string;
  name: string;
  pluginId: string;
  config: ConnectionConfig;
  /** Sidebar sort position (ascending). Defaults to 0; only set when the user
   * reorders the list via drag-and-drop. */
  order?: number;
  /** Per-connection UI preferences (e.g. `treeWidth`, `editorHeight`), stored
   * alongside the connection. Free-form so future prefs need no schema change. */
  settings?: Record<string, unknown>;
}

/** Generate a stable profile id (falls back when crypto.randomUUID is absent). */
export function genId(): string {
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
    return crypto.randomUUID();
  }
  return "c-" + Math.random().toString(36).slice(2) + Date.now().toString(36);
}

/** Load all saved profiles from disk. */
export function loadConnections(): Promise<SavedConnection[]> {
  return invoke<SavedConnection[]>("load_connections");
}

/** Persist the full set of saved profiles to disk. */
export function saveConnections(connections: SavedConnection[]): Promise<void> {
  return invoke<void>("save_connections", { connections });
}

/** Pure: insert or replace `conn` (matched by id). Returns the new list. */
export function upsert(list: SavedConnection[], conn: SavedConnection): SavedConnection[] {
  const i = list.findIndex((c) => c.id === conn.id);
  return i >= 0 ? list.map((c) => (c.id === conn.id ? conn : c)) : [...list, conn];
}

/** Pure: remove the profile with `id`. Returns the new list. */
export function remove(list: SavedConnection[], id: string): SavedConnection[] {
  return list.filter((c) => c.id !== id);
}

// --- App config -----------------------------------------------------------
//
// App-wide UI state (not tied to any plugin/connection), persisted by the Rust
// backend at <app_data_dir>/config.json. See src-tauri/src/config.rs.

/** App-wide configuration. Mirrors the Rust `AppConfig` (camelCase). */
export interface AppConfig {
  /** Whether the first-run plugin install step has been shown to the user. */
  pluginsDialogShown: boolean;
  /** GitHub repo (`owner/name`) the in-app plugin installer fetches from. */
  pluginRepo: string;
  /** Sidebar width in CSS pixels, set by dragging the sidebar's resize handle. */
  sidebarWidth: number;
  /** When true, the sidebar collapses to a narrow rail and expands on hover. */
  sidebarCollapsible: boolean;
  /** Active UI theme id (see `THEMES` in `src/theme.ts`). */
  theme: string;
}

/** Load the app config, creating it with defaults on first run. */
export function loadConfig(): Promise<AppConfig> {
  return invoke<AppConfig>("load_config");
}

/** Persist the full app config. */
export function saveConfig(config: AppConfig): Promise<void> {
  return invoke<void>("save_config", { config });
}

// --- Per-connection workspace files ---------------------------------------
//
// Plain files saved per connection profile at
// <app_data_dir>/workspace/<connectionId>/<name>.<ext>. See
// src-tauri/src/workspace_files.rs. `connectionId` here is the stable
// saved-profile id, so files persist across sessions (not the per-session live
// connection id). `ext` is the file extension (default "sql"; the SSH/CLI
// workspace uses "sh").

/** A saved workspace file. `name` is the file name without its extension. */
export interface WorkspaceFile {
  /** File name without the extension. */
  name: string;
  content: string;
}

/** List the saved workspace files with extension `ext` for a connection profile,
 * sorted by name. A thin wrapper over the generic dir/file primitives below:
 * lists the profile root, keeps `*.<ext>` files, and reads each. */
export async function listWorkspaceFiles(
  connectionId: string,
  ext = "sql",
): Promise<WorkspaceFile[]> {
  const suffix = `.${ext}`;
  const entries = await listWorkspaceDir(connectionId, "");
  const out: WorkspaceFile[] = [];
  for (const e of entries) {
    if (e.isDir || !e.name.endsWith(suffix)) continue;
    const content = await readWorkspaceFile(connectionId, e.name);
    if (content !== null) {
      out.push({ name: e.name.slice(0, -suffix.length), content });
    }
  }
  return out;
}

/** Create or overwrite a named workspace file (`<name>.<ext>`) for a profile. */
export function saveWorkspaceFile(
  connectionId: string,
  name: string,
  content: string,
  ext = "sql",
): Promise<void> {
  return writeWorkspaceFileAt(connectionId, `${name}.${ext}`, content);
}

/** Delete a named workspace file (`<name>.<ext>`) from a profile. */
export function deleteWorkspaceFile(
  connectionId: string,
  name: string,
  ext = "sql",
): Promise<void> {
  return deleteWorkspacePath(connectionId, `${name}.${ext}`);
}

/** Delete a connection profile's entire workspace folder (all saved files of
 * every extension). Used when the profile itself is deleted. */
export function deleteWorkspaceDir(connectionId: string): Promise<void> {
  return deleteWorkspacePath(connectionId, "");
}

// --- Generic per-connection path operations --------------------------------
//
// Lower-level primitives for storing an arbitrary directory tree under
// <app_data_dir>/workspace/<connectionId>/. `path` is a `/`-separated relative
// path; the backend validates every segment. See src-tauri/src/workspace_files.rs.
// `connectionId` is the stable saved-profile id, so files persist across sessions.

/** One immediate child of a workspace directory. Mirrors the Rust `DirEntry`. */
export interface DirEntry {
  name: string;
  isDir: boolean;
}

/** Read a single workspace file. Resolves to `null` when it doesn't exist. */
export function readWorkspaceFile(connectionId: string, path: string): Promise<string | null> {
  return invoke<string | null>("read_workspace_file", { connectionId, path });
}

/** Write a single workspace file at `path`, creating parent dirs. */
export function writeWorkspaceFileAt(
  connectionId: string,
  path: string,
  content: string,
): Promise<void> {
  return invoke<void>("write_workspace_file_at", { connectionId, path, content });
}

/** List the immediate children of a workspace directory (sorted by name). Empty
 * when the directory doesn't exist. The caller recurses into subdirs itself. */
export function listWorkspaceDir(connectionId: string, path: string): Promise<DirEntry[]> {
  return invoke<DirEntry[]>("list_workspace_dir", { connectionId, path });
}

/** Delete the file or directory (recursively) at `path`. Missing = success. */
export function deleteWorkspacePath(connectionId: string, path: string): Promise<void> {
  return invoke<void>("delete_workspace_path", { connectionId, path });
}

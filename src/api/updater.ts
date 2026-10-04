// Thin bridge to the app self-update commands in `src-tauri/src/update.rs`.
//
// The host owns the channel-aware updater; the frontend just triggers a check,
// drives install with progress, and relaunches. `check_update` returns null
// when up to date and rejects when the updater is unavailable (e.g. under
// `tauri dev`) — callers decide whether to surface or swallow that.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { relaunch } from "@tauri-apps/plugin-process";

/** Mirrors the Rust `UpdateInfo` (camelCase). */
export interface UpdateInfo {
  version: string;
  currentVersion: string;
  notes: string | null;
}

/** Check this build's release channel for a newer signed build. Returns null
 *  when already up to date. */
export function checkForUpdate(): Promise<UpdateInfo | null> {
  return invoke<UpdateInfo | null>("check_update");
}

/** One `update://progress` event from the host. */
interface ProgressEvent {
  chunk: number;
  total: number | null;
}

/** Download + verify + install the available update, then relaunch the app.
 *  `onProgress(downloaded, total)` fires as bytes arrive (`total` may be null
 *  if the server sends no length). Resolves only if relaunch doesn't occur. */
export async function installUpdate(
  onProgress?: (downloaded: number, total: number | null) => void,
): Promise<void> {
  let downloaded = 0;
  const unlisten = await listen<ProgressEvent>("update://progress", (e) => {
    downloaded += e.payload.chunk;
    onProgress?.(downloaded, e.payload.total);
  });
  try {
    await invoke<void>("install_update");
  } finally {
    unlisten();
  }
  await relaunch();
}

import { useCallback, useEffect, useRef, useState } from "react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { api, errString } from "../../api/api.ts";
import type { ConnectionId } from "../../api/api.ts";
import { useResizable, TREE_MIN, TREE_MAX } from "../../useResizable";
import { ConnScope, useConnectionState } from "../../connectionState";
import { useLoader } from "../Loader";
import { FileEntry, TransferStats } from "../../api/sftp.ts";

interface Props {
  connectionId: ConnectionId;
  savedId: string;
  treeWidth: number;
  onTreeWidthChange: (width: number) => void;
}

type ViewMode = "grid" | "list";

interface ContextMenuState {
  x: number;
  y: number;
  entry: FileEntry | null; // null = background (empty-space) menu
}

/** A node in the Places directory tree. `children === undefined` means "not
 *  loaded yet"; `[]` means loaded-and-empty. */
interface TreeNode {
  name: string;
  path: string;
  expanded: boolean;
  loading: boolean;
  children?: TreeNode[];
}

/** Live state for an in-progress folder download. */
/** Live state for an in-progress file transfer (download or upload). */
interface TransferState {
  title: string; // modal heading, e.g. 'Downloading "docs"' or "Uploading 3 files"
  current: string; // file currently being transferred
  done: number; // files completed so far
  total: number; // total files (0 while still scanning)
  cancelable: boolean; // whether the Cancel button is shown
}

function parentPath(p: string): string {
  if (p === "/") return "/";
  const i = p.lastIndexOf("/");
  if (i <= 0) return "/";
  return p.slice(0, i);
}

function joinPath(dir: string, name: string): string {
  if (dir === "/") return "/" + name;
  return dir + "/" + name;
}

function formatSize(bytes: number): string {
  if (bytes < 1024) return bytes + " B";
  if (bytes < 1024 * 1024) return (bytes / 1024).toFixed(1) + " KB";
  if (bytes < 1024 * 1024 * 1024) return (bytes / (1024 * 1024)).toFixed(1) + " MB";
  return (bytes / (1024 * 1024 * 1024)).toFixed(1) + " GB";
}

function formatDate(ts: number): string {
  if (!ts) return "—";
  return new Date(ts * 1000).toLocaleDateString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
  });
}

function formatPerms(mode: number): string {
  const bits = mode & 0o777;
  let s = "";
  for (let i = 2; i >= 0; i--) {
    const shift = i * 3;
    s += (bits >> (shift + 2)) & 1 ? "r" : "-";
    s += (bits >> (shift + 1)) & 1 ? "w" : "-";
    s += (bits >> shift) & 1 ? "x" : "-";
  }
  return s;
}

// ── Icons (Nautilus-ish) ──────────────────────────────────────────────────

function FolderIcon({ size = 48 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" fill="none" aria-hidden="true">
      <path
        d="M3 6.5A1.5 1.5 0 0 1 4.5 5h4.2c.4 0 .78.16 1.06.44L11.5 7h8A1.5 1.5 0 0 1 21 8.5v9A1.5 1.5 0 0 1 19.5 19h-15A1.5 1.5 0 0 1 3 17.5v-11Z"
        className="fm-icon-folder"
      />
    </svg>
  );
}

function FileIcon({ size = 48 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" fill="none" aria-hidden="true">
      <path
        d="M6 3.5A1.5 1.5 0 0 1 7.5 2h6.1L19 7.4v13.1A1.5 1.5 0 0 1 17.5 22h-10A1.5 1.5 0 0 1 6 20.5v-17Z"
        className="fm-icon-file"
      />
      <path d="M13.5 2v5.5H19" className="fm-icon-file-fold" />
    </svg>
  );
}

export function FileManagerWorkspace({
  connectionId,
  savedId,
  treeWidth,
  onTreeWidthChange,
}: Props) {
  const [width, setWidth] = useState(treeWidth);
  const treeResize = useResizable({
    width,
    min: TREE_MIN,
    max: TREE_MAX,
    onChange: setWidth,
    onCommit: onTreeWidthChange,
  });

  // Session-preserved navigation/view state (see connectionState.ts): survives
  // the unmount a connection switch causes, keyed by the stable saved-profile id.
  const scope = ConnScope(savedId, "filemanager");
  const [currentPath, setCurrentPath] = useConnectionState(scope, "currentPath", "/");
  const [entries, setEntries] = useConnectionState<FileEntry[]>(scope, "entries", []);
  // True when the current directory held more entries than the backend cap
  // (10,000) and the listing was truncated; drives a status-bar warning.
  const [truncated, setTruncated] = useConnectionState<boolean>(scope, "truncated", false);
  // Directory listing uses the app's global workspace-scoped loader (covers the
  // workspace area, leaves the sidebar interactive) instead of a local spinner.
  const loader = useLoader();
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [view, setView] = useConnectionState<ViewMode>(scope, "view", "grid");

  // Navigation history (back/forward), like a browser.
  const [history, setHistory] = useConnectionState<string[]>(scope, "history", ["/"]);
  const [histIndex, setHistIndex] = useConnectionState(scope, "histIndex", 0);

  // Places directory tree (lazy-loaded, rooted at "/").
  const [tree, setTree] = useConnectionState<TreeNode>(scope, "tree", {
    name: "/",
    path: "/",
    expanded: true,
    loading: false,
  });

  const [renameTarget, setRenameTarget] = useState<FileEntry | null>(null);
  const [renameValue, setRenameValue] = useState("");
  const [newFolderMode, setNewFolderMode] = useState(false);
  const [newFolderName, setNewFolderName] = useState("");
  const [confirmDelete, setConfirmDelete] = useState<FileEntry[] | null>(null);
  const [menu, setMenu] = useState<ContextMenuState | null>(null);

  // In-progress transfer (download or upload); null when idle. The transfer
  // itself runs *inside the plugin* (background task on the connection), so it
  // survives this component unmounting on a connection switch. We poll
  // `last_transfer_stats` for progress and reattach to a running transfer on
  // mount. `download` mirrors the latest stats into the modal shape.
  const [download, setDownload] = useState<TransferState | null>(null);
  const [downloadNote, setDownloadNote] = useState<string | null>(null);
  const [cancelling, setCancelling] = useState(false);
  // setInterval handle for the stats poll, and the modal title/destination for
  // the current transfer (kept in refs so the poll callback, registered once,
  // always sees the latest without re-subscribing).
  const pollRef = useRef<number | null>(null);
  const transferMetaRef = useRef<{ title: string; dest: string } | null>(null);
  // True while an OS file-drag is hovering the content area (drop highlight).
  const [dragOver, setDragOver] = useState(false);

  const renameRef = useRef<HTMLInputElement>(null);
  const newFolderRef = useRef<HTMLInputElement>(null);
  // Holds the latest drop handler so the OS drag-drop listener (registered once)
  // always uploads into the current directory without re-subscribing.
  const onDropRef = useRef<(paths: string[]) => void>(() => {});

  const fetchDir = useCallback(
    async (path: string) => {
      loader.show({ scope: "workspace" });
      setError(null);
      try {
        const list = await api.sftpListDir(connectionId, path);
        setEntries(list.entries);
        setTruncated(list.truncated);
        setCurrentPath(path);
        setSelected(new Set());
      } catch (e) {
        setError(errString(e));
      } finally {
        loader.hide();
      }
    },
    [connectionId, loader, setCurrentPath, setEntries, setTruncated],
  );

  // Navigate to a path and push it onto the history stack.
  const navigate = useCallback(
    (path: string) => {
      setHistory((prev) => {
        const trimmed = prev.slice(0, histIndex + 1);
        if (trimmed[trimmed.length - 1] === path) return prev;
        const next = [...trimmed, path];
        setHistIndex(next.length - 1);
        return next;
      });
      fetchDir(path);
    },
    [fetchDir, histIndex, setHistIndex, setHistory],
  );

  function goBack() {
    if (histIndex > 0) {
      const i = histIndex - 1;
      setHistIndex(i);
      fetchDir(history[i]);
    }
  }

  function goForward() {
    if (histIndex < history.length - 1) {
      const i = histIndex + 1;
      setHistIndex(i);
      fetchDir(history[i]);
    }
  }

  useEffect(() => {
    // Start in the session's home directory (writable), not "/" (often not).
    // But if navigation state was restored from the store (connection
    // switch-back), keep the user where they were instead of resetting home.
    if (entries.length > 0 || currentPath !== "/") {
      return;
    }
    let cancelled = false;
    (async () => {
      let home = "/";
      try {
        home = await api.sftpHomeDir(connectionId);
      } catch {
        // fall back to "/"
      }
      if (cancelled) return;
      const name = home === "/" ? "/" : home.split("/").pop() || home;
      setTree({ name, path: home, expanded: true, loading: false });
      setHistory([home]);
      setHistIndex(0);
      fetchDir(home);
    })();
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [connectionId]);

  // Keep the drop handler pointed at the latest state (current dir, etc.).
  onDropRef.current = (paths: string[]) => {
    if (download) return; // ignore drops while a transfer is in progress
    uploadPaths(paths);
  };

  // Register the OS file drag-drop listener once. Tauri delivers native file
  // drops (with absolute local paths) as webview drag-drop events; we highlight
  // the content area on hover and upload the dropped paths on drop.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let active = true;
    getCurrentWebview()
      .onDragDropEvent((event) => {
        const p = event.payload;
        if (p.type === "over" || p.type === "enter") setDragOver(true);
        else if (p.type === "leave") setDragOver(false);
        else if (p.type === "drop") {
          setDragOver(false);
          if (p.paths && p.paths.length) onDropRef.current(p.paths);
        }
      })
      .then((un) => {
        if (active) unlisten = un;
        else un();
      });
    return () => {
      active = false;
      unlisten?.();
    };
  }, []);

  useEffect(() => {
    if (renameTarget && renameRef.current) {
      renameRef.current.focus();
      renameRef.current.select();
    }
  }, [renameTarget]);

  useEffect(() => {
    if (newFolderMode && newFolderRef.current) {
      newFolderRef.current.focus();
    }
  }, [newFolderMode]);

  // Dismiss the context menu on any outside click / escape / scroll.
  useEffect(() => {
    if (!menu) return;
    const close = () => setMenu(null);
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setMenu(null);
    };
    window.addEventListener("click", close);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("click", close);
      window.removeEventListener("keydown", onKey);
    };
  }, [menu]);

  // Reattach to an in-progress transfer on mount (e.g. after a connection
  // switch unmounted/remounted this workspace). The transfer runs in the plugin
  // and keeps going regardless; if one is still active we repaint the modal and
  // resume polling. On unmount we only stop *polling* — the transfer is left
  // running in the plugin. Keyed on connectionId so a switch re-checks.
  useEffect(() => {
    let active = true;
    api
      .sftpLastTransferStats(connectionId)
      .then((s) => {
        if (!active || !s) return;
        if (s.phase === "scanning" || s.phase === "running") {
          // We don't know the original title/dest after a remount, so use a
          // generic heading; progress counts still come from the live stats.
          beginPolling("Transferring", "");
        }
      })
      .catch(() => {});
    return () => {
      active = false;
      stopPoll();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [connectionId]);

  // ── File operations ───────────────────────────────────────────────────────

  // Local path separator helper: the destination dir comes from the native
  // dialog, so it uses the host OS separator. We can't know it for sure in the
  // webview, but joining with "/" works on macOS/Linux and most Windows APIs
  // accept it too; the plugin's create_dir_all normalizes either way.
  function localJoin(...parts: string[]): string {
    return parts.filter(Boolean).join("/");
  }

  // ── Transfer progress (poll the plugin's background job) ────────────────────

  function stopPoll() {
    if (pollRef.current !== null) {
      clearInterval(pollRef.current);
      pollRef.current = null;
    }
  }

  /** Apply one stats snapshot to the modal, and on a terminal phase stop polling
   *  and show the result note. Returns true if the transfer is finished. */
  function applyStats(s: TransferStats | null): boolean {
    const meta = transferMetaRef.current;
    if (!s || !meta) {
      // No job at all: nothing in progress (e.g. first poll before start lands).
      return false;
    }
    if (s.phase === "scanning" || s.phase === "running") {
      setDownload({
        title: meta.title,
        current: s.phase === "scanning" ? "Scanning…" : s.current,
        done: s.done,
        total: s.total,
        cancelable: true,
      });
      return false;
    }
    // Terminal phase — tear down the modal and report the outcome.
    stopPoll();
    setDownload(null);
    setCancelling(false);
    if (s.phase === "cancelled") {
      setDownloadNote(`${meta.title} cancelled after ${s.done} file${s.done === 1 ? "" : "s"}.`);
    } else if (s.phase === "error") {
      setError(s.error || "Transfer failed.");
    } else {
      setDownloadNote(
        meta.dest
          ? `${meta.title} — ${s.done} file${s.done === 1 ? "" : "s"} to ${meta.dest}.`
          : `${meta.title} — ${s.done} file${s.done === 1 ? "" : "s"}.`,
      );
    }
    // Uploads change the current directory's contents; refresh it.
    fetchDir(currentPath);
    return true;
  }

  /** Begin polling the plugin's transfer stats (~400ms) until terminal. Shows
   *  the modal immediately with a scanning placeholder. */
  function beginPolling(title: string, dest: string) {
    transferMetaRef.current = { title, dest };
    setCancelling(false);
    setDownloadNote(null);
    setDownload({
      title,
      current: "Scanning…",
      done: 0,
      total: 0,
      cancelable: true,
    });
    stopPoll();
    pollRef.current = window.setInterval(() => {
      api
        .sftpLastTransferStats(connectionId)
        .then(applyStats)
        .catch(() => {
          // A transient poll error shouldn't kill the modal; keep trying.
        });
    }, 400);
  }

  async function downloadFile(entry: FileEntry) {
    // Single-file download: pick a destination folder, then start a one-item
    // download transfer in the plugin and poll it.
    if (entry.is_dir) return;
    const dest = await openDialog({ directory: true, multiple: false });
    if (!dest || Array.isArray(dest)) return;
    const local = localJoin(dest, entry.name);
    try {
      await api.sftpStartTransfer(connectionId, "download", [
        { remote_path: entry.path, local_path: local },
      ]);
      beginPolling(`Downloading “${entry.name}”`, dest);
    } catch (e) {
      setError(errString(e));
    }
  }

  async function downloadFolder(entry: FileEntry) {
    // Start a recursive folder download in the plugin (it walks the tree), then
    // poll. The whole transfer lives in the plugin, so it survives a connection
    // switch — returning to the workspace reattaches via `reattach` below.
    const dest = await openDialog({ directory: true, multiple: false });
    if (!dest || Array.isArray(dest)) return;
    const destRoot = localJoin(dest, entry.name);
    try {
      await api.sftpStartTransfer(connectionId, "download", [
        { remote_path: entry.path, local_path: destRoot },
      ]);
      beginPolling(`Downloading “${entry.name}”`, destRoot);
    } catch (e) {
      setError(errString(e));
    }
  }

  /** Download one or more selected entries (files and/or folders) into a single
   *  chosen destination folder. A single item delegates to the existing
   *  single-item flows (their titles read better); multiple items start one
   *  transfer carrying every entry. */
  async function downloadEntries(targets: FileEntry[]) {
    if (targets.length === 0) return;
    if (targets.length === 1) {
      const only = targets[0];
      return only.is_dir ? downloadFolder(only) : downloadFile(only);
    }
    const dest = await openDialog({ directory: true, multiple: false });
    if (!dest || Array.isArray(dest)) return;
    const items = targets.map((entry) => ({
      remote_path: entry.path,
      local_path: localJoin(dest, entry.name),
    }));
    try {
      await api.sftpStartTransfer(connectionId, "download", items);
      beginPolling(`Downloading ${targets.length} items`, dest);
    } catch (e) {
      setError(errString(e));
    }
  }

  function cancelFolderDownload() {
    setCancelling(true);
    // Cooperative: the plugin's task observes the flag between files and
    // transitions to Cancelled; our next poll picks that up and closes the modal.
    api.sftpCancelLastTransfer(connectionId).catch(() => {});
  }

  /** Upload one or more local paths (files or directories) into the current
   *  directory. Shared by the toolbar dialog and drag-and-drop. Directories are
   *  mirrored recursively by the plugin. Starts a background transfer and polls
   *  it; supports cancel. */
  async function uploadPaths(localPaths: string[]) {
    if (localPaths.length === 0) return;
    const items = localPaths.map((localPath) => {
      const name = localPath.split(/[/\\]/).pop() || "upload";
      return { local_path: localPath, remote_path: joinPath(currentPath, name) };
    });
    const title = localPaths.length === 1 ? "Uploading" : `Uploading ${localPaths.length} items`;
    try {
      await api.sftpStartTransfer(connectionId, "upload", items);
      beginPolling(title, currentPath);
    } catch (e) {
      setError(errString(e));
    }
  }

  async function upload() {
    // Pick local file(s) via the native dialog; the plugin reads them from disk
    // and writes them to the remote. (Directory upload is supported via
    // drag-and-drop, since a single native dialog can't pick both files and
    // folders.)
    const picked = await openDialog({ multiple: true, directory: false });
    if (!picked) return;
    await uploadPaths(Array.isArray(picked) ? picked : [picked]);
  }

  async function doDelete(targets: FileEntry[]) {
    setConfirmDelete(null);
    if (targets.length === 0) return;
    try {
      // One call deletes all targets in the plugin (recursively, in order).
      await api.sftpDeletePaths(
        connectionId,
        targets.map((t) => t.path),
      );
      fetchDir(currentPath);
      reloadTreePath(currentPath);
    } catch (e) {
      setError(errString(e));
    }
  }

  async function doRename() {
    if (!renameTarget || !renameValue.trim()) return;
    const to = joinPath(parentPath(renameTarget.path), renameValue.trim());
    try {
      await api.sftpRename(connectionId, renameTarget.path, to);
      setRenameTarget(null);
      setRenameValue("");
      fetchDir(currentPath);
      reloadTreePath(currentPath);
    } catch (e) {
      setError(errString(e));
    }
  }

  async function doMkdir() {
    if (!newFolderName.trim()) return;
    const path = joinPath(currentPath, newFolderName.trim());
    try {
      await api.sftpMkdir(connectionId, path);
      setNewFolderMode(false);
      setNewFolderName("");
      fetchDir(currentPath);
      reloadTreePath(currentPath);
    } catch (e) {
      setError(errString(e));
    }
  }

  function open(entry: FileEntry) {
    if (entry.is_dir) navigate(entry.path);
    else downloadFile(entry);
  }

  function startRename(entry: FileEntry) {
    setRenameTarget(entry);
    setRenameValue(entry.name);
  }

  // ── Selection ───────────────────────────────────────────────────────────

  function selectEntry(entry: FileEntry, e: React.MouseEvent) {
    if (e.ctrlKey || e.metaKey) {
      setSelected((prev) => {
        const next = new Set(prev);
        if (next.has(entry.path)) {
          next.delete(entry.path);
        } else {
          next.add(entry.path);
        }
        return next;
      });
    } else {
      setSelected(new Set([entry.path]));
    }
  }

  function openContextMenu(e: React.MouseEvent, entry: FileEntry | null) {
    e.preventDefault();
    e.stopPropagation();
    if (entry && !selected.has(entry.path)) {
      setSelected(new Set([entry.path]));
    }
    setMenu({ x: e.clientX, y: e.clientY, entry });
  }

  const selectedEntries = entries.filter((e) => selected.has(e.path));

  // ── Breadcrumb (pill pathbar) ─────────────────────────────────────────────

  function renderPathbar() {
    const parts = currentPath === "/" ? [] : currentPath.split("/").slice(1);
    const crumbs: { label: string; path: string }[] = [{ label: "/", path: "/" }];
    let acc = "";
    for (const part of parts) {
      acc += "/" + part;
      crumbs.push({ label: part, path: acc });
    }
    return (
      <div className="fm-pathbar">
        {crumbs.map((c, i) => (
          <button
            key={c.path}
            className={"fm-crumb" + (i === crumbs.length - 1 ? " current" : "")}
            onClick={() => navigate(c.path)}
          >
            {c.label}
          </button>
        ))}
      </div>
    );
  }

  // ── Places sidebar (directory tree) ────────────────────────────────────────

  /** Apply `fn` to the node at `path` anywhere in the tree, returning a new tree. */
  function updateNode(node: TreeNode, path: string, fn: (n: TreeNode) => TreeNode): TreeNode {
    if (node.path === path) return fn(node);
    if (!node.children) return node;
    return {
      ...node,
      children: node.children.map((c) => updateNode(c, path, fn)),
    };
  }

  async function loadChildren(path: string) {
    try {
      const list = await api.sftpListDir(connectionId, path);
      const dirs = list.entries.filter((e) => e.is_dir);
      setTree((t) =>
        updateNode(t, path, (n) => {
          // Preserve expansion/loaded state of children that still exist, so a
          // refresh after a mutation doesn't collapse the tree.
          const prev = new Map((n.children ?? []).map((c) => [c.path, c]));
          const children: TreeNode[] = dirs.map(
            (e) =>
              prev.get(e.path) ?? {
                name: e.name,
                path: e.path,
                expanded: false,
                loading: false,
              },
          );
          return { ...n, children, loading: false };
        }),
      );
    } catch {
      setTree((t) => updateNode(t, path, (n) => ({ ...n, children: [], loading: false })));
    }
  }

  /** Re-fetch a path's children in the tree if that node is currently expanded
   *  (keeps the Places tree in sync after create/delete/rename). */
  function reloadTreePath(path: string) {
    setTree((t) => {
      let isExpanded = false;
      const check = (n: TreeNode): void => {
        if (n.path === path) isExpanded = n.expanded;
        else n.children?.forEach(check);
      };
      check(t);
      if (isExpanded) loadChildren(path);
      return t;
    });
  }

  /** Expand/collapse a tree node's twisty (without navigating). */
  function toggleTreeNode(node: TreeNode, e: React.MouseEvent) {
    e.stopPropagation();
    if (node.expanded) {
      setTree((t) => updateNode(t, node.path, (n) => ({ ...n, expanded: false })));
      return;
    }
    setTree((t) =>
      updateNode(t, node.path, (n) => ({
        ...n,
        expanded: true,
        loading: n.children === undefined,
      })),
    );
    if (node.children === undefined) loadChildren(node.path);
  }

  /** Click a tree node: navigate into it and ensure it's expanded/loaded. */
  function clickTreeNode(node: TreeNode) {
    navigate(node.path);
    if (!node.expanded) {
      setTree((t) =>
        updateNode(t, node.path, (n) => ({
          ...n,
          expanded: true,
          loading: n.children === undefined,
        })),
      );
      if (node.children === undefined) loadChildren(node.path);
    }
  }

  function renderTreeNode(node: TreeNode, depth: number): React.ReactNode {
    const hasTwisty = node.children === undefined || node.children.length > 0;
    return (
      <div key={node.path}>
        <div
          className={"fm-tree-row" + (currentPath === node.path ? " active" : "")}
          style={{ paddingLeft: 6 + depth * 14 }}
          onClick={() => clickTreeNode(node)}
        >
          <span
            className={"fm-tree-twisty" + (hasTwisty ? "" : " empty")}
            onClick={(e) => hasTwisty && toggleTreeNode(node, e)}
          >
            {node.loading ? "·" : hasTwisty ? (node.expanded ? "▾" : "▸") : ""}
          </span>
          <svg width="15" height="15" viewBox="0 0 24 24" aria-hidden="true">
            <path
              d="M3 6.5A1.5 1.5 0 0 1 4.5 5h4.2c.4 0 .78.16 1.06.44L11.5 7h8A1.5 1.5 0 0 1 21 8.5v9A1.5 1.5 0 0 1 19.5 19h-15A1.5 1.5 0 0 1 3 17.5v-11Z"
              className="fm-icon-folder"
            />
          </svg>
          <span className="fm-tree-label">{node.path === "/" ? "/" : node.name}</span>
        </div>
        {node.expanded && node.children && node.children.map((c) => renderTreeNode(c, depth + 1))}
      </div>
    );
  }

  function renderSidebar() {
    return (
      <div className="fm-sidebar" style={{ width }}>
        <div className="fm-sidebar-section">Places</div>
        <div className="fm-tree">{renderTreeNode(tree, 0)}</div>
      </div>
    );
  }

  // ── File item (shared label / rename input) ────────────────────────────────

  function entryLabel(entry: FileEntry) {
    if (renameTarget?.path === entry.path) {
      return (
        <input
          ref={renameRef}
          className="fm-rename-input"
          value={renameValue}
          onChange={(e) => setRenameValue(e.target.value)}
          onClick={(e) => e.stopPropagation()}
          onDoubleClick={(e) => e.stopPropagation()}
          onKeyDown={(e) => {
            if (e.key === "Enter") doRename();
            if (e.key === "Escape") setRenameTarget(null);
          }}
          onBlur={doRename}
        />
      );
    }
    return <span className="fm-label-text">{entry.name}</span>;
  }

  function renderGrid() {
    return (
      <div className="fm-grid">
        {entries.map((entry) => (
          <div
            key={entry.path}
            className={"fm-tile" + (selected.has(entry.path) ? " selected" : "")}
            onClick={(e) => selectEntry(entry, e)}
            onDoubleClick={() => open(entry)}
            onContextMenu={(e) => openContextMenu(e, entry)}
            title={entry.name}
          >
            <div className="fm-tile-icon">{entry.is_dir ? <FolderIcon /> : <FileIcon />}</div>
            <div className="fm-tile-label">{entryLabel(entry)}</div>
          </div>
        ))}
      </div>
    );
  }

  function renderList() {
    return (
      <table className="fm-list">
        <thead>
          <tr>
            <th>Name</th>
            <th className="fm-col-size">Size</th>
            <th className="fm-col-date">Modified</th>
            <th className="fm-col-perms">Permissions</th>
          </tr>
        </thead>
        <tbody>
          {entries.map((entry) => (
            <tr
              key={entry.path}
              className={"fm-list-row" + (selected.has(entry.path) ? " selected" : "")}
              onClick={(e) => selectEntry(entry, e)}
              onDoubleClick={() => open(entry)}
              onContextMenu={(e) => openContextMenu(e, entry)}
            >
              <td className="fm-list-name">
                <span className="fm-list-icon">
                  {entry.is_dir ? <FolderIcon size={20} /> : <FileIcon size={20} />}
                </span>
                {entryLabel(entry)}
              </td>
              <td className="fm-col-size muted">{entry.is_dir ? "—" : formatSize(entry.size)}</td>
              <td className="fm-col-date muted">{formatDate(entry.modified)}</td>
              <td className="fm-col-perms muted">{formatPerms(entry.permissions)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    );
  }

  // ── Render ──────────────────────────────────────────────────────────────

  return (
    <div className="workspace fm-workspace">
      {renderSidebar()}
      <div className="tree-resizer" onMouseDown={treeResize.onMouseDown} title="Drag to resize" />

      <div className="fm-main">
        {/* Header bar */}
        <div className="fm-headerbar">
          <div className="fm-nav">
            <button className="fm-iconbtn" title="Back" disabled={histIndex <= 0} onClick={goBack}>
              ←
            </button>
            <button
              className="fm-iconbtn"
              title="Forward"
              disabled={histIndex >= history.length - 1}
              onClick={goForward}
            >
              →
            </button>
            <button
              className="fm-iconbtn"
              title="Up"
              disabled={currentPath === "/"}
              onClick={() => navigate(parentPath(currentPath))}
            >
              ↑
            </button>
          </div>

          {renderPathbar()}

          <div className="fm-actions">
            <div className="fm-viewtoggle">
              <button
                className={"fm-iconbtn" + (view === "grid" ? " active" : "")}
                title="Grid view"
                onClick={() => setView("grid")}
              >
                ▦
              </button>
              <button
                className={"fm-iconbtn" + (view === "list" ? " active" : "")}
                title="List view"
                onClick={() => setView("list")}
              >
                ☰
              </button>
            </div>
            <button className="fm-iconbtn" title="Upload file" onClick={upload}>
              ⬆
            </button>
            <button
              className="fm-iconbtn"
              title="New folder"
              onClick={() => {
                setNewFolderMode(true);
                setNewFolderName("");
              }}
            >
              ＋
            </button>
            <button className="fm-iconbtn" title="Refresh" onClick={() => fetchDir(currentPath)}>
              ↻
            </button>
          </div>
        </div>

        {error && <div className="fm-error">{error}</div>}

        {newFolderMode && (
          <div className="fm-newfolder">
            <FolderIcon size={20} />
            <input
              ref={newFolderRef}
              value={newFolderName}
              placeholder="Folder name"
              onChange={(e) => setNewFolderName(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") doMkdir();
                if (e.key === "Escape") setNewFolderMode(false);
              }}
            />
            <button className="primary" onClick={doMkdir}>
              Create
            </button>
            <button onClick={() => setNewFolderMode(false)}>Cancel</button>
          </div>
        )}

        {/* Content area */}
        <div
          className={"fm-content" + (dragOver ? " drag-over" : "")}
          onContextMenu={(e) => openContextMenu(e, null)}
          onClick={() => setSelected(new Set())}
        >
          <div onClick={(e) => e.stopPropagation()}>
            {view === "grid" ? renderGrid() : renderList()}
          </div>
          {entries.length === 0 &&
            !(loader.state.visible && loader.state.scope === "workspace") && (
              <div className="fm-empty">This folder is empty</div>
            )}
          {dragOver && <div className="fm-drop-overlay">Drop files to upload here</div>}
        </div>

        {/* Status bar */}
        <div className="fm-statusbar">
          {downloadNote ? (
            <span className="fm-statusbar-note">
              {downloadNote}
              <button
                className="fm-statusbar-dismiss"
                title="Dismiss"
                onClick={() => setDownloadNote(null)}
              >
                ✕
              </button>
            </span>
          ) : selectedEntries.length > 0 ? (
            `${selectedEntries.length} of ${entries.length} selected`
          ) : (
            `${entries.length} item${entries.length === 1 ? "" : "s"}`
          )}
          {truncated && !downloadNote && (
            <span
              className="fm-statusbar-warning"
              title="This directory has too many files to list in full"
            >
              ⚠ Showing first {entries.length.toLocaleString()} entries — directory truncated
            </span>
          )}
        </div>
      </div>

      {/* Transfer progress overlay — scoped to this workspace, not the whole
          window. The backdrop covers only the file-manager area (it's absolutely
          positioned within `.fm-workspace`), so it blocks further actions on
          *this* connection while a transfer runs but leaves the sidebar —
          including the connection switcher — fully interactive. The transfer
          itself runs in the plugin and survives switching away and back. */}
      {download && (
        <div className="fm-progress-backdrop">
          <div className="modal fm-progress">
            <h3>{download.title}</h3>
            <div className="fm-progress-bar">
              <div className="fm-progress-fill" />
            </div>
            <p className="muted fm-progress-status">
              {download.total > 0
                ? `${download.done} of ${download.total} files`
                : download.current}
              {download.current && download.total > 0 && (
                <>
                  {" · "}
                  <span className="fm-progress-file">{download.current}</span>
                </>
              )}
            </p>
            {download.cancelable && (
              <div className="modal-actions">
                <button onClick={cancelFolderDownload} disabled={cancelling}>
                  {cancelling ? "Cancelling…" : "Cancel"}
                </button>
              </div>
            )}
          </div>
        </div>
      )}

      {/* Context menu */}
      {menu && (
        <div
          className="fm-context"
          style={{ left: menu.x, top: menu.y }}
          onClick={(e) => e.stopPropagation()}
        >
          {menu.entry ? (
            (() => {
              // When several items are selected (and the right-clicked entry is
              // one of them), the menu acts on the whole selection: only the
              // operations that make sense in bulk — Download and Delete — are
              // shown. Open and Rename are single-item only.
              const multi = selectedEntries.length > 1 && selected.has(menu.entry.path);
              const targets = multi ? selectedEntries : [menu.entry];
              return (
                <>
                  {!multi && (
                    <button
                      onClick={() => {
                        open(menu.entry!);
                        setMenu(null);
                      }}
                    >
                      Open
                    </button>
                  )}
                  <button
                    onClick={() => {
                      downloadEntries(targets);
                      setMenu(null);
                    }}
                  >
                    {multi
                      ? `Download ${targets.length} items…`
                      : menu.entry!.is_dir
                        ? "Download folder…"
                        : "Download…"}
                  </button>
                  {!multi && (
                    <button
                      onClick={() => {
                        startRename(menu.entry!);
                        setMenu(null);
                      }}
                    >
                      Rename…
                    </button>
                  )}
                  <div className="fm-context-sep" />
                  <button
                    className="fm-context-danger"
                    onClick={() => {
                      setConfirmDelete(targets);
                      setMenu(null);
                    }}
                  >
                    {multi ? `Delete ${targets.length} items` : "Delete"}
                  </button>
                </>
              );
            })()
          ) : (
            <>
              <button
                onClick={() => {
                  setNewFolderMode(true);
                  setNewFolderName("");
                  setMenu(null);
                }}
              >
                New Folder
              </button>
              <button
                onClick={() => {
                  upload();
                  setMenu(null);
                }}
              >
                Upload File…
              </button>
              <button
                onClick={() => {
                  fetchDir(currentPath);
                  setMenu(null);
                }}
              >
                Refresh
              </button>
            </>
          )}
        </div>
      )}

      {confirmDelete && (
        <div className="modal-backdrop">
          <div className="modal">
            <h3>
              Delete{" "}
              {confirmDelete.length === 1 ? confirmDelete[0].name : `${confirmDelete.length} items`}
              ?
            </h3>
            <p className="muted">This action cannot be undone.</p>
            <div className="modal-actions">
              <button className="danger" onClick={() => doDelete(confirmDelete)}>
                Delete
              </button>
              <button onClick={() => setConfirmDelete(null)}>Cancel</button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

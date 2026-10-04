import { useEffect, useRef, useState, type CSSProperties } from "react";
import type { OpenConnection } from "../App";
import type { PluginInfo, ConnectionId } from "../api/api.ts";
import type { SavedConnection } from "../api/store.ts";
import { pluginLogo } from "../pluginLogos";
import { THEMES } from "../theme";
import { Modal } from "./Modal";
import { open as openExternal } from "@tauri-apps/plugin-shell";
import { getVersion } from "@tauri-apps/api/app"; // v2 Import
import pkg from "../../package.json";

/** Project home (from package.json) — opened by Help and shown in About. */
const REPO_URL = pkg.homepage;

interface SidebarProps {
  saved: SavedConnection[];
  plugins: PluginInfo[];
  /** The app's release channel ("nightly" or "stable"); shows a nightly badge. */
  channel: string;
  /** Sidebar width in CSS pixels (driven by the resize handle). */
  width: number;
  /** When true, render as a narrow rail that expands on hover. */
  collapsible: boolean;
  openConnections: OpenConnection[];
  activeId: ConnectionId | null;
  creating: boolean;
  /** Active UI theme id (one of THEMES). */
  theme: string;
  onThemeChange: (theme: string) => void;
  onToggleCollapsible: () => void;
  onSelect: (profile: SavedConnection) => void;
  /** Reorder the saved list: receives profile ids in the new display order. */
  onReorder: (orderedIds: string[]) => void;
  onNew: () => void;
  onEdit: (id: string) => void;
  onClone: (id: string) => void;
  onDelete: (id: string) => void;
  onDisconnect: (savedId: string) => void;
  onInstallPlugin: () => void;
  onCheckUpdates: () => void;
}

export function Sidebar({
  saved,
  plugins,
  channel,
  width,
  collapsible,
  openConnections,
  activeId,
  creating,
  theme,
  onThemeChange,
  onToggleCollapsible,
  onSelect,
  onReorder,
  onNew,
  onEdit,
  onClone,
  onDelete,
  onDisconnect,
  onInstallPlugin,
  onCheckUpdates,
}: SidebarProps) {
  function kindOf(pluginId: string): string {
    return plugins.find((p) => p.id === pluginId)?.kind ?? "other";
  }

  const [menuOpen, setMenuOpen] = useState(false);
  const menuRef = useRef<HTMLDivElement>(null);

  // Saved-profile id whose per-connection actions popup (⋮) is open, or null.
  const [openMenuId, setOpenMenuId] = useState<string | null>(null);
  // Viewport coords (right/top, px) to anchor the open popup. The popup is
  // position:fixed so it escapes the scrollable .conn-list (which would
  // otherwise clip it and add a scrollbar).
  const [menuPos, setMenuPos] = useState<{ right: number; top: number } | null>(null);
  const connMenuRef = useRef<HTMLDivElement>(null);

  const [version, setVersion] = useState("");
  // When true, the About dialog is shown.
  const [aboutOpen, setAboutOpen] = useState(false);

  // Pointer-driven reordering of the connection list: id of the row being
  // dragged and of the row it's currently hovering over (for visual feedback).
  // We use pointer events rather than the HTML5 drag-and-drop API because the
  // latter is broken on WebKitGTK (the Linux webview), where native drag
  // events never fire reliably.
  const [dragId, setDragId] = useState<string | null>(null);
  const [overId, setOverId] = useState<string | null>(null);
  // Live drag bookkeeping kept in a ref so pointermove doesn't thrash renders.
  // `dragging` flips true only once the pointer moves past a small threshold,
  // so a plain click still selects the row. `suppressClickRef` swallows the
  // click event that fires after a drag completes.
  const dragRef = useRef<{
    id: string;
    startX: number;
    startY: number;
    dragging: boolean;
  } | null>(null);
  const suppressClickRef = useRef(false);

  /** Saved-profile id of the `.conn-item` under the given viewport point. */
  function connIdAt(x: number, y: number): string | null {
    const el = document.elementFromPoint(x, y) as HTMLElement | null;
    return el?.closest<HTMLElement>(".conn-item")?.dataset.connId ?? null;
  }

  /** Move `fromId` to `toId`'s slot (after when dragging down, before when up)
   * and report the new id order. */
  function reorder(fromId: string, toId: string) {
    const ids = saved.map((s) => s.id);
    const from = ids.indexOf(fromId);
    const to = ids.indexOf(toId);
    if (from < 0 || to < 0 || from === to) return;
    const next = ids.filter((id) => id !== fromId);
    next.splice(next.indexOf(toId) + (from < to ? 1 : 0), 0, fromId);
    onReorder(next);
  }

  function onItemPointerDown(e: React.PointerEvent, id: string) {
    // Left button only; ignore presses that land on the action buttons/menu.
    if (e.button !== 0) return;
    if ((e.target as HTMLElement).closest("button, .conn-menu")) return;
    dragRef.current = { id, startX: e.clientX, startY: e.clientY, dragging: false };
  }

  function onItemPointerMove(e: React.PointerEvent) {
    const st = dragRef.current;
    if (!st) return;
    if (!st.dragging) {
      if (Math.abs(e.clientX - st.startX) < 5 && Math.abs(e.clientY - st.startY) < 5) return;
      st.dragging = true;
      setDragId(st.id);
      // Route the rest of the gesture to this row even if the pointer strays.
      e.currentTarget.setPointerCapture(e.pointerId);
    }
    const over = connIdAt(e.clientX, e.clientY);
    setOverId(over && over !== st.id ? over : null);
  }

  function onItemPointerUp(e: React.PointerEvent) {
    const st = dragRef.current;
    dragRef.current = null;
    if (st?.dragging) {
      const target = connIdAt(e.clientX, e.clientY);
      if (target && target !== st.id) reorder(st.id, target);
      suppressClickRef.current = true; // don't let this gesture select a row
    }
    setDragId(null);
    setOverId(null);
  }

  useEffect(() => {
    // getVersion returns a Promise <string>
    getVersion()
      .then((appVersion) => setVersion(appVersion))
      .catch((err) => console.error("Failed to get version:", err));
  }, []);

  useEffect(() => {
    if (!menuOpen) return;
    function onClick(e: MouseEvent) {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) {
        setMenuOpen(false);
      }
    }
    document.addEventListener("mousedown", onClick);
    return () => document.removeEventListener("mousedown", onClick);
  }, [menuOpen]);

  // Dismiss an open per-connection actions popup on any outside click. The
  // popup is position:fixed, so also close it on scroll (it wouldn't follow the
  // row) — captured so a scroll anywhere, including inside .conn-list, fires.
  useEffect(() => {
    if (openMenuId === null) return;
    function onClick(e: MouseEvent) {
      if (connMenuRef.current && !connMenuRef.current.contains(e.target as Node)) {
        setOpenMenuId(null);
      }
    }
    function onScroll() {
      setOpenMenuId(null);
    }
    document.addEventListener("mousedown", onClick);
    window.addEventListener("scroll", onScroll, true);
    return () => {
      document.removeEventListener("mousedown", onClick);
      window.removeEventListener("scroll", onScroll, true);
    };
  }, [openMenuId]);

  return (
    <aside
      className={"sidebar" + (collapsible ? " collapsible" : "")}
      style={collapsible ? ({ "--sidebar-expanded": `${width}px` } as CSSProperties) : { width }}
    >
      <div className="sidebar-header">
        <span className="logo">polyclient</span>
        <span className="logo-sub">client</span>
        <span className="logo-badge" title="App Version">
          {channel === "nightly" ? "nightly" : `v${version}`}
        </span>
      </div>
      <button className={"new-btn" + (creating ? " active" : "")} onClick={onNew}>
        + New connection
      </button>
      <nav className="conn-list">
        {saved.length === 0 && <p className="muted">No saved connections.</p>}
        {saved.map((s) => {
          const live = openConnections.find((o) => o.savedId === s.id);
          const isActive = !creating && live != null && live.id === activeId;
          const kind = kindOf(s.pluginId);
          const logo = pluginLogo(s.pluginId, kind);
          return (
            <div
              key={s.id}
              data-conn-id={s.id}
              className={
                "conn-item" +
                (isActive ? " active" : "") +
                (live ? " connected" : "") +
                (dragId === s.id ? " dragging" : "") +
                (overId === s.id && dragId !== s.id ? " drag-over" : "")
              }
              onClick={() => {
                if (suppressClickRef.current) {
                  suppressClickRef.current = false;
                  return;
                }
                onSelect(s);
              }}
              onPointerDown={(e) => onItemPointerDown(e, s.id)}
              onPointerMove={onItemPointerMove}
              onPointerUp={onItemPointerUp}
            >
              {logo ? (
                <img
                  className={"plugin-logo" + (live ? " connected" : "")}
                  src={logo}
                  alt=""
                  draggable={false}
                  title={live ? "Connected" : "Not connected"}
                />
              ) : (
                <span
                  className={
                    "dot dot-" + kind + " dot-id-" + s.pluginId + (live ? " connected" : "")
                  }
                  title={live ? "Connected" : "Not connected"}
                />
              )}
              <span className="conn-name" title={s.name}>
                {s.name}
              </span>
              {live && (
                <button
                  className="icon-btn"
                  title="Disconnect"
                  onClick={(e) => {
                    e.stopPropagation();
                    onDisconnect(s.id);
                  }}
                >
                  ⏏
                </button>
              )}
              <div className="conn-menu" ref={openMenuId === s.id ? connMenuRef : undefined}>
                <button
                  className="icon-btn"
                  title="More"
                  onClick={(e) => {
                    e.stopPropagation();
                    if (openMenuId === s.id) {
                      setOpenMenuId(null);
                      return;
                    }
                    const r = e.currentTarget.getBoundingClientRect();
                    setMenuPos({
                      right: window.innerWidth - r.right,
                      top: r.bottom + 4,
                    });
                    setOpenMenuId(s.id);
                  }}
                >
                  ⋮
                </button>
                {openMenuId === s.id && menuPos && (
                  <div
                    className="conn-menu-popup"
                    style={{ right: menuPos.right, top: menuPos.top }}
                  >
                    <button
                      className="footer-menu-item"
                      onClick={(e) => {
                        e.stopPropagation();
                        setOpenMenuId(null);
                        onEdit(s.id);
                      }}
                    >
                      ✎ Edit
                    </button>
                    <button
                      className="footer-menu-item"
                      onClick={(e) => {
                        e.stopPropagation();
                        setOpenMenuId(null);
                        onClone(s.id);
                      }}
                    >
                      ⧉ Clone
                    </button>
                    <button
                      className="footer-menu-item danger"
                      onClick={(e) => {
                        e.stopPropagation();
                        setOpenMenuId(null);
                        onDelete(s.id);
                      }}
                    >
                      × Delete
                    </button>
                  </div>
                )}
              </div>
            </div>
          );
        })}
      </nav>
      <div className="sidebar-footer">
        <button className="install-btn" onClick={onInstallPlugin}>
          ⤓ Install plugin
        </button>
        <div className="footer-menu" ref={menuRef}>
          <button className="icon-btn" title="More" onClick={() => setMenuOpen((o) => !o)}>
            ⋮
          </button>
          {menuOpen && (
            <div className="footer-menu-popup">
              <label className="footer-menu-theme">
                <span className="muted">Theme</span>
                <select value={theme} onChange={(e) => onThemeChange(e.target.value)}>
                  {THEMES.map((t) => (
                    <option key={t.id} value={t.id}>
                      {t.label}
                    </option>
                  ))}
                </select>
              </label>
              <label className="footer-menu-toggle">
                <input
                  type="checkbox"
                  checked={collapsible}
                  onChange={() => onToggleCollapsible()}
                />
                <span>Collapse sidebar</span>
              </label>
              <button
                className="footer-menu-item"
                onClick={() => {
                  setMenuOpen(false);
                  onCheckUpdates();
                }}
              >
                Check for updates
              </button>
              <button
                className="footer-menu-item support-btn"
                title="Support / Donate"
                onClick={() =>
                  openExternal("https://github.com/sponsors/somyatripathi7").catch(() => {})
                }
              >
                ♥ Support
              </button>
              <button
                className="footer-menu-item"
                onClick={() => {
                  setMenuOpen(false);
                  openExternal(REPO_URL + "/discussions").catch(() => {});
                }}
              >
                ? Help
              </button>
              <button
                className="footer-menu-item"
                onClick={() => {
                  setMenuOpen(false);
                  setAboutOpen(true);
                }}
              >
                ⓘ About
              </button>
            </div>
          )}
        </div>
      </div>
      {aboutOpen && (
        <Modal title="About polyclient" onClose={() => setAboutOpen(false)}>
          <div className="about-dialog">
            <p className="about-name">
              <span className="logo">polyclient</span> <span className="muted">client</span>
            </p>
            <p>
              Version{" "}
              <strong>{channel === "nightly" ? `${version} (nightly)` : `v${version}`}</strong>
            </p>
            <p className="muted">
              A cross-platform desktop client for relational databases, document stores, and message
              brokers.
            </p>
            <p>
              <a
                href={REPO_URL}
                onClick={(e) => {
                  e.preventDefault();
                  openExternal(REPO_URL).catch(() => {});
                }}
              >
                {REPO_URL}
              </a>
            </p>
            <p className="muted">Licensed under MIT.</p>
            <p className="muted">Made in India, with love ❤️</p>
          </div>
        </Modal>
      )}
    </aside>
  );
}

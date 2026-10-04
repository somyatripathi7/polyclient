import { useEffect, useMemo, useState } from "react";
import { api, errString, ptyCloseConnection } from "./api/api.ts";
import type { PluginInfo, ConnectionId, PluginKind } from "./api/api.ts";
import { Sidebar } from "./components/Sidebar";
import { ConnectionForm } from "./components/ConnectionForm";
import { InstallPluginDialog } from "./components/InstallPluginDialog";
import { UpdateBanner } from "./components/UpdateBanner";
import { PluginUpdateBanner } from "./components/PluginUpdateBanner";
import { ConfirmDialog } from "./components/Modal";
import { useLoader, WorkspaceLoaderSlot } from "./components/Loader";
import { clearConnectionState } from "./connectionState";
import { checkForUpdate, type UpdateInfo } from "./api/updater.ts";
import { RdbmsWorkspace } from "./components/workspaces/RdbmsWorkspace";
import { MongodbWorkspace } from "./components/workspaces/MongodbWorkspace";
import { RabbitMqWorkspace } from "./components/workspaces/RabbitMqWorkspace";
import { CliWorkspace } from "./components/workspaces/CliWorkspace";
import { FileManagerWorkspace } from "./components/workspaces/FileManagerWorkspace";
import { CurlUiWorkspace } from "./components/workspaces/CurlUiWorkspace";
import type { SavedConnection } from "./api/store.ts";
import { loadConnections, saveConnections, upsert, remove, genId } from "./api/store.ts";
import { loadConfig, saveConfig, deleteWorkspaceDir } from "./api/store.ts";
import type { AppConfig } from "./api/store.ts";
import { applyTheme, resolveTheme } from "./theme";

/** Min/max sidebar width (px) enforced while dragging the resize handle. */
const SIDEBAR_MIN = 180;
const SIDEBAR_MAX = 500;

/** Default tree-panel width (px) when a connection has no saved width. */
const TREE_DEFAULT = 240;
const CLI_SCRIPTS_DEFAULT = 220;

/** Default editor-pane height (px) when a connection has no saved height. */
const RDBMS_EDITOR_DEFAULT = 180;
const CLI_EDITOR_DEFAULT = 200;

/** A live, currently-open connection to a backend (one per connected profile). */
export interface OpenConnection {
  id: ConnectionId;
  savedId: string;
  pluginId: string;
  name: string;
  kind: PluginKind;
  uiModule: string;
}

export function App() {
  const loader = useLoader();
  const [plugins, setPlugins] = useState<PluginInfo[]>([]);
  // Persisted connection profiles (survive app restarts).
  const [saved, setSaved] = useState<SavedConnection[]>([]);
  // Live connections opened this session, keyed back to their saved profile.
  const [open, setOpen] = useState<OpenConnection[]>([]);
  const [activeId, setActiveId] = useState<ConnectionId | null>(null);
  const [creating, setCreating] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [connectError, setConnectError] = useState<string | null>(null);
  const [installing, setInstalling] = useState(false);
  // The release channel this app build tracks ("nightly" or "stable").
  const [channel, setChannel] = useState<string>("");
  // An available app self-update (auto-checked on launch + manual).
  const [update, setUpdate] = useState<UpdateInfo | null>(null);
  // Transient note from a manual update check ("up to date" / error).
  const [updateNote, setUpdateNote] = useState<string | null>(null);
  // Number of installed plugins with a newer release available (checked on
  // launch). >0 shows a dismissible banner; null/0 = none / not yet checked.
  const [pluginUpdates, setPluginUpdates] = useState<number>(0);
  // Persisted app config (UI prefs); kept so width saves merge other fields.
  const [config, setConfig] = useState<AppConfig | null>(null);
  // Sidebar width in px, restored from config and updated by the drag handle.
  const [sidebarWidth, setSidebarWidth] = useState(240);
  // When true, the sidebar shows as a narrow rail and expands on hover.
  const [sidebarCollapsible, setSidebarCollapsible] = useState(false);
  // Saved-profile id pending a delete confirmation (null = no prompt shown).
  const [pendingDelete, setPendingDelete] = useState<string | null>(null);
  // A clone draft seeding the new-connection form (config of a source profile
  // with a fresh id and "<name> clone" name); null when not cloning.
  const [cloneDraft, setCloneDraft] = useState<SavedConnection | null>(null);

  useEffect(() => {
    api
      .listPlugins()
      .then(setPlugins)
      .catch((e) => setLoadError(errString(e)));
    api
      .appChannel()
      .then(setChannel)
      .catch(() => {});
    loadConnections()
      .then(setSaved)
      .catch((e) => setLoadError(errString(e)));
    // First launch: show the plugin install step once, then remember it so it
    // doesn't reappear on subsequent launches.
    loadConfig()
      .then((cfg) => {
        const effective = cfg.pluginsDialogShown ? cfg : { ...cfg, pluginsDialogShown: true };
        setConfig(effective);
        applyTheme(cfg.theme);
        if (cfg.sidebarWidth) {
          setSidebarWidth(Math.min(SIDEBAR_MAX, Math.max(SIDEBAR_MIN, cfg.sidebarWidth)));
        }
        setSidebarCollapsible(cfg.sidebarCollapsible ?? false);
        if (!cfg.pluginsDialogShown) {
          setInstalling(true);
          saveConfig(effective).catch(() => {});
        }
        // Check the configured repo for newer plugin releases. Each entry
        // already carries its install/update status from the host; surface a
        // banner if any installed plugin is outdated. Non-blocking; errors
        // (offline, no repo configured, rate limit) are ignored.
        api
          .listGithubPlugins(cfg.pluginRepo)
          .then((avail) => {
            const n = avail.filter((p) => p.status === "update_available").length;
            if (n > 0) setPluginUpdates(n);
          })
          .catch(() => {});
      })
      .catch(() => {});
    // Auto-check for an app update shortly after launch (non-blocking; errors,
    // e.g. running under `tauri dev`, are ignored).
    const t = setTimeout(() => {
      checkForUpdate()
        .then((u) => u && setUpdate(u))
        .catch(() => {});
    }, 3000);
    return () => clearTimeout(t);
  }, []);

  /** Manual "Check for updates": show the banner if one exists, else a note. */
  function checkForUpdates() {
    setUpdateNote(null);
    checkForUpdate()
      .then((u) => {
        if (u) setUpdate(u);
        else {
          setUpdateNote("You're up to date.");
          setTimeout(() => setUpdateNote(null), 4000);
        }
      })
      .catch((e) => {
        setUpdateNote(errString(e));
        setTimeout(() => setUpdateNote(null), 5000);
      });
  }

  /** Re-fetch the installed plugin list (e.g. after installing a new one). */
  function refreshPlugins() {
    api
      .listPlugins()
      .then(setPlugins)
      .catch((e) => setLoadError(errString(e)));
  }

  const active = open.find((c) => c.id === activeId) ?? null;
  // Saved profiles in sidebar display order: ascending `order`, stable on ties
  // (so equal-order items keep their load order). `saved` stays the canonical
  // list everywhere else.
  const orderedSaved = useMemo(
    () =>
      saved
        .map((c, i) => [c, i] as const)
        .sort((a, b) => (a[0].order ?? 0) - (b[0].order ?? 0) || a[1] - b[1])
        .map(([c]) => c),
    [saved],
  );
  const editingProfile = useMemo(
    () => saved.find((s) => s.id === editingId) ?? null,
    [saved, editingId],
  );
  // Show the form when creating, editing, or when nothing is open/selected.
  const showForm = creating || editingId !== null || active === null;

  /** Update saved profiles in state and persist them to disk. */
  function persist(next: SavedConnection[]) {
    setSaved(next);
    saveConnections(next).catch((e) => setConnectError(errString(e)));
  }

  /** Re-stamp `order` on every profile to match the new sidebar sequence and
   * persist. The only place `order` is written — so it changes only on drag. */
  function reorderConnections(orderedIds: string[]) {
    persist(saved.map((c) => ({ ...c, order: orderedIds.indexOf(c.id) })));
  }

  /** Build a live OpenConnection record from a saved profile + backend id. */
  function toOpen(profile: SavedConnection, id: ConnectionId): OpenConnection {
    const plugin = plugins.find((p) => p.id === profile.pluginId);
    return {
      id,
      savedId: profile.id,
      pluginId: profile.pluginId,
      name: profile.name,
      kind: plugin?.kind ?? "other",
      uiModule: plugin?.ui_module ?? plugin?.kind ?? profile.pluginId,
    };
  }

  /** Persist the profile, then open a live connection. Persisting first means
   * settings are kept even if connecting fails. Throws on connect failure so
   * the form can show it. */
  async function saveAndConnect(profile: SavedConnection) {
    persist(upsert(saved, profile));
    // If this profile already had a live connection (e.g. editing it), close
    // the stale one so we don't leak a backend connection.
    const prevLive = open.find((o) => o.savedId === profile.id);
    loader.show({ message: "Connecting…", scope: "app" });
    try {
      if (prevLive) {
        try {
          await ptyCloseConnection(prevLive.id);
          await api.closeConnection(prevLive.id);
        } catch {
          // Best effort.
        }
        // Reconnecting after an edit (e.g. a different database/host): the old
        // workspace state may no longer apply, so start the new session fresh.
        clearConnectionState(profile.id);
      }
      const id = await api.openConnection(profile.pluginId, profile.config);
      const conn = toOpen(profile, id);
      setOpen((os) => [...os.filter((o) => o.savedId !== profile.id), conn]);
      setActiveId(id);
      setCreating(false);
      setEditingId(null);
      setCloneDraft(null);
      setConnectError(null);
    } finally {
      loader.hide();
    }
  }

  /** Connect a saved profile from the sidebar (or focus it if already open). */
  async function connectSaved(profile: SavedConnection) {
    const existing = open.find((o) => o.savedId === profile.id);
    if (existing) {
      setActiveId(existing.id);
      setCreating(false);
      setEditingId(null);
      return;
    }
    setConnectError(null);
    loader.show({ message: "Connecting…", scope: "app" });
    try {
      const id = await api.openConnection(profile.pluginId, profile.config);
      const conn = toOpen(profile, id);
      setOpen((os) => [...os, conn]);
      setActiveId(id);
      setCreating(false);
      setEditingId(null);
    } catch (e) {
      // Drop into the prefilled form so the user can fix the settings.
      setConnectError(errString(e));
      setEditingId(profile.id);
      setCreating(false);
    } finally {
      loader.hide();
    }
  }

  /** Close the live connection for a saved profile (leaving the profile). */
  async function disconnect(savedId: string) {
    const live = open.find((o) => o.savedId === savedId);
    if (!live) return;
    try {
      // Close every ssh PTY tab too (no-op for non-CLI connections); the host
      // keeps them alive across UI unmounts, so disconnect is the explicit
      // teardown.
      await ptyCloseConnection(live.id);
      await api.closeConnection(live.id);
    } catch {
      // Best effort — drop it locally regardless.
    }
    setOpen((os) => os.filter((o) => o.id !== live.id));
    setActiveId((cur) => (cur === live.id ? null : cur));
    // Closing the connection ends its session, so drop its preserved workspace
    // state — a later reconnect starts fresh. (Switching connections keeps it.)
    clearConnectionState(savedId);
  }

  async function deleteSaved(id: string) {
    const live = open.find((o) => o.savedId === id);
    if (live) {
      try {
        await ptyCloseConnection(live.id);
        await api.closeConnection(live.id);
      } catch {
        // Best effort.
      }
      setOpen((os) => os.filter((o) => o.savedId !== id));
      setActiveId((cur) => (cur === live.id ? null : cur));
    }
    persist(remove(saved, id));
    setEditingId((cur) => (cur === id ? null : cur));
    // The profile is gone; discard any preserved workspace state for it.
    clearConnectionState(id);
    // Remove its on-disk workspace folder (saved SQL/scripts). Best effort.
    deleteWorkspaceDir(id).catch(() => {});
  }

  function renderWorkspace(conn: OpenConnection) {
    const mod = conn.uiModule || conn.kind;
    switch (mod) {
      case "rdbms": {
        // The configured database seeds the workspace's database picker; the
        // optional configured schema is expanded by default in the tree.
        const cfg = saved.find((s) => s.id === conn.savedId)?.config;
        const db = cfg?.database;
        const sch = cfg?.schema;
        return (
          <RdbmsWorkspace
            key={conn.id}
            connectionId={conn.id}
            savedId={conn.savedId}
            database={typeof db === "string" ? db : null}
            defaultSchema={typeof sch === "string" && sch ? sch : null}
            treeWidth={treeWidthFor(conn.savedId, TREE_DEFAULT)}
            onTreeWidthChange={(w) => commitTreeWidth(conn.savedId, w)}
            editorHeight={editorHeightFor(conn.savedId, RDBMS_EDITOR_DEFAULT)}
            onEditorHeightChange={(h) => commitEditorHeight(conn.savedId, h)}
          />
        );
      }
      case "document": {
        // The configured default DB seeds the workspace's database picker.
        const cfg = saved.find((s) => s.id === conn.savedId)?.config;
        const db = cfg?.database;
        return (
          <MongodbWorkspace
            key={conn.id}
            connectionId={conn.id}
            savedId={conn.savedId}
            database={typeof db === "string" && db ? db : null}
            treeWidth={treeWidthFor(conn.savedId, TREE_DEFAULT)}
            onTreeWidthChange={(w) => commitTreeWidth(conn.savedId, w)}
          />
        );
      }
      case "rabbitmq":
        return <RabbitMqWorkspace key={conn.id} connectionId={conn.id} savedId={conn.savedId} />;
      case "cli": {
        // The ssh PTY lives in the host and survives this component
        // unmounting, so the workspace can mount/unmount like any other; it
        // reattaches to the running session (and replays scrollback) on mount.
        // The host gets the launch command from the plugin (cli.spawn_spec), so
        // no config is passed from here.
        return (
          <CliWorkspace
            key={conn.id}
            connectionId={conn.id}
            savedId={conn.savedId}
            scriptsWidth={treeWidthFor(conn.savedId, CLI_SCRIPTS_DEFAULT)}
            onScriptsWidthChange={(w) => commitTreeWidth(conn.savedId, w)}
            editorHeight={editorHeightFor(conn.savedId, CLI_EDITOR_DEFAULT)}
            onEditorHeightChange={(h) => commitEditorHeight(conn.savedId, h)}
          />
        );
      }
      case "filemanager":
        return (
          <FileManagerWorkspace
            key={conn.id}
            connectionId={conn.id}
            savedId={conn.savedId}
            treeWidth={treeWidthFor(conn.savedId, TREE_DEFAULT)}
            onTreeWidthChange={(w) => commitTreeWidth(conn.savedId, w)}
          />
        );
      case "curlui": {
        return (
          <CurlUiWorkspace
            key={conn.id}
            savedId={conn.savedId}
            config={saved.find((s) => s.id === conn.savedId)?.config ?? {}}
            treeWidth={treeWidthFor(conn.savedId, TREE_DEFAULT)}
            onTreeWidthChange={(w) => commitTreeWidth(conn.savedId, w)}
            pluginVersion={plugins.find((p) => p.id === conn.pluginId)?.version ?? ""}
          />
        );
      }
      default:
        return <div className="placeholder">No workspace available for “{mod}”.</div>;
    }
  }

  const formError = loadError ? "Failed to load plugins: " + loadError : connectError;

  // Drag the divider between sidebar and main to resize; persist on release.
  function startSidebarResize(e: React.MouseEvent) {
    e.preventDefault();
    const startX = e.clientX;
    const startWidth = sidebarWidth;
    function onMove(ev: MouseEvent) {
      const next = Math.min(SIDEBAR_MAX, Math.max(SIDEBAR_MIN, startWidth + ev.clientX - startX));
      setSidebarWidth(next);
    }
    function onUp() {
      document.removeEventListener("mousemove", onMove);
      document.removeEventListener("mouseup", onUp);
      document.body.style.cursor = "";
      setSidebarWidth((w) => {
        saveConfig({ ...(config ?? ({} as AppConfig)), sidebarWidth: w }).catch(() => {});
        return w;
      });
    }
    document.addEventListener("mousemove", onMove);
    document.addEventListener("mouseup", onUp);
    document.body.style.cursor = "col-resize";
  }

  /** Switch the UI theme: apply it immediately and persist to config. */
  function changeTheme(theme: string) {
    applyTheme(theme);
    setConfig((c) => {
      const next = { ...(c ?? ({} as AppConfig)), theme };
      saveConfig(next).catch(() => {});
      return next;
    });
  }

  /** Toggle whether the sidebar collapses to a hover-expanding rail. */
  function toggleSidebarCollapsible() {
    setSidebarCollapsible((on) => {
      const next = !on;
      setConfig((c) => {
        const cfg = { ...(c ?? ({} as AppConfig)), sidebarCollapsible: next };
        saveConfig(cfg).catch(() => {});
        return cfg;
      });
      return next;
    });
  }

  /** Merge `patch` into a saved profile's `settings` map and persist via the
   * connections store (settings now travel with the connection record). */
  function commitSetting(savedId: string, patch: Record<string, unknown>) {
    const conn = saved.find((s) => s.id === savedId);
    if (!conn) return;
    const next = { ...conn, settings: { ...conn.settings, ...patch } };
    persist(upsert(saved, next));
  }

  /** Read a numeric per-connection setting, falling back to `dflt`. */
  function numSetting(savedId: string, key: string, dflt: number): number {
    const v = saved.find((s) => s.id === savedId)?.settings?.[key];
    return typeof v === "number" ? v : dflt;
  }

  /** The saved workspace tree-panel width for a connection, or `dflt`. */
  function treeWidthFor(savedId: string, dflt: number): number {
    return numSetting(savedId, "treeWidth", dflt);
  }

  /** Persist a connection's workspace tree-panel width. */
  function commitTreeWidth(savedId: string, width: number) {
    commitSetting(savedId, { treeWidth: width });
  }

  /** The saved workspace editor-pane height for a connection, or `dflt`. */
  function editorHeightFor(savedId: string, dflt: number): number {
    return numSetting(savedId, "editorHeight", dflt);
  }

  /** Persist a connection's workspace editor-pane height. */
  function commitEditorHeight(savedId: string, height: number) {
    commitSetting(savedId, { editorHeight: height });
  }

  return (
    <div className="app">
      <Sidebar
        saved={orderedSaved}
        plugins={plugins}
        channel={channel}
        width={sidebarWidth}
        collapsible={sidebarCollapsible}
        openConnections={open}
        activeId={activeId}
        creating={creating || editingId !== null}
        theme={resolveTheme(config?.theme)}
        onThemeChange={changeTheme}
        onToggleCollapsible={toggleSidebarCollapsible}
        onSelect={connectSaved}
        onReorder={reorderConnections}
        onNew={() => {
          setCreating(true);
          setEditingId(null);
          setActiveId(null);
          setCloneDraft(null);
          setConnectError(null);
        }}
        onEdit={(id) => {
          setEditingId(id);
          setCreating(false);
          setCloneDraft(null);
          setConnectError(null);
        }}
        onClone={(id) => {
          const src = saved.find((s) => s.id === id);
          if (!src) return;
          // Seed a brand-new connection form with the source's settings and a
          // "<name> clone" name; a fresh id so it saves as a separate profile.
          setCloneDraft({
            id: genId(),
            name: `${src.name} clone`,
            pluginId: src.pluginId,
            config: { ...src.config },
          });
          setCreating(true);
          setEditingId(null);
          setActiveId(null);
          setConnectError(null);
        }}
        onDelete={(id) => setPendingDelete(id)}
        onDisconnect={disconnect}
        onInstallPlugin={() => setInstalling(true)}
        onCheckUpdates={checkForUpdates}
      />
      <div
        className="sidebar-resizer"
        onMouseDown={startSidebarResize}
        title="Drag to resize sidebar"
        style={sidebarCollapsible ? { display: "none" } : undefined}
      />
      <main className="main">
        <WorkspaceLoaderSlot />
        {showForm ? (
          <ConnectionForm
            key={
              editingId ?? (cloneDraft ? `clone-${cloneDraft.id}` : creating ? "new" : "default")
            }
            plugins={plugins}
            error={formError}
            existing={saved}
            initial={editingProfile}
            prefill={cloneDraft}
            onSaveAndConnect={saveAndConnect}
          />
        ) : (
          <>
            <header className="workspace-header">
              <span className="workspace-title">{active!.name}</span>
              <span className="badge">{active!.pluginId}</span>
            </header>
            {renderWorkspace(active!)}
          </>
        )}
      </main>
      {installing && (
        <InstallPluginDialog
          onClose={() => setInstalling(false)}
          onInstalled={() => refreshPlugins()}
          onUninstalled={() => refreshPlugins()}
        />
      )}
      {update && <UpdateBanner update={update} onDismiss={() => setUpdate(null)} />}
      {pluginUpdates > 0 && !installing && (
        <PluginUpdateBanner
          count={pluginUpdates}
          stacked={Boolean(update)}
          onReview={() => {
            setPluginUpdates(0);
            setInstalling(true);
          }}
          onDismiss={() => setPluginUpdates(0)}
        />
      )}
      {updateNote && <div className="update-toast">{updateNote}</div>}
      {pendingDelete && (
        <ConfirmDialog
          title="Delete connection"
          message={
            <>
              Delete{" "}
              <strong>
                {saved.find((c) => c.id === pendingDelete)?.name ?? "this connection"}
              </strong>
              ? This can't be undone.
            </>
          }
          onCancel={() => setPendingDelete(null)}
          onConfirm={() => {
            const id = pendingDelete;
            setPendingDelete(null);
            void deleteSaved(id);
          }}
        />
      )}
    </div>
  );
}

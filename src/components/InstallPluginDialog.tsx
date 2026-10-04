import { RefObject, useEffect, useMemo, useRef, useState } from "react";
import { api, errString } from "../api/api.ts";
import type { AvailablePlugin, GithubPreview, PluginInfo, PluginStatus } from "../api/api.ts";
import { loadConfig } from "../api/store.ts";
import { Modal } from "./Modal";

interface InstallPluginDialogProps {
  onClose: () => void;
  /** Called after a successful install with the new plugin's info. */
  onInstalled: (info: PluginInfo) => void;
  /** Called after a plugin is uninstalled, with its id. */
  onUninstalled: (pluginId: string) => void;
}

function formatSize(bytes: number): string {
  if (bytes >= 1024 * 1024) return (bytes / (1024 * 1024)).toFixed(1) + " MB";
  if (bytes >= 1024) return (bytes / 1024).toFixed(0) + " KB";
  return bytes + " B";
}

/** The action button label for a plugin's install/update status. */
function actionLabel(status: PluginStatus): string {
  switch (status) {
    case "not_installed":
      return "Install";
    case "update_available":
      return "Update";
    case "up_to_date":
      return "✓ Installed";
    case "unknown":
      return "Reinstall";
  }
}

/** Install a plugin from the configured GitHub repo. The installable plugins
 *  (the app channel's releases) are fetched up front with a per-plugin
 *  install/update status, so there is no repo/tag to type — pick a plugin,
 *  confirm the checksum, install. */
export function InstallPluginDialog({
  onClose,
  onInstalled,
  onUninstalled,
}: InstallPluginDialogProps) {
  const [repo, setRepo] = useState<string>("");
  const [channel, setChannel] = useState<string>("");
  const [available, setAvailable] = useState<AvailablePlugin[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  // Free-text filter over the plugin list (matches name / id / description).
  const [query, setQuery] = useState<string>("");

  // The plugin the user picked, plus its fetched preview (asset + checksum)
  // awaiting confirmation.
  const [selected, setSelected] = useState<AvailablePlugin | null>(null);
  const [preview, setPreview] = useState<GithubPreview | null>(null);
  const [busy, setBusy] = useState<"preview" | "install" | null>(null);
  // Id of the plugin currently being uninstalled, if any.
  const [uninstalling, setUninstalling] = useState<string | null>(null);
  // Id of the plugin awaiting inline uninstall confirmation, if any.
  const [confirmingId, setConfirmingId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  // "Update all" progress: the plugin id currently updating + how many of the
  // batch are done, or null when no bulk update is running.
  const [updatingAll, setUpdatingAll] = useState<{
    id: string;
    done: number;
    total: number;
  } | null>(null);

  const previewRef: RefObject<HTMLDivElement | null> = useRef(null);

  // The list narrowed by the search box (case-insensitive over name/id/desc).
  // "Update all" still acts on the full list, not just what's shown.
  const filtered = useMemo(() => {
    if (!available) return null;
    const q = query.trim().toLowerCase();
    if (!q) return available;
    return available.filter((p) =>
      [p.name, p.id, p.description]
        .filter((s): s is string => Boolean(s))
        .some((s) => s.toLowerCase().includes(q)),
    );
  }, [available, query]);

  useEffect(() => {
    previewRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [preview]);

  // Load the configured repo + app channel, then the available plugin list
  // (each entry already carries its install/update status from the host).
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const [cfg, ch] = await Promise.all([loadConfig(), api.appChannel()]);
        if (cancelled) return;
        setRepo(cfg.pluginRepo);
        setChannel(ch);
        const avail = await api.listGithubPlugins(cfg.pluginRepo);
        if (cancelled) return;
        setAvailable(avail);
      } catch (e) {
        if (!cancelled) setLoadError(errString(e));
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  /** Pick a plugin and fetch its preview (asset + published checksum) for the
   *  confirmation step. Nothing is downloaded or executed yet. */
  async function onSelect(plugin: AvailablePlugin) {
    setError(null);
    setPreview(null);
    setSelected(plugin);
    setBusy("preview");
    try {
      const p = await api.previewGithubPlugin(repo, plugin.tag, plugin.id);
      setPreview(p);
    } catch (e) {
      setError(errString(e));
      setSelected(null);
    } finally {
      setBusy(null);
    }
  }

  async function onInstall() {
    if (!preview || !selected) return;
    setError(null);
    setBusy("install");
    try {
      const info = await api.installGithubPlugin(
        preview.repo,
        preview.tag,
        selected.id,
        preview.sha256,
      );
      // Reflect the new install in the list: this plugin is now up to date.
      setAvailable((list) =>
        (list ?? []).map((p) =>
          p.id === info.id ? { ...p, status: "up_to_date", installedVersion: info.version } : p,
        ),
      );
      setSelected(null);
      setPreview(null);
      onInstalled(info);
    } catch (e) {
      setError(errString(e));
    } finally {
      setBusy(null);
    }
  }

  /** Update every plugin with an available update, one at a time. Each plugin
   *  is previewed (to resolve its asset + published checksum) then installed,
   *  verifying the checksum when one is published. Stops at the first failure
   *  so a broken release doesn't cascade; plugins already updated stay updated. */
  async function onUpdateAll() {
    const outdated = (available ?? []).filter((p) => p.status === "update_available");
    if (outdated.length === 0) return;
    setError(null);
    // Clear any pending single-plugin selection so the two flows don't fight.
    setSelected(null);
    setPreview(null);
    let done = 0;
    for (const plugin of outdated) {
      setUpdatingAll({ id: plugin.id, done, total: outdated.length });
      try {
        const pv = await api.previewGithubPlugin(repo, plugin.tag, plugin.id);
        const info = await api.installGithubPlugin(pv.repo, pv.tag, plugin.id, pv.sha256);
        setAvailable((list) =>
          (list ?? []).map((p) =>
            p.id === info.id ? { ...p, status: "up_to_date", installedVersion: info.version } : p,
          ),
        );
        onInstalled(info);
      } catch (e) {
        setError(`Updating ${plugin.name ?? plugin.id} failed: ${errString(e)}`);
        setUpdatingAll(null);
        return;
      }
      done += 1;
    }
    setUpdatingAll(null);
  }

  async function onUninstall(plugin: AvailablePlugin) {
    setError(null);
    setConfirmingId(null);
    setUninstalling(plugin.id);
    try {
      await api.uninstallPlugin(plugin.id);
      // Reflect removal: this plugin is now installable again.
      setAvailable((list) =>
        (list ?? []).map((p) =>
          p.id === plugin.id ? { ...p, status: "not_installed", installedVersion: null } : p,
        ),
      );
      if (selected?.id === plugin.id) {
        setSelected(null);
        setPreview(null);
      }
      onUninstalled(plugin.id);
    } catch (e) {
      setError(errString(e));
    } finally {
      setUninstalling(null);
    }
  }

  /** Secondary line under a plugin name: size + install/update state. */
  function statusNote(plugin: AvailablePlugin): string {
    const size = formatSize(plugin.sizeBytes);
    const v = plugin.installedVersion;
    switch (plugin.status) {
      case "up_to_date":
        return `${size} · up to date${v ? ` (v${v})` : ""}`;
      case "update_available":
        return `${size} · update available${v ? ` (have v${v})` : ""}`;
      case "unknown":
        return `${size} · installed${v ? ` (v${v})` : ""}`;
      case "not_installed":
        return plugin.availableVersion ? `${size} · v${plugin.availableVersion}` : size;
    }
  }

  return (
    <Modal
      onClose={() => {
        // Don't let a backdrop click / × close the dialog mid-bulk-update;
        // closing would orphan the in-flight install progress.
        if (updatingAll) return;
        onClose();
      }}
      title={
        <>
          Install plugin{repo ? ` from ${repo}` : ""}
          {channel && <span className="channel-tag">{channel}</span>}
        </>
      }
    >
      {loadError && <p className="msg error">{loadError}</p>}

      {!available && !loadError && <p className="muted">Loading plugins…</p>}

      {available && available.length === 0 && !loadError && (
        <p className="muted">
          No installable plugins found for this platform in <code>{repo}</code>.
        </p>
      )}

      {available && available.length > 0 && (
        <div>
          <div className="plugin-list-head">
            <p className="msg warn">Make sure no connections are open while updating plugin.</p>
            {available.some((p) => p.status === "update_available") && (
              <button
                className="primary"
                onClick={onUpdateAll}
                disabled={busy !== null || uninstalling !== null || updatingAll !== null}
              >
                {updatingAll
                  ? `Updating ${updatingAll.done + 1}/${updatingAll.total}…`
                  : `Update all (${available.filter((p) => p.status === "update_available").length})`}
              </button>
            )}
          </div>
          <input
            type="text"
            className="plugin-search"
            placeholder="Search plugins…"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            disabled={updatingAll !== null}
          />
          {filtered && filtered.length === 0 ? (
            <p className="muted">No plugins match “{query.trim()}”.</p>
          ) : (
            <ul className="plugin-list">
              {(filtered ?? []).map((plugin) => {
                const active = selected?.id === plugin.id;
                const upToDate = plugin.status === "up_to_date";
                const installed = plugin.status !== "not_installed";
                const removing = uninstalling === plugin.id;
                const confirming = confirmingId === plugin.id;
                // This row is the one currently being updated by "Update all".
                const bulkUpdating = updatingAll?.id === plugin.id;
                return (
                  <li key={plugin.id} className="plugin-row">
                    <div className="plugin-row-main">
                      <span className="plugin-name">{plugin.name ?? plugin.id}</span>
                      {plugin.description && (
                        <span className="muted small">{plugin.description}</span>
                      )}
                      <span className="muted small">
                        {confirming
                          ? "Delete this plugin's files?"
                          : bulkUpdating
                            ? "Updating…"
                            : statusNote(plugin)}
                      </span>
                    </div>
                    <div className="plugin-row-actions">
                      {confirming ? (
                        <>
                          <button
                            onClick={() => onUninstall(plugin)}
                            disabled={uninstalling !== null}
                            className="danger"
                          >
                            {removing ? "Uninstalling…" : "Confirm"}
                          </button>
                          <button
                            onClick={() => setConfirmingId(null)}
                            disabled={uninstalling !== null}
                          >
                            Cancel
                          </button>
                        </>
                      ) : (
                        <>
                          <button
                            onClick={() => onSelect(plugin)}
                            disabled={
                              busy !== null ||
                              uninstalling !== null ||
                              updatingAll !== null ||
                              upToDate
                            }
                            className={plugin.status === "update_available" ? "primary" : ""}
                          >
                            {bulkUpdating
                              ? "Updating…"
                              : active && busy === "preview"
                                ? "Fetching…"
                                : actionLabel(plugin.status)}
                          </button>
                          {installed && (
                            <button
                              onClick={() => setConfirmingId(plugin.id)}
                              disabled={
                                busy !== null || uninstalling !== null || updatingAll !== null
                              }
                              className="danger"
                            >
                              Uninstall
                            </button>
                          )}
                        </>
                      )}
                    </div>
                  </li>
                );
              })}
            </ul>
          )}
        </div>
      )}

      {selected && preview && (
        <div className="preview" ref={previewRef}>
          <div className="preview-row">
            <span className="muted">Plugin</span>
            <span>{selected.name ?? selected.id}</span>
          </div>
          {selected.description && <p className="muted small">{selected.description}</p>}
          <div className="preview-row">
            <span className="muted">Asset</span>
            <span>{preview.assetName}</span>
          </div>
          <div className="preview-row">
            <span className="muted">Size</span>
            <span>{formatSize(preview.sizeBytes)}</span>
          </div>
          <div className="preview-row">
            <span className="muted">Checksum</span>
            {preview.sha256 ? (
              <span className="ok" title={preview.sha256}>
                ✓ sha256 published
              </span>
            ) : (
              <span className="warn">⚠ no published checksum</span>
            )}
          </div>
          {!preview.sha256 && (
            <p className="warn small">
              This release publishes no checksum. The download cannot be verified — only install if
              you trust this source.
            </p>
          )}
          <p className="muted small">
            Plugins are native executables that run with full access to your machine. Only install
            plugins you trust.
          </p>
          <button className="primary" onClick={onInstall} disabled={busy !== null}>
            {busy === "install"
              ? "Installing…"
              : preview.sha256
                ? "Verify & install"
                : "Install anyway"}
          </button>
        </div>
      )}

      {error && <p className="msg error">{error}</p>}
    </Modal>
  );
}

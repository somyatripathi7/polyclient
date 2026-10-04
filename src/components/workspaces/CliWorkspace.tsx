import { useEffect, useRef, useState } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { listen } from "@tauri-apps/api/event";
import { ptySpawn, ptyWrite, ptyResize, ptySnapshot, ptyClose } from "../../api/api.ts";
import type { ConnectionId } from "../../api/api.ts";
import {
  genId,
  listWorkspaceFiles,
  saveWorkspaceFile,
  deleteWorkspaceFile,
  type WorkspaceFile,
} from "../../api/store.ts";
import { WorkspaceFileList } from "./WorkspaceFileList";
import { CodeEditorV2, type CodeEditorV2Handle } from "../CodeEditorV2.tsx";
import { ConfirmDialog } from "../Modal";
import { useResizable, TREE_MIN, TREE_MAX, EDITOR_MIN, EDITOR_MAX } from "../../useResizable";
import { ConnScope, useConnectionState } from "../../connectionState";
import "@xterm/xterm/css/xterm.css";

interface Props {
  connectionId: ConnectionId;
  /** Stable saved-profile id; scopes saved scripts to this profile. */
  savedId: string;
  /** Initial width (px) of the scripts list, restored from per-connection config. */
  scriptsWidth: number;
  /** Called with the final width (px) when the user finishes dragging. */
  onScriptsWidthChange: (width: number) => void;
  /** Initial height (px) of the editor row, restored from per-connection config. */
  editorHeight: number;
  /** Called with the final height (px) when the user finishes dragging. */
  onEditorHeightChange: (height: number) => void;
}

/** Scripts are stored as `.sh` workspace files (vs `.sql` for RDBMS). */
const SCRIPT_EXT = "sh";

/** Sent before every piped script run. Bash-family shells expand `!` history
 * references (e.g. `!!`) on raw interactive input *before* quote parsing, so a
 * script containing `!` (like `echo "Hi!!!"`) gets silently rewritten from
 * history — and the rewritten line is itself appended to history, compounding
 * on every subsequent run. Disabling history expansion up front keeps `!`
 * literal; redirected because not every remote shell recognizes `set +H`. */
const DISABLE_HISTORY_EXPANSION = "set +H > /dev/null 2>&1\n";

/** One terminal tab: a frontend-minted PTY id + its display title. */
interface TerminalTab {
  id: string;
  title: string;
}

/** A single terminal tab's live xterm bound to one PTY.
 *
 * The ssh PTY lives in the Tauri host (PtyManager) keyed by `terminalId` and
 * outlives this component, so the session survives unmount/remount (switching
 * connections or — because inactive tabs are hidden, not unmounted — switching
 * tabs). On mount we (idempotently) ensure the PTY is running, subscribe to its
 * output, and replay the host's retained scrollback. On unmount we only tear
 * down the xterm + listener — never the PTY (closed explicitly via the tab's ×
 * or on disconnect).
 *
 * Inactive tabs are kept mounted but hidden (`display:none`); xterm can't
 * measure a hidden element, so we re-fit + resize the PTY whenever the tab
 * becomes active again.
 */
function CliTerminal({
  connectionId,
  terminalId,
  active,
  registerWriter,
}: {
  connectionId: ConnectionId;
  terminalId: string;
  active: boolean;
  /** Publish this terminal's PTY writer so the parent can route "Run" to it. */
  registerWriter: (terminalId: string, write: ((text: string) => void) | null) => void;
}) {
  const containerRef = useRef<HTMLDivElement>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const termRef = useRef<Terminal | null>(null);

  useEffect(() => {
    if (!containerRef.current) return;

    const term = new Terminal({
      cursorBlink: true,
      fontSize: 13,
      fontFamily: '"Cascadia Code", "JetBrains Mono", Menlo, monospace',
      allowTransparency: true,
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(containerRef.current);
    fit.fit();
    fitRef.current = fit;
    termRef.current = term;

    const onData = term.onData((data) => {
      const bytes = Array.from(new TextEncoder().encode(data));
      ptyWrite(terminalId, bytes).catch(() => {});
    });

    // Expose a writer so "Run" can pipe a script's text into this PTY.
    registerWriter(terminalId, (text: string) => {
      const bytes = Array.from(new TextEncoder().encode(text));
      ptyWrite(terminalId, bytes).catch(() => {});
    });

    let disposed = false;
    const eventName = `pty://output/${terminalId}`;
    let unlisten: (() => void) | undefined;

    // Subscribe to live output first, then replay scrollback, so we never miss
    // bytes that arrive between the snapshot and the subscription.
    listen<number[]>(eventName, (event) => {
      term.write(new Uint8Array(event.payload));
    })
      .then((fn) => {
        if (disposed) {
          fn();
          return;
        }
        unlisten = fn;
        // Ensure the PTY exists (idempotent), then repaint retained history.
        return ptySpawn(connectionId, terminalId)
          .then(() => ptySnapshot(terminalId))
          .then((bytes) => {
            if (!disposed && bytes.length) {
              term.write(new Uint8Array(bytes));
            }
          });
      })
      .catch((e: unknown) => {
        if (!disposed) term.writeln(`\r\n\x1b[31mSSH error: ${e}\x1b[0m`);
      });

    const ro = new ResizeObserver(() => {
      // A hidden tab has zero size; skip so xterm doesn't collapse to 1 col.
      if (!containerRef.current?.clientHeight) return;
      fit.fit();
      ptyResize(terminalId, term.cols, term.rows).catch(() => {});
    });
    ro.observe(containerRef.current);

    return () => {
      disposed = true;
      onData.dispose();
      unlisten?.();
      ro.disconnect();
      term.dispose();
      fitRef.current = null;
      termRef.current = null;
      registerWriter(terminalId, null);
      // Intentionally NOT closing the PTY here: the host keeps the ssh session
      // alive so a remount (switching tabs/connections) reattaches to it.
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Becoming active: the element was display:none so xterm couldn't measure it.
  // Re-fit and tell the PTY the new size, and focus for typing.
  useEffect(() => {
    if (!active) return;
    const term = termRef.current;
    const fit = fitRef.current;
    if (!term || !fit) return;
    // Defer to after the element is laid out (display flips this same frame).
    const t = setTimeout(() => {
      fit.fit();
      ptyResize(terminalId, term.cols, term.rows).catch(() => {});
      term.focus();
    }, 0);
    return () => clearTimeout(t);
  }, [active, terminalId]);

  return (
    <div
      ref={containerRef}
      className="cli-terminal"
      style={active ? undefined : { display: "none" }}
    />
  );
}

export function CliWorkspace({
  connectionId,
  savedId,
  scriptsWidth,
  onScriptsWidthChange,
  editorHeight,
  onEditorHeightChange,
}: Props) {
  const editorRef = useRef<CodeEditorV2Handle>(null);
  // Scripts list width (px); restored from per-connection config, resizable.
  const [width, setWidth] = useState(scriptsWidth);
  const scriptsResize = useResizable({
    width,
    min: TREE_MIN,
    max: TREE_MAX,
    onChange: setWidth,
    onCommit: onScriptsWidthChange,
  });
  // Editor row height (px); restored from per-connection config, resizable via
  // the horizontal handle between the editor and the terminal below it.
  const [topHeight, setTopHeight] = useState(editorHeight);
  const editorResize = useResizable({
    width: topHeight,
    min: EDITOR_MIN,
    max: EDITOR_MAX,
    onChange: setTopHeight,
    onCommit: onEditorHeightChange,
    axis: "y",
  });
  // Live PTY writers per terminal, published by each mounted CliTerminal so
  // "Run" can pipe a script into the *active* tab.
  const writers = useRef<Map<string, (text: string) => void>>(new Map());
  const registerWriter = (terminalId: string, write: ((text: string) => void) | null) => {
    if (write) writers.current.set(terminalId, write);
    else writers.current.delete(terminalId);
  };
  // Terminals that have already had history expansion disabled — `set +H`
  // persists for the shell's lifetime, so it only needs sending once, not
  // before every Run (which would otherwise echo as a visible extra line).
  const historyExpansionDisabled = useRef<Set<string>>(new Set());
  function withHistoryGuard(terminalId: string, body: string): string {
    if (historyExpansionDisabled.current.has(terminalId)) return body;
    historyExpansionDisabled.current.add(terminalId);
    return DISABLE_HISTORY_EXPANSION + body;
  }

  // Terminal tabs + the active one. Stored in the session state store so they
  // survive the workspace unmount/remount that happens on every connection
  // switch (the PTYs themselves are kept alive by the host); see
  // connectionState.ts. Seed one tab on first mount so behaviour matches the
  // old single-terminal workspace.
  const scope = ConnScope(savedId, "cli");
  const [tabs, setTabs] = useConnectionState<TerminalTab[]>(scope, "terminals", []);
  const [activeTab, setActiveTab] = useConnectionState<string | null>(
    scope,
    "activeTerminal",
    null,
  );
  // Monotonic counter for terminal titles ("Terminal N"). Persisted so numbers
  // always increase across adds — deriving the number from the current tab
  // count would reuse a number after a tab in the middle is closed.
  const [nextNo, setNextNo] = useConnectionState<number>(scope, "nextTerminalNo", 1);
  useEffect(() => {
    if (tabs.length === 0) {
      const id = genId();
      setTabs([{ id, title: "Terminal 1" }]);
      setActiveTab(id);
      setNextNo(2);
    } else if (!activeTab || !tabs.some((t) => t.id === activeTab)) {
      setActiveTab(tabs[0].id);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  function addTab() {
    const id = genId();
    setTabs((prev) => [...prev, { id, title: `Terminal ${nextNo}` }]);
    setNextNo((n) => n + 1);
    setActiveTab(id);
  }

  function closeTab(id: string) {
    ptyClose(id).catch(() => {});
    const idx = tabs.findIndex((t) => t.id === id);
    const next = tabs.filter((t) => t.id !== id);
    setTabs(next);
    // If we closed the active tab, fall back to a neighbour.
    if (activeTab === id) {
      const neighbour = next[idx] ?? next[idx - 1] ?? next[0] ?? null;
      setActiveTab(neighbour ? neighbour.id : null);
    }
  }

  // Saved scripts for this profile, the editor buffer, and the active file.
  // The editor buffer + active file persist across connection switches; see
  // connectionState.ts.
  const [files, setFiles] = useState<WorkspaceFile[]>([]);
  const [script, setScript] = useConnectionState(scope, "script", "");
  const [activeFile, setActiveFile] = useConnectionState<string | null>(scope, "activeFile", null);
  const [newName, setNewName] = useState<string | null>(null);
  const [pendingDelete, setPendingDelete] = useState<string | null>(null);

  // ----- Saved scripts -----
  useEffect(() => {
    listWorkspaceFiles(savedId, SCRIPT_EXT)
      .then(setFiles)
      .catch(() => setFiles([]));
  }, [savedId]);

  function loadFile(f: WorkspaceFile) {
    setActiveFile(f.name);
    setScript(f.content);
  }

  // Autosave the active script ~800ms after the last keystroke. No-op when
  // nothing's loaded or the content already matches what's on disk.
  useEffect(() => {
    if (!activeFile) return;
    const stored = files.find((f) => f.name === activeFile);
    if (!stored || stored.content === script) return;
    const t = setTimeout(() => {
      saveWorkspaceFile(savedId, activeFile, script, SCRIPT_EXT)
        .then(() =>
          setFiles((prev) =>
            prev.map((f) => (f.name === activeFile ? { ...f, content: script } : f)),
          ),
        )
        .catch(() => {});
    }, 800);
    return () => clearTimeout(t);
  }, [script, activeFile, files, savedId]);

  async function saveNew() {
    const name = (newName ?? "").trim();
    if (!name) {
      setNewName(null);
      return;
    }
    await saveWorkspaceFile(savedId, name, script, SCRIPT_EXT);
    setFiles(await listWorkspaceFiles(savedId, SCRIPT_EXT));
    setActiveFile(name);
    setNewName(null);
  }

  async function confirmDelete() {
    const name = pendingDelete;
    if (!name) return;
    await deleteWorkspaceFile(savedId, name, SCRIPT_EXT);
    setFiles(await listWorkspaceFiles(savedId, SCRIPT_EXT));
    if (activeFile === name) {
      setActiveFile(null);
      setScript("");
    }
    setPendingDelete(null);
  }

  /** Pipe a script's text into the active terminal (with a trailing newline so
   * the shell executes the final line). */
  function runScript(text: string) {
    const trimmed = text.trim();
    if (!trimmed) return;
    if (!activeTab) return;
    const write = writers.current.get(activeTab);
    if (!write) return;
    const body = text.endsWith("\n") ? text : text + "\n";
    write(withHistoryGuard(activeTab, body));
  }

  /** Pipe a script's text into *every* open terminal at once. */
  function runScriptAll(text: string) {
    const trimmed = text.trim();
    if (!trimmed) return;
    const body = text.endsWith("\n") ? text : text + "\n";
    for (const t of tabs) {
      writers.current.get(t.id)?.(withHistoryGuard(t.id, body));
    }
  }

  /** Run-on-all button: run the selection if any, else the whole script, on
   * every open terminal. */
  function runAllSelectionOrAll() {
    runScriptAll(selectedText() ?? script);
  }

  /** The current selection in the editor, or null if nothing is selected. */
  function selectedText(): string | null {
    const sel = editorRef.current?.getSelection() ?? "";
    return sel === "" ? null : sel;
  }

  /** The full line the cursor sits on (newlines stripped at the boundaries). */
  function cursorLine(): string {
    return editorRef.current?.getCursorLine() ?? "";
  }

  /** Run button: run the selection if any, else the whole script. */
  function runSelectionOrAll() {
    runScript(selectedText() ?? script);
  }

  /** Ctrl/Cmd+Enter: run the selection if any, else the line the cursor is on. */
  function runSelectionOrLine() {
    runScript(selectedText() ?? cursorLine());
  }

  // True while the active script has edits not yet flushed to disk by autosave.
  const unsaved =
    activeFile != null && files.find((f) => f.name === activeFile)?.content !== script;

  return (
    <div className="workspace cli-workspace">
      <div className="cli-top" style={{ height: topHeight }}>
        <div className="cli-scripts" style={{ width }}>
          <WorkspaceFileList
            files={files}
            activeFile={activeFile}
            newName={newName}
            onToggleAdd={() => setNewName(newName === null ? "" : null)}
            onNewNameChange={setNewName}
            onSave={saveNew}
            onCancelAdd={() => setNewName(null)}
            onLoad={loadFile}
            onRequestDelete={setPendingDelete}
            onRun={(f) => runScript(f.content)}
            label="Scripts"
            ext={SCRIPT_EXT}
            addTitle="Save current script as a file"
            emptyText="No scripts."
          />
        </div>
        <div
          className="tree-resizer"
          onMouseDown={scriptsResize.onMouseDown}
          title="Drag to resize"
        />
        <div className="cli-editor">
          <div className="cli-editor-head">
            <span className="editor-file-name">{activeFile ? `${activeFile}.sh` : "Script"}</span>
            {activeFile && (
              <span className="muted cli-save-status">{unsaved ? "Saving…" : "Saved"}</span>
            )}
            <div className="cli-editor-actions">
              <button
                className="primary"
                title="Run selection, or whole script, in the active terminal (Cmd/Ctrl+Enter runs the current line)"
                disabled={!script.trim()}
                onClick={runSelectionOrAll}
              >
                ▶ Run
              </button>
              {tabs.length > 1 && (
                <button
                  title={`Run selection, or whole script, in all ${tabs.length} terminals`}
                  disabled={!script.trim()}
                  onClick={runAllSelectionOrAll}
                >
                  ⏩ Run on all
                </button>
              )}
            </div>
          </div>
          <CodeEditorV2
            handleRef={editorRef}
            className="cli-editor-area"
            language="bash"
            value={script}
            placeholder="# Write a shell script, then Run to pipe it into the SSH session…"
            onChange={setScript}
            keybindings={[{ key: "Mod-Enter", run: runSelectionOrLine }]}
          />
        </div>
      </div>
      <div
        className="pane-resizer-y"
        onMouseDown={editorResize.onMouseDown}
        title="Drag to resize the editor"
      />
      <div className="cli-tabs">
        {tabs.map((t) => (
          <div
            key={t.id}
            className={"cli-tab" + (t.id === activeTab ? " active" : "")}
            onClick={() => setActiveTab(t.id)}
          >
            <span className="cli-tab-title">{t.title}</span>
            {tabs.length > 1 && (
              <button
                className="cli-tab-close"
                title="Close terminal"
                onClick={(e) => {
                  e.stopPropagation();
                  closeTab(t.id);
                }}
              >
                ×
              </button>
            )}
          </div>
        ))}
        <button className="cli-tab-add" title="New terminal" onClick={addTab}>
          +
        </button>
      </div>
      <div className="cli-terminals">
        {tabs.map((t) => (
          <CliTerminal
            key={t.id}
            connectionId={connectionId}
            terminalId={t.id}
            active={t.id === activeTab}
            registerWriter={registerWriter}
          />
        ))}
      </div>
      {pendingDelete && (
        <ConfirmDialog
          title="Delete script"
          message={`Delete "${pendingDelete}.sh"? This cannot be undone.`}
          confirmLabel="Delete"
          onConfirm={confirmDelete}
          onCancel={() => setPendingDelete(null)}
        />
      )}
    </div>
  );
}

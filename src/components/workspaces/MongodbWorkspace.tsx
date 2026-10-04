import { useEffect, useRef, useState } from "react";
import { api, errString } from "../../api/api.ts";
import type { ConnectionId } from "../../api/api.ts";
import type { MongoCollection } from "../../api/document.ts";
import type { WorkspaceFile } from "../../api/store.ts";
import { listWorkspaceFiles, saveWorkspaceFile, deleteWorkspaceFile } from "../../api/store.ts";
import { useResizable, TREE_MIN, TREE_MAX } from "../../useResizable";
import { ConnScope, useConnectionState } from "../../connectionState";
import { CodeEditorV2, type CodeEditorV2Handle } from "../CodeEditorV2.tsx";
import { ConfirmDialog } from "../Modal";
import { NavTree } from "./NavTree";
import { WorkspaceFileList } from "./WorkspaceFileList";
import {
  STAGE_OPS,
  DEFAULT_PIPELINE,
  newStage,
  stageDefault,
  formatDoc,
  formatDocJson,
  formatRunResult,
  cursorBatch,
  describeIndex,
  buildFindCommand,
  buildAggregateCommand,
  buildListIndexesCommand,
  findScript,
  aggregateScript,
  docId,
  buildDeleteCommand,
  buildReplaceCommand,
  type Stage,
  type DocsResult,
  type StatementResult,
} from "./mongodb/mongo.ts";
import { executeScript, statementAtCursor } from "./mongodb/script.ts";

/** Saved script files use this extension (a mongosh-like multi-statement script). */
const SCRIPT_EXT = "mongo";

/** The per-collection views plus the database-scoped Script tab. */
type Tab = "documents" | "aggregation" | "indexes" | "script";

interface Props {
  connectionId: ConnectionId;
  /** Stable saved-profile id; scopes session-preserved workspace state and the
   * saved script files to this profile. */
  savedId: string;
  /** Configured default database; opened automatically on connect when present
   * (and known to the server). Null when none is configured. */
  database: string | null;
  /** Initial width (px) of the tree panel, restored from per-connection config. */
  treeWidth: number;
  /** Called with the final width (px) when the user finishes dragging. */
  onTreeWidthChange: (width: number) => void;
}

export function MongodbWorkspace({
  connectionId,
  savedId,
  database,
  treeWidth,
  onTreeWidthChange,
}: Props) {
  // Tree panel width (px); restored from per-connection config, resizable.
  const [width, setWidth] = useState(treeWidth);
  const treeResize = useResizable({
    width,
    min: TREE_MIN,
    max: TREE_MAX,
    onChange: setWidth,
    onCommit: onTreeWidthChange,
  });
  // Session-preserved workspace state (see connectionState.ts): survives the
  // unmount a connection switch causes, keyed by the stable saved-profile id.
  const scope = ConnScope(savedId, "document");
  // Databases on the server (drive the picker) and the one currently selected.
  const [databases, setDatabases] = useConnectionState<string[]>(scope, "databases", []);
  const [currentDatabase, setCurrentDatabase] = useConnectionState<string | null>(
    scope,
    "currentDatabase",
    null,
  );
  // Collections cache keyed by database; only the current database's list shows.
  const [collections, setCollections] = useConnectionState<Record<string, MongoCollection[]>>(
    scope,
    "collections",
    {},
  );
  // The expanded database node in the tree (its name), or null when collapsed.
  const [openDb, setOpenDb] = useConnectionState<string | null>(scope, "openDb", null);
  // The selected collection (within the current database), or null.
  const [active, setActive] = useConnectionState<string | null>(scope, "active", null);
  // Which tab is showing (per-collection views, or the database-scoped Script).
  const [tab, setTab] = useConnectionState<Tab>(scope, "tab", "documents");
  const [filter, setFilter] = useConnectionState(scope, "filter", "{}");
  const [limit, setLimit] = useConnectionState(scope, "limit", 50);
  const [result, setResult] = useConnectionState<DocsResult | null>(scope, "result", null);
  // Aggregation builder state (scoped to the active collection; reset on pick).
  const [pipeline, setPipeline] = useConnectionState<Stage[]>(scope, "pipeline", DEFAULT_PIPELINE);
  const [aggResult, setAggResult] = useConnectionState<DocsResult | null>(scope, "aggResult", null);
  // Indexes for the active collection; null means "not loaded yet" (lazy-loaded
  // when the Indexes tab is opened).
  const [indexes, setIndexes] = useConnectionState<unknown[] | null>(scope, "indexes", null);

  // --- Script tab state (database-scoped, mongosh-like multi-statement) ----
  const [scriptText, setScriptText] = useConnectionState(scope, "scriptText", "");
  const [scriptResults, setScriptResults] = useConnectionState<StatementResult[] | null>(
    scope,
    "scriptResults",
    null,
  );
  // Saved `.mongo` script files for this connection profile (the "Scripts" section).
  const [scriptFiles, setScriptFiles] = useState<WorkspaceFile[]>([]);
  const [newScriptName, setNewScriptName] = useState<string | null>(null);
  const [scriptActiveFile, setScriptActiveFile] = useConnectionState<string | null>(
    scope,
    "scriptActiveFile",
    null,
  );
  const [confirmDeleteScript, setConfirmDeleteScript] = useState<string | null>(null);

  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // Brief "Copied" flash on the Documents/Aggregation copy-query buttons.
  const [copied, setCopied] = useState(false);
  // Per-document actions in the Documents result (Compass-style edit/copy/delete).
  const [editingDoc, setEditingDoc] = useState<number | null>(null);
  const [editDocText, setEditDocText] = useState("");
  const [copiedDoc, setCopiedDoc] = useState<string | null>(null);
  const [confirmDeleteDoc, setConfirmDeleteDoc] = useState<number | null>(null);

  const filterEditorRef = useRef<CodeEditorV2Handle>(null);
  const scriptEditorRef = useRef<CodeEditorV2Handle>(null);

  // Whether a database was already selected on mount (state restored from a
  // connection switch-back); if so, don't auto-select and clobber it.
  const restoredOnMount = useRef(currentDatabase !== null);

  useEffect(() => {
    api
      .docListDatabases(connectionId)
      .then((dbs) => {
        setDatabases(dbs);
        if (restoredOnMount.current) return;
        // Prefer the configured default DB; otherwise, with a single database,
        // select it so the user doesn't have to. With several and no default,
        // leave the picker on its "Select a database…" prompt.
        let pick: string | null = null;
        if (database && dbs.includes(database)) pick = database;
        else if (dbs.length === 1) pick = dbs[0];
        if (pick) {
          setCurrentDatabase(pick);
          setOpenDb(pick);
          void loadCollections(pick);
        }
      })
      .catch((e) => setError(errString(e)));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [connectionId]);

  // Load this profile's saved script files.
  useEffect(() => {
    listWorkspaceFiles(savedId, SCRIPT_EXT)
      .then(setScriptFiles)
      .catch(() => setScriptFiles([]));
  }, [savedId]);

  // Lazily load indexes the first time the Indexes tab is shown for a collection.
  useEffect(() => {
    if (tab === "indexes" && active && indexes === null) void loadIndexes();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tab, active, indexes]);

  // Auto-save edits to the active script file (debounced) so we don't write on
  // every keystroke. No-op when nothing's loaded or content already matches disk.
  useEffect(() => {
    if (!scriptActiveFile) return;
    const stored = scriptFiles.find((f) => f.name === scriptActiveFile);
    if (!stored || stored.content === scriptText) return;
    const t = setTimeout(() => {
      saveWorkspaceFile(savedId, scriptActiveFile, scriptText, SCRIPT_EXT)
        .then(() =>
          setScriptFiles((prev) =>
            prev.map((f) => (f.name === scriptActiveFile ? { ...f, content: scriptText } : f)),
          ),
        )
        .catch((e) => setError(errString(e)));
    }, 800);
    return () => clearTimeout(t);
  }, [scriptText, scriptActiveFile, scriptFiles, savedId]);

  async function loadCollections(db: string) {
    try {
      const c = await api.docListCollections(connectionId, db);
      setCollections((m) => ({ ...m, [db]: c }));
    } catch (e) {
      setError(errString(e));
    }
  }

  /** Drop any per-collection view state (used when the selected collection or
   * database changes). */
  function resetCollectionViews() {
    setResult(null);
    setAggResult(null);
    setIndexes(null);
    setPipeline(DEFAULT_PIPELINE);
    setEditingDoc(null);
    setConfirmDeleteDoc(null);
  }

  /** Switch the picker to another database and load its collections. Everything
   * shown belonged to the previous database, so the selection/result reset. */
  async function switchDatabase(db: string) {
    if (db === currentDatabase) return;
    setCurrentDatabase(db);
    setOpenDb(db);
    setActive(null);
    resetCollectionViews();
    if (!collections[db]) await loadCollections(db);
  }

  /** Reload the database list and the current database's collections from the
   * server, picking up anything created since the tree was last loaded. */
  async function refreshTree() {
    setBusy(true);
    setError(null);
    try {
      const dbs = await api.docListDatabases(connectionId);
      setDatabases(dbs);
      if (currentDatabase && dbs.includes(currentDatabase)) {
        const c = await api.docListCollections(connectionId, currentDatabase);
        setCollections((m) => ({ ...m, [currentDatabase]: c }));
      } else {
        setCurrentDatabase(null);
        setOpenDb(null);
        setCollections({});
        setActive(null);
        resetCollectionViews();
      }
    } catch (e) {
      setError(errString(e));
    } finally {
      setBusy(false);
    }
  }

  async function pick(coll: string) {
    setActive(coll);
    setTab("documents");
    resetCollectionViews();
    setError(null);
    await runFind(coll);
  }

  /** Run the Documents filter as a `find` command via `runCommand`. */
  async function runFind(coll?: string) {
    const db = currentDatabase;
    const target = coll ?? active;
    if (!db || !target) return;
    let command: string;
    try {
      // The filter box accepts MongoDB shell syntax (`{ _id: ObjectId('...') }`).
      command = buildFindCommand(target, filter, limit);
    } catch (e) {
      setError("Invalid filter: " + errString(e));
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const rc = await api.docRunCommand(connectionId, db, command);
      setResult({ documents: cursorBatch(rc), elapsed_ms: rc.elapsed_ms });
    } catch (e) {
      setError(errString(e));
      setResult(null);
    } finally {
      setBusy(false);
    }
  }

  /** Run the builder's pipeline as an `aggregate` command via `runCommand`. */
  async function runAggregate() {
    const db = currentDatabase;
    const target = active;
    if (!db || !target) return;
    let command: string;
    try {
      command = buildAggregateCommand(target, pipeline, limit);
    } catch (e) {
      setError("Invalid pipeline stage: " + errString(e));
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const rc = await api.docRunCommand(connectionId, db, command);
      setAggResult({ documents: cursorBatch(rc), elapsed_ms: rc.elapsed_ms });
    } catch (e) {
      setError(errString(e));
      setAggResult(null);
    } finally {
      setBusy(false);
    }
  }

  /** Load the active collection's indexes via a `listIndexes` command. */
  async function loadIndexes(coll?: string) {
    const db = currentDatabase;
    const target = coll ?? active;
    if (!db || !target) return;
    setBusy(true);
    setError(null);
    try {
      const rc = await api.docRunCommand(connectionId, db, buildListIndexesCommand(target));
      setIndexes(cursorBatch(rc));
    } catch (e) {
      setError(errString(e));
      setIndexes([]);
    } finally {
      setBusy(false);
    }
  }

  /** Run the Script tab's text: each statement is executed as JavaScript with a
   * `db` runtime in scope, building and running one command per method call.
   * `srcOverride` runs that text instead of the editor's (used by ▶ run-from-list,
   * whose `setScriptText` hasn't applied yet). */
  async function runScript(srcOverride?: string) {
    const db = currentDatabase;
    if (!db) return;
    const src = srcOverride ?? scriptText;
    setBusy(true);
    setError(null);
    setScriptResults(null);
    try {
      const results = await executeScript(
        src,
        limit,
        db,
        (commandJson) => api.docRunCommand(connectionId, db, commandJson),
        (partial) => setScriptResults(partial),
      );
      setScriptResults(results);
    } catch (e) {
      setError(errString(e));
    } finally {
      setBusy(false);
    }
  }

  /** ⌘/Ctrl+Enter: run the selection if any, else the statement the cursor is
   *  in — not the whole script (the Run button does that). */
  function runFromEditor() {
    if (busy) return;
    const ed = scriptEditorRef.current;
    if (!ed) return;
    const sel = ed.getSelection();
    const stmt = sel.trim() ? sel : statementAtCursor(ed.getValue(), ed.getCursorOffset());
    if (stmt.trim()) void runScript(stmt);
  }

  /** Copy the current Documents/Aggregation query as a runnable mongosh script. */
  async function copyQuery() {
    if (!active) return;
    const script =
      tab === "aggregation" ? aggregateScript(active, pipeline) : findScript(active, filter, limit);
    try {
      await navigator.clipboard.writeText(script);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch (e) {
      setError(errString(e));
    }
  }

  /** Copy a single result document to the clipboard — `shell` syntax
   * (`ObjectId('…')`) or raw relaxed JSON. */
  async function copyDoc(i: number, kind: "shell" | "json") {
    const doc = result?.documents[i];
    if (doc === undefined) return;
    const text = kind === "json" ? formatDocJson(doc) : formatDoc(doc);
    const key = `${i}-${kind}`;
    try {
      await navigator.clipboard.writeText(text);
      setCopiedDoc(key);
      setTimeout(() => setCopiedDoc((c) => (c === key ? null : c)), 1500);
    } catch (e) {
      setError(errString(e));
    }
  }

  /** Open the inline editor for a result document. */
  function startEditDoc(i: number) {
    const doc = result?.documents[i];
    if (doc === undefined) return;
    setEditDocText(formatDoc(doc));
    setEditingDoc(i);
    setError(null);
  }

  /** Save the edited document: replace the matched (_id) document, then refresh. */
  async function saveEditDoc() {
    const db = currentDatabase;
    if (!db || !active) return;
    let command: string;
    try {
      command = buildReplaceCommand(active, editDocText);
    } catch (e) {
      setError("Invalid document: " + errString(e));
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await api.docRunCommand(connectionId, db, command);
      setEditingDoc(null);
      await runFind();
    } catch (e) {
      setError(errString(e));
    } finally {
      setBusy(false);
    }
  }

  /** Delete a result document by its _id (after inline confirmation), then refresh. */
  async function deleteDoc(i: number) {
    setConfirmDeleteDoc(null);
    const db = currentDatabase;
    const doc = result?.documents[i];
    if (!db || !active || doc === undefined) return;
    let command: string;
    try {
      command = buildDeleteCommand(active, doc);
    } catch (e) {
      setError(errString(e));
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await api.docRunCommand(connectionId, db, command);
      await runFind();
    } catch (e) {
      setError(errString(e));
    } finally {
      setBusy(false);
    }
  }

  // --- Aggregation builder mutations ---------------------------------------
  function updateStage(i: number, patch: Partial<Stage>) {
    setPipeline((p) => p.map((s, idx) => (idx === i ? { ...s, ...patch } : s)));
  }
  /** Change a stage's operator, dropping the new operator's default snippet
   * into the editor as the stage body. */
  function changeStageOp(i: number, op: string) {
    setPipeline((p) => p.map((s, idx) => (idx === i ? { ...s, op, body: stageDefault(op) } : s)));
  }
  function addStage() {
    setPipeline((p) => [...p, newStage()]);
  }
  function removeStage(i: number) {
    setPipeline((p) => (p.length > 1 ? p.filter((_, idx) => idx !== i) : DEFAULT_PIPELINE));
  }

  /** Save the current script under `name` (from the inline name input). */
  async function saveCurrentScript() {
    const name = newScriptName?.trim();
    if (!name) {
      setNewScriptName(null);
      return;
    }
    if (scriptFiles.some((f) => f.name.toLowerCase() === name.toLowerCase())) {
      setError(`A script named "${name}" already exists.`);
      return;
    }
    try {
      await saveWorkspaceFile(savedId, name, scriptText, SCRIPT_EXT);
      setScriptFiles(await listWorkspaceFiles(savedId, SCRIPT_EXT));
      setNewScriptName(null);
      setScriptActiveFile(name);
      setError(null);
    } catch (e) {
      setError(errString(e));
    }
  }

  /** Delete a saved script file (after inline confirmation). */
  async function removeScriptFile(name: string) {
    setConfirmDeleteScript(null);
    try {
      await deleteWorkspaceFile(savedId, name, SCRIPT_EXT);
      setScriptFiles(await listWorkspaceFiles(savedId, SCRIPT_EXT));
      if (scriptActiveFile === name) setScriptActiveFile(null);
    } catch (e) {
      setError(errString(e));
    }
  }

  /** Open a saved script file: load it into the script editor. */
  function loadScriptFile(file: WorkspaceFile) {
    setScriptText(file.content);
    setScriptActiveFile(file.name);
    setTab("script");
  }

  const scriptDirty =
    !!scriptActiveFile &&
    scriptFiles.find((f) => f.name === scriptActiveFile)?.content !== scriptText;
  const shownCollections = currentDatabase
    ? [...(collections[currentDatabase] ?? [])].sort((a, b) => a.name.localeCompare(b.name))
    : [];

  return (
    <div className="workspace">
      <div className="tree" style={{ width }}>
        <div className="tree-top">
          <WorkspaceFileList
            files={scriptFiles}
            activeFile={scriptActiveFile}
            newName={newScriptName}
            onToggleAdd={() => {
              if (newScriptName === null) {
                // Start a new script: blank editor shown in the script view,
                // with the inline name input open to save it.
                setScriptText("");
                setScriptActiveFile(null);
                setScriptResults(null);
                setTab("script");
                setNewScriptName("");
              } else {
                setNewScriptName(null);
              }
            }}
            onNewNameChange={setNewScriptName}
            onSave={saveCurrentScript}
            onCancelAdd={() => setNewScriptName(null)}
            onLoad={loadScriptFile}
            onRequestDelete={setConfirmDeleteScript}
            onRun={(f) => {
              loadScriptFile(f);
              void runScript(f.content);
            }}
            label="Scripts"
            ext={SCRIPT_EXT}
            addTitle="Save current script as a file"
            emptyText="No saved scripts."
          />
          <div className="tree-dbselect">
            {databases.length > 0 && <span className="field-label">Database</span>}
            <div className="tree-db-row">
              {databases.length > 0 && (
                <select
                  value={currentDatabase ?? ""}
                  onChange={(e) => switchDatabase(e.target.value)}
                >
                  {currentDatabase === null && (
                    <option value="" disabled>
                      Select a database…
                    </option>
                  )}
                  {databases.map((d) => (
                    <option key={d} value={d}>
                      {d}
                    </option>
                  ))}
                </select>
              )}
              <button
                className="tree-refresh"
                disabled={busy}
                title="Reload databases and collections"
                onClick={refreshTree}
              >
                ↻
              </button>
            </div>
          </div>
        </div>
        <NavTree
          groups={currentDatabase ? [currentDatabase] : []}
          items={
            currentDatabase
              ? { [currentDatabase]: shownCollections.map((c) => ({ name: c.name })) }
              : {}
          }
          openGroup={openDb}
          activeKey={active && currentDatabase ? currentDatabase + "." + active : null}
          onToggleGroup={(name) => setOpenDb((o) => (o === name ? null : name))}
          onPickItem={(_db, name) => pick(name)}
          emptyText="Select a database."
        />
      </div>
      <div className="tree-resizer" onMouseDown={treeResize.onMouseDown} title="Drag to resize" />
      <div className="editor-pane">
        {active && tab !== "script" && (
          <div className="tabs">
            <span className="mongo-tabs-coll">
              {currentDatabase} / {active}
            </span>
            <button
              className={"tab" + (tab === "documents" ? " active" : "")}
              onClick={() => setTab("documents")}
            >
              Documents
            </button>
            <button
              className={"tab" + (tab === "aggregation" ? " active" : "")}
              onClick={() => setTab("aggregation")}
            >
              Aggregation
            </button>
            <button
              className={"tab" + (tab === "indexes" ? " active" : "")}
              onClick={() => setTab("indexes")}
            >
              Indexes
            </button>
          </div>
        )}

        {tab === "script" ? (
          <>
            {scriptActiveFile && (
              <div className="editor-file">
                <span className="editor-file-name">
                  {scriptActiveFile}.{SCRIPT_EXT}
                </span>
                {scriptDirty && (
                  <span className="editor-file-dirty" title="Auto-saving…">
                    ●
                  </span>
                )}
              </div>
            )}
            <div className="editor-code-pane" style={{ height: 220 }}>
              <CodeEditorV2
                handleRef={scriptEditorRef}
                className="code"
                language="javascript"
                value={scriptText}
                onChange={setScriptText}
                placeholder={"db.users.find({ active: true }).limit(5);"}
                lineWrapping
                keybindings={[{ key: "Mod-Enter", run: () => void runFromEditor() }]}
              />
            </div>
            <div className="editor-toolbar">
              <label className="row">
                Limit{" "}
                <input
                  type="number"
                  min={1}
                  value={limit}
                  style={{ width: 80 }}
                  onChange={(e) => setLimit(Number(e.target.value) || 1)}
                />
              </label>
              <button
                className="primary"
                disabled={busy || !currentDatabase}
                onClick={() => runScript()}
                title="Run the whole script"
              >
                Run all
              </button>
              {!currentDatabase ? (
                <span className="status-line">Select a database first.</span>
              ) : (
                <span className="status-line muted">
                  ⌘/Ctrl+Enter runs the selection, or the statement at the cursor
                </span>
              )}
              {scriptResults && (
                <span className="status-line">
                  {scriptResults.length} result(s) · {scriptResults.filter((r) => !r.error).length}{" "}
                  ok · {scriptResults.filter((r) => r.error).length} error(s)
                </span>
              )}
            </div>
            {error && <div className="status-line error">{error}</div>}
            {scriptResults && (
              <div className="result-scroll">
                {scriptResults.map((r, i) => {
                  const docs = r.result ? formatRunResult(r.result) : [];
                  return (
                    <div className="script-result" key={i}>
                      <div className="script-result-head">
                        <span className="script-result-num">#{r.index}</span>
                        <code className="script-result-src">
                          {r.method ? `${r.collection}.${r.method}()` : r.source}
                        </code>
                        {r.result && <span className="status-line">{r.result.elapsed_ms} ms</span>}
                      </div>
                      {r.error ? (
                        <div className="status-line error">{r.error}</div>
                      ) : r.result ? (
                        docs.length > 0 ? (
                          docs.map((doc, j) => (
                            <CodeEditorV2
                              key={j}
                              className="doc-card"
                              language="javascript"
                              value={doc}
                              readOnly
                              lineWrapping
                            />
                          ))
                        ) : (
                          <div className="status-line muted script-empty">No documents.</div>
                        )
                      ) : r.value !== undefined ? (
                        <CodeEditorV2
                          className="doc-card"
                          language="javascript"
                          value={r.value}
                          readOnly
                          lineWrapping
                        />
                      ) : null}
                    </div>
                  );
                })}
              </div>
            )}
          </>
        ) : !active ? (
          <div className="empty-hint">
            Select a collection to view its documents, build an aggregation, or inspect its indexes
            — or open a script to run shell commands.
          </div>
        ) : (
          <>
            {tab === "documents" && (
              <>
                <div className="editor-code-pane" style={{ height: 160 }}>
                  <CodeEditorV2
                    handleRef={filterEditorRef}
                    className="code"
                    language="javascript"
                    value={filter}
                    onChange={setFilter}
                    placeholder="{ }"
                    lineWrapping
                    keybindings={[{ key: "Mod-Enter", run: () => void runFind() }]}
                  />
                </div>
                <div className="editor-toolbar">
                  <label className="row">
                    Limit{" "}
                    <input
                      type="number"
                      min={1}
                      value={limit}
                      style={{ width: 80 }}
                      onChange={(e) => setLimit(Number(e.target.value) || 1)}
                    />
                  </label>
                  <button className="primary" disabled={busy} onClick={() => runFind()}>
                    Find
                  </button>
                  <button
                    className="ghost"
                    title="Copy as a runnable mongosh script"
                    onClick={() => void copyQuery()}
                  >
                    {copied ? "Copied" : "Copy query"}
                  </button>
                  {result && (
                    <span className="status-line">
                      {result.documents.length} doc(s) · {result.elapsed_ms} ms
                    </span>
                  )}
                </div>
                {error && <div className="status-line error">{error}</div>}
                {result && (
                  <div className="result-scroll">
                    {result.documents.length === 0 && (
                      <div className="status-line muted script-empty">No documents.</div>
                    )}
                    {result.documents.map((d, i) => {
                      const hasId = docId(d) !== null;
                      if (editingDoc === i) {
                        return (
                          <div className="doc-row editing" key={i}>
                            <CodeEditorV2
                              className="doc-card"
                              language="javascript"
                              value={editDocText}
                              onChange={setEditDocText}
                              lineWrapping
                              keybindings={[{ key: "Mod-Enter", run: () => void saveEditDoc() }]}
                            />
                            <div className="doc-actions">
                              <button
                                className="primary"
                                disabled={busy}
                                onClick={() => void saveEditDoc()}
                              >
                                Save
                              </button>
                              <button
                                className="ghost"
                                disabled={busy}
                                onClick={() => setEditingDoc(null)}
                              >
                                Cancel
                              </button>
                            </div>
                          </div>
                        );
                      }
                      return (
                        <div className="doc-row" key={i}>
                          <div className="doc-actions">
                            {hasId && (
                              <button
                                className="icon-btn"
                                title="Edit document"
                                disabled={busy}
                                onClick={() => startEditDoc(i)}
                              >
                                ✎
                              </button>
                            )}
                            <button
                              className="icon-btn"
                              title="Copy document (shell syntax)"
                              onClick={() => void copyDoc(i, "shell")}
                            >
                              {copiedDoc === `${i}-shell` ? "✓" : "⧉"}
                            </button>
                            <button
                              className="icon-btn"
                              title="Copy raw JSON"
                              onClick={() => void copyDoc(i, "json")}
                            >
                              {copiedDoc === `${i}-json` ? "✓" : "{ }"}
                            </button>
                            {hasId && (
                              <button
                                className="icon-btn danger"
                                title="Delete document"
                                disabled={busy}
                                onClick={() => setConfirmDeleteDoc(i)}
                              >
                                🗑
                              </button>
                            )}
                          </div>
                          <CodeEditorV2
                            className="doc-card"
                            language="javascript"
                            value={formatDoc(d)}
                            readOnly
                            lineWrapping
                          />
                        </div>
                      );
                    })}
                  </div>
                )}
              </>
            )}

            {tab === "aggregation" && (
              <>
                <div className="agg-stages">
                  {pipeline.map((stage, i) => (
                    <div key={i} className={"agg-stage" + (stage.enabled ? "" : " disabled")}>
                      <div className="agg-stage-head">
                        <span className="agg-stage-num">{i + 1}</span>
                        <select value={stage.op} onChange={(e) => changeStageOp(i, e.target.value)}>
                          {STAGE_OPS.map((op) => (
                            <option key={op} value={op}>
                              {op}
                            </option>
                          ))}
                        </select>
                        <button
                          className="agg-stage-btn"
                          title={stage.enabled ? "Disable stage" : "Enable stage"}
                          onClick={() => updateStage(i, { enabled: !stage.enabled })}
                        >
                          {stage.enabled ? "⊘" : "○"}
                        </button>
                        <button
                          className="agg-stage-btn"
                          title="Remove stage"
                          onClick={() => removeStage(i)}
                        >
                          ✕
                        </button>
                      </div>
                      <div className="agg-stage-editor">
                        <CodeEditorV2
                          className="code"
                          language="javascript"
                          value={stage.body}
                          onChange={(v) => updateStage(i, { body: v })}
                          placeholder="{ }"
                          lineWrapping
                          keybindings={[{ key: "Mod-Enter", run: () => void runAggregate() }]}
                        />
                      </div>
                    </div>
                  ))}
                </div>
                <div className="editor-toolbar">
                  <button className="ghost" onClick={addStage}>
                    + Add stage
                  </button>
                  <label className="row">
                    Limit{" "}
                    <input
                      type="number"
                      min={1}
                      value={limit}
                      style={{ width: 80 }}
                      onChange={(e) => setLimit(Number(e.target.value) || 1)}
                    />
                  </label>
                  <button className="primary" disabled={busy} onClick={() => runAggregate()}>
                    Run pipeline
                  </button>
                  <button
                    className="ghost"
                    title="Copy as a runnable mongosh script"
                    onClick={() => void copyQuery()}
                  >
                    {copied ? "Copied" : "Copy query"}
                  </button>
                  {aggResult && (
                    <span className="status-line">
                      {aggResult.documents.length} doc(s) · {aggResult.elapsed_ms} ms
                    </span>
                  )}
                </div>
                {error && <div className="status-line error">{error}</div>}
                {aggResult && (
                  <div className="result-scroll">
                    {aggResult.documents.map((d, i) => (
                      <CodeEditorV2
                        key={i}
                        className="doc-card"
                        language="javascript"
                        value={formatDoc(d)}
                        readOnly
                        lineWrapping
                      />
                    ))}
                  </div>
                )}
              </>
            )}

            {tab === "indexes" && (
              <>
                <div className="editor-toolbar">
                  <button className="ghost" disabled={busy} onClick={() => loadIndexes()}>
                    ↻ Refresh
                  </button>
                  {indexes && <span className="status-line">{indexes.length} index(es)</span>}
                </div>
                {error && <div className="status-line error">{error}</div>}
                {indexes && (
                  <div className="result-scroll">
                    <table className="grid">
                      <thead>
                        <tr>
                          <th>Name</th>
                          <th>Keys</th>
                          <th>Properties</th>
                        </tr>
                      </thead>
                      <tbody>
                        {indexes.map((raw, i) => {
                          const idx = describeIndex(raw);
                          return (
                            <tr key={i}>
                              <td>{idx.name}</td>
                              <td>{idx.keys}</td>
                              <td>{idx.props}</td>
                            </tr>
                          );
                        })}
                        {indexes.length === 0 && (
                          <tr className="empty-row">
                            <td colSpan={3}>No indexes.</td>
                          </tr>
                        )}
                      </tbody>
                    </table>
                  </div>
                )}
              </>
            )}
          </>
        )}
      </div>
      {confirmDeleteScript && (
        <ConfirmDialog
          title="Delete script"
          message={
            <>
              Delete{" "}
              <strong>
                {confirmDeleteScript}.{SCRIPT_EXT}
              </strong>
              ? This can't be undone.
            </>
          }
          onCancel={() => setConfirmDeleteScript(null)}
          onConfirm={() => void removeScriptFile(confirmDeleteScript)}
        />
      )}
      {confirmDeleteDoc !== null && (
        <ConfirmDialog
          title="Delete document"
          message={
            <>
              Delete this document from <strong>{active}</strong>? This can't be undone.
            </>
          }
          onCancel={() => setConfirmDeleteDoc(null)}
          onConfirm={() => void deleteDoc(confirmDeleteDoc)}
        />
      )}
    </div>
  );
}

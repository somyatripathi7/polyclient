import type { WorkspaceFile } from "../../api/store.ts";

interface WorkspaceFileListProps {
  files: WorkspaceFile[];
  activeFile: string | null;
  /** The in-progress new-file name, or null when the add input is hidden. */
  newName: string | null;
  onToggleAdd: () => void;
  onNewNameChange: (v: string) => void;
  onSave: () => void;
  onCancelAdd: () => void;
  onLoad: (file: WorkspaceFile) => void;
  onRequestDelete: (name: string) => void;
  /** Optional per-file run action; when set, each row shows a ▶ button. */
  onRun?: (file: WorkspaceFile) => void;
  /** Section header label (e.g. "SQL files", "Scripts"). */
  label: string;
  /** File extension shown after each name (e.g. "sql", "sh"). */
  ext: string;
  /** Title for the add (＋) button. */
  addTitle?: string;
  /** Text shown when there are no files. */
  emptyText?: string;
}

/** A saved-workspace-file list with an inline "new file" name input, plus
 * optional per-file load/run/delete actions. Shared by the RDBMS (SQL files)
 * and SSH/CLI (shell scripts) workspaces. */
export function WorkspaceFileList({
  files,
  activeFile,
  newName,
  onToggleAdd,
  onNewNameChange,
  onSave,
  onCancelAdd,
  onLoad,
  onRequestDelete,
  onRun,
  label,
  ext,
  addTitle,
  emptyText,
}: WorkspaceFileListProps) {
  return (
    <div className="tree-queries">
      <div className="tree-section-head">
        <span className="field-label">{label}</span>
        <button
          className="tree-add"
          title={addTitle ?? `Save current ${label} as a file`}
          onClick={onToggleAdd}
        >
          ＋
        </button>
      </div>
      {newName !== null && (
        <input
          className="tree-name-input"
          autoFocus
          placeholder="file name…"
          value={newName}
          onChange={(e) => onNewNameChange(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") onSave();
            else if (e.key === "Escape") onCancelAdd();
          }}
          onBlur={onSave}
        />
      )}
      <div className="tree-queries-list">
        {files.length === 0 ? (
          <p className="muted">{emptyText ?? "No files."}</p>
        ) : (
          files.map((f) => (
            <div
              key={f.name}
              className={"tree-node leaf" + (activeFile === f.name ? " active" : "")}
              onClick={() => onLoad(f)}
              title="Load into editor"
            >
              <span className="conn-name">
                {f.name}.{ext}
              </span>
              {onRun && (
                <button
                  className="icon-btn"
                  title="Run in terminal"
                  onClick={(e) => {
                    e.stopPropagation();
                    onRun(f);
                  }}
                >
                  ▶
                </button>
              )}
              <button
                className="close-x"
                title="Delete file"
                onClick={(e) => {
                  e.stopPropagation();
                  onRequestDelete(f.name);
                }}
              >
                ×
              </button>
            </div>
          ))
        )}
      </div>
    </div>
  );
}

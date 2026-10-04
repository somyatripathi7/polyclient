import { Modal } from "../../Modal";
import { ContentEditor } from "../../ContentEditor";

interface CellEditorModalProps {
  columnName: string;
  /** Whether to edit as JSON (syntax-highlighted, with Validate/Format). */
  json: boolean;
  draft: string;
  message: { kind: "error" | "ok"; text: string } | null;
  onDraftChange: (v: string) => void;
  onApply: () => void;
  onSetNull: () => void;
  onCancel: () => void;
}

/** Modal editor for large cell values (text/json/xml), opened on double-click.
 * JSON columns get the syntax-highlighting {@link ContentEditor} (its own
 * Validate/Format actions, quote-normalizing); other types fall through to its
 * plain-text mode. ⌘/Ctrl+Enter applies, Esc cancels. */
export function CellEditorModal({
  columnName,
  json,
  draft,
  message,
  onDraftChange,
  onApply,
  onSetNull,
  onCancel,
}: CellEditorModalProps) {
  // Esc cancels, ⌘/Ctrl+Enter applies — shared by both editor variants.
  const keybindings = [
    { key: "Escape", run: onCancel },
    { key: "Mod-Enter", run: onApply },
  ];

  return (
    <Modal
      className="cell-editor-modal"
      onClose={onCancel}
      title={
        <>
          Edit <code>{columnName}</code>
          {json && <span className="channel-tag">json</span>}
        </>
      }
    >
      <ContentEditor
        className="cell-editor"
        autoFocus
        contentType={json ? "application/json" : "text/plain"}
        value={draft}
        onChange={onDraftChange}
        keybindings={keybindings}
      />
      {message && <div className={"msg " + message.kind}>{message.text}</div>}
      <div className="form-actions cell-editor-actions">
        <span className="spacer" />
        <button onClick={onSetNull} title="Set NULL">
          Set NULL
        </button>
        <button onClick={onCancel}>Cancel</button>
        <button className="primary" onClick={onApply}>
          Apply
        </button>
      </div>
    </Modal>
  );
}

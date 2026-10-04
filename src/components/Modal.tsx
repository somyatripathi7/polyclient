import type { ReactNode } from "react";

interface ModalProps {
  /** Header content. A `ReactNode` so callers can pass tags/badges alongside text. */
  title: ReactNode;
  /** Called on backdrop click and on the × button. */
  onClose: () => void;
  /** Extra class on the `.modal` box (e.g. a width modifier). */
  className?: string;
  children: ReactNode;
}

/** Centered modal dialog: a click-to-dismiss backdrop wrapping a `.modal` box
 * with a header (title + close button). Body is `children`. Shared by every
 * dialog in the app so the markup/behavior stays in one place. */
export function Modal({ title, onClose, className, children }: ModalProps) {
  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div
        className={"modal" + (className ? " " + className : "")}
        onClick={(e) => e.stopPropagation()}
      >
        <header className="modal-header">
          <span>{title}</span>
          <button className="close-x" onClick={onClose} title="Close">
            ×
          </button>
        </header>
        {children}
      </div>
    </div>
  );
}

interface ConfirmDialogProps {
  title: ReactNode;
  /** The body prompt. */
  message: ReactNode;
  /** Label for the confirm button (defaults to "Delete"). */
  confirmLabel?: string;
  /** Style the confirm button as destructive (default true) vs. primary. */
  danger?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}

/** A yes/no confirmation built on {@link Modal}: a message plus Cancel and a
 * confirm button. Used for destructive actions (delete connection / file). */
export function ConfirmDialog({
  title,
  message,
  confirmLabel = "Delete",
  danger = true,
  onConfirm,
  onCancel,
}: ConfirmDialogProps) {
  return (
    <Modal title={title} onClose={onCancel}>
      <p>{message}</p>
      <div className="form-actions">
        <button onClick={onCancel}>Cancel</button>
        <button className={danger ? "danger" : "primary"} onClick={onConfirm}>
          {confirmLabel}
        </button>
      </div>
    </Modal>
  );
}

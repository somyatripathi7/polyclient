import { useRef } from "react";
import { type KvRow, newKvRow } from "../api/curlui.ts";
import { VarHighlightInput } from "./VarHighlightField.tsx";

// Monotonic counter for per-row React keys (see `keysRef` below). Only needs to
// be unique within a mounted editor and stable across its renders.
let keyCounter = 0;
const genKey = () => "kv" + keyCounter++;

/** Editable key/value table shared by params, headers, and form body.
 *  Keeps a trailing blank row so typing into it appends a new one.
 *
 *  In `disabled` mode the table is read-only: rows render greyed and locked
 *  (no editing, toggling, or deleting, and no trailing blank is appended). This
 *  is used to display the auto-generated headers — a row with `enabled: false`
 *  is shown struck-through to signal it's been overridden. */
export function KvEditor({
  rows,
  onChange,
  keyPlaceholder = "Key",
  valuePlaceholder = "Value",
  disabled = false,
  disabledTitle = "Added automatically — add a header with the same name to override",
  overriddenTitle = "Overridden by a header you added below",
  env,
}: {
  rows: KvRow[];
  onChange: (rows: KvRow[]) => void;
  keyPlaceholder?: string;
  valuePlaceholder?: string;
  disabled?: boolean;
  disabledTitle?: string;
  overriddenTitle?: string;
  /** When set (and the row isn't `disabled`), key/value inputs highlight
   *  `{{NAME}}` placeholders, colored by whether `NAME` resolves here. Omit
   *  for variable-*definition* tables (env/collection-overrides editors),
   *  where highlighting would imply resolution support that doesn't exist. */
  env?: Record<string, string>;
}) {
  // Stable, generated React key per row *position*. The rows handed in may
  // carry freshly-generated `r.id`s on every render (some callers rebuild them
  // from a map via headersToRows), which would remount each <input> and steal
  // focus mid-edit. Keying by a position-stable generated id instead keeps the
  // inputs mounted across renders. Extend on growth, trim on shrink.
  const keysRef = useRef<string[]>([]);
  while (keysRef.current.length < rows.length) keysRef.current.push(genKey());
  if (keysRef.current.length > rows.length) {
    keysRef.current = keysRef.current.slice(0, rows.length);
  }
  const keys = keysRef.current;

  function patch(id: string, field: "key" | "value" | "enabled", value: unknown) {
    if (disabled) return;
    let next = rows.map((r) => (r.id === id ? { ...r, [field]: value } : r));
    const last = next[next.length - 1];
    if (last && (last.key.trim() || last.value.trim())) {
      next = [...next, newKvRow()];
    }
    onChange(next);
  }
  function remove(id: string) {
    if (disabled) return;
    const next = rows.filter((r) => r.id !== id);
    onChange(next.length ? next : [newKvRow()]);
  }
  return (
    <div className="curlui-kv">
      {rows.map((r, i) => {
        const isLast = i === rows.length - 1;
        // In disabled mode an unchecked row is one that's been overridden.
        const overridden = disabled && !r.enabled;
        return (
          <div
            key={keys[i]}
            className={
              "curlui-kv-row" +
              (disabled ? " curlui-kv-locked" : "") +
              (overridden ? " curlui-kv-overridden" : "")
            }
            title={
              overridden
                ? overriddenTitle || undefined
                : disabled
                  ? disabledTitle || undefined
                  : undefined
            }
          >
            <input
              type="checkbox"
              className="curlui-kv-check"
              checked={r.enabled}
              disabled={disabled || (isLast && !r.key.trim() && !r.value.trim())}
              onChange={(e) => patch(r.id, "enabled", e.target.checked)}
            />
            {env && !disabled ? (
              <>
                <VarHighlightInput
                  className="curlui-kv-key"
                  env={env}
                  placeholder={keyPlaceholder}
                  value={r.key}
                  onChange={(v) => patch(r.id, "key", v)}
                />
                <VarHighlightInput
                  className="curlui-kv-value"
                  env={env}
                  placeholder={valuePlaceholder}
                  value={r.value}
                  onChange={(v) => patch(r.id, "value", v)}
                />
              </>
            ) : (
              <>
                <input
                  type="text"
                  className="curlui-kv-key"
                  placeholder={keyPlaceholder}
                  value={r.key}
                  readOnly={disabled}
                  onChange={(e) => patch(r.id, "key", e.target.value)}
                />
                <input
                  type="text"
                  className="curlui-kv-value"
                  placeholder={valuePlaceholder}
                  value={r.value}
                  readOnly={disabled}
                  onChange={(e) => patch(r.id, "value", e.target.value)}
                />
              </>
            )}
            <button
              type="button"
              className="curlui-kv-del"
              title="Remove"
              tabIndex={-1}
              onClick={() => remove(r.id)}
              style={{
                visibility: disabled || isLast ? "hidden" : "visible",
              }}
            >
              ✕
            </button>
          </div>
        );
      })}
    </div>
  );
}

import { useRef } from "react";
import { open as openFileDialog } from "@tauri-apps/plugin-dialog";
import type { MultipartPart } from "../api/curlui.ts";
import { VarHighlightInput } from "./VarHighlightField.tsx";

// Monotonic counter for per-row React keys (mirrors KvEditor). Only needs to be
// unique within a mounted editor and stable across its renders.
let keyCounter = 0;
const genKey = () => "mp" + keyCounter++;

/** Last path segment of a POSIX/Windows path, for the default display name. */
function basename(path: string): string {
  const seg = path.split(/[/\\]/).pop();
  return seg && seg.length ? seg : path;
}

/** Editable multipart/form-data field table. Each row is a text value or a file
 *  picked from disk (the chosen path is sent to the plugin, which reads the
 *  bytes). Keeps a trailing blank row so adding a field appends a new one. */
export function MultipartEditor({
  parts,
  onChange,
  env,
}: {
  parts: MultipartPart[];
  onChange: (parts: MultipartPart[]) => void;
  /** When set, the text-value field highlights `{{NAME}}` placeholders. */
  env?: Record<string, string>;
}) {
  // The blank trailing row is presentation-only; it isn't part of the stored
  // model until the user types into it.
  const rows: MultipartPart[] = [...parts, { name: "", kind: "text", value: "" }];

  // Position-stable React keys keep inputs mounted across renders (see KvEditor).
  const keysRef = useRef<string[]>([]);
  while (keysRef.current.length < rows.length) keysRef.current.push(genKey());
  if (keysRef.current.length > rows.length) {
    keysRef.current = keysRef.current.slice(0, rows.length);
  }
  const keys = keysRef.current;

  // Commit the edited row set: drop fully-blank rows so the trailing blank never
  // persists, then hand the model back to the parent.
  function commit(next: MultipartPart[]) {
    onChange(next.filter((p) => p.name.trim() || p.value.trim() || p.filename || p.content_type));
  }

  function patch(i: number, changes: Partial<MultipartPart>) {
    commit(rows.map((r, idx) => (idx === i ? { ...r, ...changes } : r)));
  }
  function remove(i: number) {
    commit(rows.filter((_, idx) => idx !== i));
  }

  async function browse(i: number) {
    const selected = await openFileDialog({ multiple: false, directory: false });
    if (typeof selected === "string") {
      patch(i, { value: selected });
    }
  }

  return (
    <div className="curlui-kv curlui-mp">
      {rows.map((r, i) => {
        const isLast = i === rows.length - 1;
        const isFile = r.kind === "file";
        return (
          <div key={keys[i]} className="curlui-kv-row">
            <input
              type="text"
              className="curlui-kv-key"
              placeholder="Field name"
              value={r.name}
              onChange={(e) => patch(i, { name: e.target.value })}
            />
            <select
              className="curlui-mp-kind"
              value={r.kind}
              onChange={(e) =>
                // Reset the value when switching type so a stale path/text
                // doesn't leak across kinds.
                patch(i, {
                  kind: e.target.value as MultipartPart["kind"],
                  value: "",
                })
              }
            >
              <option value="text">Text</option>
              <option value="file">File</option>
            </select>
            {isFile ? (
              <div className="curlui-mp-file">
                <span className="curlui-mp-path" title={r.value || undefined}>
                  {r.value ? basename(r.value) : "No file selected"}
                </span>
                <button type="button" onClick={() => browse(i)}>
                  Browse…
                </button>
              </div>
            ) : env ? (
              <VarHighlightInput
                className="curlui-kv-value"
                env={env}
                placeholder="Value"
                value={r.value}
                onChange={(v) => patch(i, { value: v })}
              />
            ) : (
              <input
                type="text"
                className="curlui-kv-value"
                placeholder="Value"
                value={r.value}
                onChange={(e) => patch(i, { value: e.target.value })}
              />
            )}
            <button
              type="button"
              className="curlui-kv-del"
              title="Remove"
              tabIndex={-1}
              onClick={() => remove(i)}
              style={{ visibility: isLast ? "hidden" : "visible" }}
            >
              ✕
            </button>
          </div>
        );
      })}
    </div>
  );
}

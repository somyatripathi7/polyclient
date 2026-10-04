import { useEffect, useRef, useState } from "react";
import type { BrowseOp } from "../../../api/rdbms.ts";

/** A staged per-column filter (the column is the key in the workspace's filter
 * map). `value` is the raw input text, turned into a `BrowseFilter` at query
 * time. */
export interface FilterRow {
  op: BrowseOp;
  value: string;
}

/** Browse-filter operators with their labels and whether they take a value. */
// eslint-disable-next-line react-refresh/only-export-components
export const BROWSE_OPS: { op: BrowseOp; label: string; needsValue: boolean }[] = [
  { op: "eq", label: "= equals", needsValue: true },
  { op: "ne", label: "≠ not equal", needsValue: true },
  { op: "lt", label: "< less than", needsValue: true },
  { op: "lte", label: "≤ at most", needsValue: true },
  { op: "gt", label: "> greater than", needsValue: true },
  { op: "gte", label: "≥ at least", needsValue: true },
  { op: "like", label: "LIKE", needsValue: true },
  { op: "ilike", label: "ILIKE (case-insensitive)", needsValue: true },
  { op: "is_null", label: "IS NULL", needsValue: false },
  { op: "is_not_null", label: "IS NOT NULL", needsValue: false },
];

// eslint-disable-next-line react-refresh/only-export-components
export function opNeedsValue(op: BrowseOp): boolean {
  return BROWSE_OPS.find((o) => o.op === op)?.needsValue ?? true;
}

/** Sentinel `<select>` value that reveals the raw-WHERE (advanced) field
 * instead of a per-column operator. */
const RAW_OPT = "__raw__";

/** A small funnel glyph for the column-filter button. Inline SVG so it renders
 * identically across platforms (unlike a funnel emoji). `currentColor` lets the
 * button's text color drive it (muted normally, accent when a filter is on). */
export function FunnelIcon() {
  return (
    <svg className="funnel-icon" width="11" height="11" viewBox="0 0 16 16" aria-hidden="true">
      <path
        d="M1.5 2.5h13l-5 6v5l-3 1.5v-6.5z"
        fill="currentColor"
        stroke="currentColor"
        strokeWidth="1"
        strokeLinejoin="round"
      />
    </svg>
  );
}

interface FilterPopoverProps {
  column: string;
  /** The current filter on this column, if any. */
  filter: FilterRow | undefined;
  /** The shared raw-WHERE clause (global; editable from any column's popover). */
  where: string;
  disabled: boolean;
  onChange: (patch: Partial<FilterRow>) => void;
  onWhereChange: (next: string) => void;
  onApply: () => void;
  onClear: () => void;
  onClose: () => void;
}

/** Per-column filter editor, anchored under a column header's funnel. Holds the
 * operator + value for that column, plus the shared raw-WHERE clause. Enter
 * applies, Esc closes; an outside click closes it. */
export function FilterPopover({
  column,
  filter,
  where,
  disabled,
  onChange,
  onWhereChange,
  onApply,
  onClear,
  onClose,
}: FilterPopoverProps) {
  const ref = useRef<HTMLDivElement>(null);
  const op = filter?.op ?? "eq";
  const value = filter?.value ?? "";
  const needsValue = opNeedsValue(op);
  // Raw WHERE is hidden by default; revealed by picking "Raw WHERE…" in the
  // operator dropdown. Start shown if a clause is already set, so it's visible.
  const [showRaw, setShowRaw] = useState(() => where.trim() !== "");

  // Close when clicking outside the popover.
  useEffect(() => {
    function onDown(e: MouseEvent) {
      if (ref.current && !ref.current.contains(e.target as Node)) onClose();
    }
    document.addEventListener("mousedown", onDown);
    return () => document.removeEventListener("mousedown", onDown);
  }, [onClose]);

  return (
    <div
      className="filter-popover"
      ref={ref}
      // Headers carry their own click handlers (sort); keep them out of here.
      onClick={(e) => e.stopPropagation()}
    >
      <div className="filter-popover-title">{column}</div>
      <div className="filter-popover-row">
        <select
          value={showRaw ? RAW_OPT : op}
          disabled={disabled}
          onChange={(e) => {
            if (e.target.value === RAW_OPT) {
              setShowRaw(true);
            } else {
              setShowRaw(false);
              onChange({ op: e.target.value as BrowseOp });
            }
          }}
        >
          {BROWSE_OPS.map((o) => (
            <option key={o.op} value={o.op}>
              {o.label}
            </option>
          ))}
          <option value={RAW_OPT}>Raw WHERE…</option>
        </select>
        {!showRaw && needsValue && (
          <input
            className="filter-value"
            autoFocus
            value={value}
            placeholder="value"
            disabled={disabled}
            onChange={(e) => onChange({ value: e.target.value })}
            onKeyDown={(e) => {
              if (e.key === "Enter") onApply();
              else if (e.key === "Escape") onClose();
            }}
          />
        )}
      </div>
      {showRaw && (
        <>
          <label className="filter-popover-label">
            Raw WHERE (advanced, applies to the whole query)
          </label>
          <textarea
            className="filter-popover-where"
            rows={2}
            autoFocus
            spellCheck={false}
            placeholder="e.g. status = 'active' AND age > 30"
            value={where}
            disabled={disabled}
            onChange={(e) => onWhereChange(e.target.value)}
            onKeyDown={(e) => {
              if ((e.metaKey || e.ctrlKey) && e.key === "Enter") onApply();
              else if (e.key === "Escape") onClose();
            }}
          />
        </>
      )}
      <div className="filter-popover-actions">
        <button className="filter-clear" disabled={disabled} onClick={onClear}>
          Clear
        </button>
        <span className="spacer" />
        <button disabled={disabled} onClick={onClose}>
          Close
        </button>
        <button className="primary" disabled={disabled} onClick={onApply}>
          Apply
        </button>
      </div>
    </div>
  );
}

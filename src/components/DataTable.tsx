import { useMemo, useState } from "react";

export interface Col<T> {
  key: string;
  label: string;
  /** Cell renderer. */
  render: (row: T) => React.ReactNode;
  /** Value used for sorting; defaults to no sort on this column. */
  sortVal?: (row: T) => string | number;
  align?: "right";
}

/** A sortable, optionally row-clickable read-only table. Generic over the row
 * type; columns declare their own renderers and sort keys. Used by the
 * message-broker workspaces (queues, exchanges, connections, …). */
export function DataTable<T>({
  cols,
  rows,
  rowKey,
  onRowClick,
  selectedKey,
  empty,
}: {
  cols: Col<T>[];
  rows: T[];
  rowKey: (row: T) => string;
  onRowClick?: (row: T) => void;
  selectedKey?: string | null;
  empty: string;
}) {
  const [sortKey, setSortKey] = useState<string | null>(null);
  const [desc, setDesc] = useState(false);

  const sorted = useMemo(() => {
    const col = cols.find((c) => c.key === sortKey);
    if (!col?.sortVal) return rows;
    const out = [...rows].sort((a, b) => {
      const av = col.sortVal!(a);
      const bv = col.sortVal!(b);
      if (av < bv) return -1;
      if (av > bv) return 1;
      return 0;
    });
    return desc ? out.reverse() : out;
  }, [rows, cols, sortKey, desc]);

  function clickHeader(c: Col<T>) {
    if (!c.sortVal) return;
    if (sortKey === c.key) setDesc((d) => !d);
    else {
      setSortKey(c.key);
      setDesc(false);
    }
  }

  if (rows.length === 0) return <div className="placeholder">{empty}</div>;

  return (
    <div className="result-scroll">
      <table className="grid">
        <thead>
          <tr>
            {cols.map((c) => (
              <th
                key={c.key}
                onClick={() => clickHeader(c)}
                style={{
                  cursor: c.sortVal ? "pointer" : "default",
                  textAlign: c.align === "right" ? "right" : "left",
                }}
              >
                {c.label}
                {sortKey === c.key ? (desc ? " ▾" : " ▴") : ""}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {sorted.map((row) => {
            const k = rowKey(row);
            return (
              <tr
                key={k}
                onClick={() => onRowClick?.(row)}
                className={selectedKey === k ? "mq-row-sel" : ""}
                style={{ cursor: onRowClick ? "pointer" : "default" }}
              >
                {cols.map((c) => (
                  <td key={c.key} style={{ textAlign: c.align === "right" ? "right" : "left" }}>
                    {c.render(row)}
                  </td>
                ))}
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

/** A single labelled metric tile (used on the broker overview). */
export function Stat({ label, value }: { label: string; value: React.ReactNode }) {
  return (
    <div className="mq-stat">
      <div className="mq-stat-val">{value}</div>
      <div className="mq-stat-label">{label}</div>
    </div>
  );
}

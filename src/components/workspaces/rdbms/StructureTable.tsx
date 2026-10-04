import type { ReactNode } from "react";

/** One table cell. A bare string/number renders as a single chip; an array
 * renders as one chip per item; null/empty renders nothing. */
export type StructureCell = ReactNode | string[];

interface StructureRow {
  key: string;
  cells: StructureCell[];
}

interface StructureTableProps {
  headers: string[];
  rows: StructureRow[];
  /** Message shown as a single centered row when there are no rows. */
  empty?: string;
}

function Cell({ value }: { value: StructureCell }) {
  if (value === null || value === undefined || value === "") return null;
  if (Array.isArray(value)) {
    return (
      <>
        {value.map((v, i) => (
          <span key={i} className="chip">
            {v}
          </span>
        ))}
      </>
    );
  }
  if (typeof value === "string" || typeof value === "number") {
    return <span className="chip">{value}</span>;
  }
  // Pre-composed node (e.g. an icon + text): render as-is.
  return <>{value}</>;
}

/** A roomy, read-only table for the structure view: wider row spacing and each
 * cell value shown as a pill/chip. */
export function StructureTable({ headers, rows, empty }: StructureTableProps) {
  return (
    <table className="grid structure-table">
      <thead>
        <tr>
          {headers.map((h) => (
            <th key={h}>{h}</th>
          ))}
        </tr>
      </thead>
      <tbody>
        {rows.map((r) => (
          <tr key={r.key}>
            {r.cells.map((c, i) => (
              <td key={i}>
                <Cell value={c} />
              </td>
            ))}
          </tr>
        ))}
        {empty && rows.length === 0 && (
          <tr className="empty-row">
            <td colSpan={headers.length}>{empty}</td>
          </tr>
        )}
      </tbody>
    </table>
  );
}

import type { Column } from "../../../api/rdbms.ts";

export function fmt(v: unknown): string {
  if (v === null || v === undefined) return "NULL";
  if (typeof v === "object") return JSON.stringify(v);
  return String(v);
}

/** A column's SQL type for display, including a declared length/precision when
 * applicable (e.g. `varchar(255)`, `numeric(10,2)`). Enums report
 * `USER-DEFINED`; their real type name lives in `udt_name`. */
export function displayType(c: Column): string {
  const base = c.data_type === "USER-DEFINED" && c.udt_name ? c.udt_name : c.data_type;
  // Character types: varchar(n) / char(n).
  if (c.char_max_length != null) return `${base}(${c.char_max_length})`;
  // numeric/decimal: numeric(p) or numeric(p,s).
  const t = (c.udt_name ?? c.data_type).toLowerCase();
  if ((t === "numeric" || t === "decimal") && c.numeric_precision != null) {
    return c.numeric_scale
      ? `${base}(${c.numeric_precision},${c.numeric_scale})`
      : `${base}(${c.numeric_precision})`;
  }
  return base;
}

/** Text shown in the inline editor for a cell value. */
export function fmtEditable(v: unknown): string {
  if (v === null || v === undefined) return "";
  if (typeof v === "object") return JSON.stringify(v);
  return String(v);
}

/** Like {@link fmtEditable}, but pretty-prints JSON with 2-space indentation
 * for the modal editor so it doesn't open as one long line. Accepts an
 * already-parsed object or a compact JSON string; anything that doesn't parse
 * as JSON falls back to its plain editable form unchanged. */
export function fmtEditableJson(v: unknown): string {
  if (v === null || v === undefined) return "";
  if (typeof v === "object") return JSON.stringify(v, null, 2);
  if (typeof v === "string") {
    try {
      return JSON.stringify(JSON.parse(v), null, 2);
    } catch {
      return v;
    }
  }
  return String(v);
}

/** SQL type to CAST a bound value to. Enums report `USER-DEFINED` for
 * `data_type`; their real type name lives in `udt_name`. */
export function castType(c: Column): string {
  if (c.data_type === "USER-DEFINED" && c.udt_name) return c.udt_name;
  return c.data_type;
}

/** Lower-cased SQL type name, for type-family checks. */
export function typeName(c: Column): string {
  return (castType(c) || c.data_type || "").toLowerCase();
}

export function isJsonType(c: Column): boolean {
  return !!c.json;
}

/** Long-text-ish columns edit in a modal rather than a one-line input. The
 * owning plugin classifies this (via `Column.large`) so the UI is dialect-free. */
export function isLargeType(c: Column): boolean {
  return !!c.large;
}

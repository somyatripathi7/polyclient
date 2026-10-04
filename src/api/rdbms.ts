import { ConnectionId, pluginCall } from "./api.ts";

// --- RDBMS ----------------------------------------------------------------

/** Accepted browse-filter operators (mirrors `BrowseFilter::OPS` in Rust).
 * `is_null`/`is_not_null` ignore the value. */
export type BrowseOp =
  "eq" | "ne" | "lt" | "lte" | "gt" | "gte" | "like" | "ilike" | "is_null" | "is_not_null";

export interface Schema {
  name: string;
}

export type TableKind = "table" | "view" | "materializedview";

export interface Table {
  schema: string;
  name: string;
  kind: TableKind;
}

export interface ForeignKey {
  table: string;
  column: string;
}

export interface Column {
  name: string;
  data_type: string;
  /** Underlying type (`udt_name`); the actual enum type for `USER-DEFINED`. */
  udt_name?: string | null;
  nullable: boolean;
  primary_key: boolean;
  /** True when a single-column UNIQUE constraint covers this column. */
  unique?: boolean;
  /** Default value expression from the catalog, if any. */
  default_value?: string | null;
  /** Foreign-key reference this column participates in, if any. */
  foreign_key?: ForeignKey | null;
  /** Declared length for character types (`varchar(n)`/`char(n)`), if any. */
  char_max_length?: number | null;
  /** Precision/scale for `numeric`/`decimal` columns, if declared. */
  numeric_precision?: number | null;
  numeric_scale?: number | null;
  /** JSON-valued column: the UI offers the JSON editor (validate/format). */
  json?: boolean;
  /** Long-text-ish column (incl. JSON): edits open in a modal, not inline.
   * The owning plugin classifies this so the UI stays dialect-agnostic. */
  large?: boolean;
}

/** A cell value plus the SQL type to CAST it to, for editing DML. */
export interface ColumnValue {
  column: string;
  type: string;
  value: unknown;
}

/** A staged update to one row: identify it by `pk`, set `changes`. */
export interface RowUpdate {
  pk: ColumnValue[];
  changes: ColumnValue[];
}

/** A batch of edits to a single table, applied atomically. */
export interface RowChanges {
  updates: RowUpdate[];
  /** Column values for each new row (omit a column to use its DB default). */
  inserts: ColumnValue[][];
  /** Key tuple of each row to delete. */
  deletes: ColumnValue[][];
}

export interface ApplyResult {
  updated: number;
  inserted: number;
  deleted: number;
}

export interface ColumnMeta {
  name: string;
  data_type: string;
}

/** An index on a table (shown in the structure view). */
export interface Index {
  name: string;
  /** Access method (`btree`, `hash`, `gin`, …). */
  method: string;
  unique: boolean;
  primary: boolean;
  columns: string[];
}

/** A table's columns and indexes, fetched in one call for the structure view. */
export interface TableDescription {
  columns: Column[];
  indexes: Index[];
}

export interface QueryResult {
  columns: ColumnMeta[];
  rows: unknown[][];
  rows_affected: number | null;
  elapsed_ms: number;
  /** True when the result was capped at the row limit, so `rows` is partial. */
  result_truncated?: boolean;
  /** The SQL that produced this result, when re-runnable (SELECT-ish/browse).
   * `null`/absent for DML whose result is a row count. Used to export. */
  sql?: string | null;
}

/** One `ORDER BY` term for browsing a table. */
export interface BrowseSort {
  column: string;
  descending: boolean;
}

/** One structured `WHERE` condition: `column <op> value`. The value is sent as
 * a bound parameter CAST to `type`, so it's never interpolated into SQL. */
export interface BrowseFilter {
  column: string;
  type: string;
  op: BrowseOp;
  value: unknown;
}

/** How to browse a table: structured filters (AND'd), sort terms, a row limit,
 * an offset for paging, and an optional raw `WHERE` fragment (advanced). */
export interface BrowseSpec {
  filters: BrowseFilter[];
  sorts: BrowseSort[];
  limit: number;
  offset: number;
  where_sql?: string | null;
}

export const rdbms_api = {
  // RDBMS
  rdbmsListSchemas: (connectionId: ConnectionId) =>
    pluginCall<Schema[]>(connectionId, "rdbms.list_schemas", {}),

  /** Databases on the same server. Rejects with `Unsupported` for backends
   *  with no notion of multiple databases (e.g. SQLite). */
  rdbmsListDatabases: (connectionId: ConnectionId) =>
    pluginCall<string[]>(connectionId, "rdbms.list_databases", {}),

  /** Switch the connection's active database in place (keeps the same id). */
  rdbmsUseDatabase: (connectionId: ConnectionId, database: string) =>
    pluginCall<void>(connectionId, "rdbms.use_database", { database }),

  rdbmsListTables: (connectionId: ConnectionId, schema: string) =>
    pluginCall<Table[]>(connectionId, "rdbms.list_tables", { schema }),

  /** A table's columns, plus its indexes when `includeIndexes` is set (the
   * structure view); column-only callers omit it to skip the extra query. */
  rdbmsDescribeTable: (
    connectionId: ConnectionId,
    schema: string,
    table: string,
    includeIndexes = false,
  ) =>
    pluginCall<TableDescription>(connectionId, "rdbms.describe_table", {
      schema,
      table,
      include_indexes: includeIndexes,
    }),

  /** Full backend-specific DDL (CREATE TABLE + indexes) for a table. */
  rdbmsDdlStatement: (connectionId: ConnectionId, schema: string, table: string) =>
    pluginCall<string>(connectionId, "rdbms.ddl_statement", { schema, table }),

  /** Run a SQL script; returns one result per statement, in order. */
  rdbmsExecute: (connectionId: ConnectionId, sql: string) =>
    pluginCall<QueryResult[]>(connectionId, "rdbms.execute", { sql }),

  /** Re-run `sql` and have the plugin write the full result set (no row cap) to
   * `path` as CSV. Returns the number of data rows written. */
  rdbmsExportCsv: (connectionId: ConnectionId, sql: string, path: string) =>
    pluginCall<number>(connectionId, "rdbms.export_csv", { sql, path }),

  /** Fetch rows of a table per a browse spec (filters, sort, limit, paging);
   * the plugin builds the dialect-correct query, binding filter values. */
  rdbmsBrowseTable: (connectionId: ConnectionId, schema: string, table: string, spec: BrowseSpec) =>
    pluginCall<QueryResult>(connectionId, "rdbms.browse_table", {
      schema,
      table,
      spec,
    }),

  rdbmsApplyChanges: (
    connectionId: ConnectionId,
    schema: string,
    table: string,
    changes: RowChanges,
  ) =>
    pluginCall<ApplyResult>(connectionId, "rdbms.apply_changes", {
      schema,
      table,
      changes,
    }),
};

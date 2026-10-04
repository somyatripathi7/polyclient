//! Shared abstractions for relational database plugins.
//!
//! Plugins (postgres, mysql, sqlite, ...) implement [`RdbmsPlugin`] in addition
//! to [`polyclient_core::Plugin`]. The host calls into the same trait regardless of
//! backend, and the frontend reuses the `rdbms` UI module for all of them.
//!
//! ## Adding a new RDBMS plugin
//!
//! 1. Make a binary crate that depends on `polyclient-core`, `polyclient-rdbms-common`, and
//!    `polyclient-plugin-runtime`.
//! 2. Define a struct that implements `Plugin` (returning `PluginKind::Rdbms`)
//!    and `RdbmsPlugin`.
//! 3. In `main`, call `polyclient_plugin_runtime::run(plugin, RdbmsDispatcher(plugin))`.
//!
//! [`dispatch_rdbms`] maps the opaque `op` strings the host forwards
//! (`"rdbms.execute"`, `"rdbms.apply_changes"`, ...) onto the trait methods.

use async_trait::async_trait;
use polyclient_core::{Connection, PluginError, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schema {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Table {
    pub schema: String,
    pub name: String,
    pub kind: TableKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TableKind {
    Table,
    View,
    MaterializedView,
}

/// Foreign-key reference on a column: the table and column it points to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForeignKey {
    pub table: String,
    pub column: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub data_type: String,
    /// Underlying type name (`information_schema.columns.udt_name`). Used as the
    /// cast target when `data_type` is not itself a usable SQL type — most
    /// notably `USER-DEFINED` (enums) where `udt_name` is the actual enum type.
    #[serde(default)]
    pub udt_name: Option<String>,
    pub nullable: bool,
    pub primary_key: bool,
    /// True when a UNIQUE constraint covers only this column.
    #[serde(default)]
    pub unique: bool,
    /// Default value expression as stored in the catalog, if any.
    #[serde(default)]
    pub default_value: Option<String>,
    /// Foreign-key reference this column participates in, if any.
    #[serde(default)]
    pub foreign_key: Option<ForeignKey>,
    /// Declared length for character types (`varchar(n)`, `char(n)`), if any.
    #[serde(default)]
    pub char_max_length: Option<i32>,
    /// Precision for `numeric`/`decimal` columns, if declared.
    #[serde(default)]
    pub numeric_precision: Option<i32>,
    /// Scale for `numeric`/`decimal` columns, if declared.
    #[serde(default)]
    pub numeric_scale: Option<i32>,
    /// True for JSON-valued columns; the UI offers a JSON editor (validate/format).
    #[serde(default)]
    pub json: bool,
    /// True for long-text-ish columns (incl. JSON); the UI edits them in a modal
    /// rather than a one-line input. The plugin classifies this so the frontend
    /// stays dialect-agnostic.
    #[serde(default)]
    pub large: bool,
}

/// A single cell's value together with the SQL type to cast it to. Used to
/// build parameterized DML for editing: the frontend sends the column name,
/// the type to `CAST` the bound parameter to, and the new (or key) value.
///
/// `value` carries the JSON representation; `Null` means SQL `NULL`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnValue {
    pub column: String,
    /// SQL type used as the `CAST` target for the bound parameter
    /// (e.g. `integer`, `text`, `jsonb`, a user-defined enum name).
    #[serde(rename = "type")]
    pub type_name: String,
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Vec<serde_json::Value>>,
    pub rows_affected: Option<u64>,
    pub elapsed_ms: u128,
    /// True when fetching stopped at the connection's `max_row_count` cap, so
    /// `rows` is a partial view of the full result set. The UI surfaces this so
    /// the user knows more rows exist than are shown.
    #[serde(default)]
    pub result_truncated: bool,
    /// The single SQL statement that produced this result, when it is a
    /// re-runnable query (SELECT-ish / browse). `None` for DML/DDL statements
    /// whose result is a row count, not a row set. Carried so the frontend can
    /// ask the plugin to re-run it and export the full result set.
    #[serde(default)]
    pub sql: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnMeta {
    pub name: String,
    pub data_type: String,
}

/// An index on a table, for the structure view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Index {
    pub name: String,
    /// Access method (`btree`, `hash`, `gin`, …).
    pub method: String,
    pub unique: bool,
    pub primary: bool,
    /// The indexed columns (or expressions), in order.
    pub columns: Vec<String>,
}

/// Columns + indexes for a table, fetched in one call for the structure view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableDescription {
    pub columns: Vec<Column>,
    pub indexes: Vec<Index>,
}

/// A staged update to one row: identify it by `pk`, set the columns in `changes`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RowUpdate {
    pub pk: Vec<ColumnValue>,
    pub changes: Vec<ColumnValue>,
}

/// A batch of edits to a single table, applied atomically.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RowChanges {
    #[serde(default)]
    pub updates: Vec<RowUpdate>,
    /// Each entry is the column values for one new row (omit a column to take
    /// its database default).
    #[serde(default)]
    pub inserts: Vec<Vec<ColumnValue>>,
    /// Each entry is the key tuple of a row to delete.
    #[serde(default)]
    pub deletes: Vec<Vec<ColumnValue>>,
}

/// Per-operation row counts from applying a [`RowChanges`] batch.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ApplyResult {
    pub updated: u64,
    pub inserted: u64,
    pub deleted: u64,
}

/// One `ORDER BY` term for a browse query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowseSort {
    pub column: String,
    /// `true` => `DESC`, `false` => `ASC`.
    pub descending: bool,
}

/// One structured `WHERE` condition for a browse query: `column <op> value`.
/// The value travels as a bound parameter `CAST`-ed to `type_name`, so it's
/// never interpolated into SQL. `op` is one of a fixed allow-list (see
/// [`BrowseFilter::OPS`]); `is_null`/`is_not_null` ignore `value`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowseFilter {
    pub column: String,
    /// SQL type used as the `CAST` target for the bound value (same role as
    /// [`ColumnValue::type_name`]).
    #[serde(rename = "type")]
    pub type_name: String,
    pub op: String,
    #[serde(default)]
    pub value: serde_json::Value,
}

impl BrowseFilter {
    /// The accepted operator tokens. Both ends (TS `<select>` and the plugin)
    /// enforce this list so no arbitrary operator text reaches SQL.
    pub const OPS: &'static [&'static str] = &[
        "eq",
        "ne",
        "lt",
        "lte",
        "gt",
        "gte",
        "like",
        "ilike",
        "is_null",
        "is_not_null",
    ];
}

/// How to browse a table: optional structured filters (AND'd), an optional raw
/// `WHERE` fragment (advanced, interpolated verbatim), sort terms, a row limit,
/// and an offset for paging. An all-default spec (`limit` aside) reproduces the
/// old `SELECT * FROM t LIMIT n` behaviour.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowseSpec {
    #[serde(default)]
    pub filters: Vec<BrowseFilter>,
    #[serde(default)]
    pub sorts: Vec<BrowseSort>,
    pub limit: u32,
    #[serde(default)]
    pub offset: u32,
    /// Raw `WHERE` fragment from the advanced box, AND'd with `filters`. Opt-in
    /// and interpolated as-is, so the frontend must treat it as trusted input.
    #[serde(default)]
    pub where_sql: Option<String>,
}

#[async_trait]
pub trait RdbmsPlugin: Send + Sync {
    async fn list_schemas(&self, conn: Arc<dyn Connection>) -> Result<Vec<Schema>>;

    /// List the databases reachable on the same server as this connection.
    /// Backends with no notion of multiple databases (e.g. SQLite) return
    /// [`PluginError::Unsupported`], in which case the UI hides the picker.
    async fn list_databases(&self, _conn: Arc<dyn Connection>) -> Result<Vec<String>> {
        Err(PluginError::Unsupported)
    }

    /// Point this connection at a different database on the same server,
    /// without changing its id. The connection keeps the same credentials and
    /// host; only the active database changes. Plugins that can't switch return
    /// [`PluginError::Unsupported`].
    async fn use_database(&self, _conn: Arc<dyn Connection>, _database: &str) -> Result<()> {
        Err(PluginError::Unsupported)
    }

    async fn list_tables(&self, conn: Arc<dyn Connection>, schema: &str) -> Result<Vec<Table>>;

    /// Describe a table's columns, and (when `include_indexes`) its indexes too.
    /// The index list is only needed for the structure view, so column-only
    /// callers pass `false` to skip the extra catalog query; a plugin with no
    /// index metadata simply returns an empty `indexes` either way.
    async fn describe_table(
        &self,
        conn: Arc<dyn Connection>,
        schema: &str,
        table: &str,
        include_indexes: bool,
    ) -> Result<TableDescription>;

    /// Return the full `CREATE TABLE` (and related `CREATE INDEX`, etc.) DDL for
    /// a table as backend-specific text. Plugins that can't produce DDL return
    /// [`PluginError::Unsupported`].
    async fn ddl_statement(
        &self,
        _conn: Arc<dyn Connection>,
        _schema: &str,
        _table: &str,
    ) -> Result<String> {
        Err(PluginError::Unsupported)
    }

    /// Fetch rows of a table for browsing, per [`BrowseSpec`]. The default
    /// builds an ANSI `SELECT * FROM "schema"."table" [WHERE ...] [ORDER BY ...]
    /// LIMIT n OFFSET m` (double-quote-escaping identifiers) and runs it via
    /// [`RdbmsPlugin::execute`]. Because the default has no way to bind
    /// parameters, it supports only the raw `where_sql` fragment, sorts, and
    /// limit/offset — structured `filters` (which carry values) require a
    /// plugin that overrides this to bind them. Plugins on dialects that quote
    /// or page differently (MySQL backticks, SQL Server `TOP`, …) also override,
    /// keeping dialect knowledge out of the frontend.
    async fn browse_table(
        &self,
        conn: Arc<dyn Connection>,
        schema: &str,
        table: &str,
        spec: BrowseSpec,
    ) -> Result<QueryResult> {
        if !spec.filters.is_empty() {
            return Err(PluginError::Unsupported);
        }
        let mut q = format!(
            "SELECT * FROM \"{}\".\"{}\"",
            schema.replace('"', "\"\""),
            table.replace('"', "\"\""),
        );
        if let Some(w) = spec
            .where_sql
            .as_deref()
            .map(str::trim)
            .filter(|w| !w.is_empty())
        {
            q.push_str(&format!(" WHERE ({w})"));
        }
        if !spec.sorts.is_empty() {
            let terms: Vec<String> = spec
                .sorts
                .iter()
                .map(|s| {
                    format!(
                        "\"{}\" {}",
                        s.column.replace('"', "\"\""),
                        if s.descending { "DESC" } else { "ASC" }
                    )
                })
                .collect();
            q.push_str(&format!(" ORDER BY {}", terms.join(", ")));
        }
        q.push_str(&format!(" LIMIT {} OFFSET {}", spec.limit, spec.offset));
        // Browsing is a single SELECT, so hand back just that one result.
        Ok(self.execute(conn, &q).await?.pop().unwrap_or(QueryResult {
            columns: Vec::new(),
            rows: Vec::new(),
            rows_affected: None,
            elapsed_ms: 0,
            result_truncated: false,
            sql: None,
        }))
    }

    /// Run a SQL script and return one [`QueryResult`] per statement, in order.
    async fn execute(&self, conn: Arc<dyn Connection>, sql: &str) -> Result<Vec<QueryResult>>;

    /// Run a single re-runnable query (`sql`) and write its **full** result set
    /// (no row cap) to `path` as CSV, returning the number of data rows written.
    /// The plugin owns the connection pool, so it streams rows straight to disk
    /// rather than shipping them across the wire. Plugins that don't support
    /// CSV export return [`PluginError::Unsupported`].
    async fn export_csv(&self, _conn: Arc<dyn Connection>, _sql: &str, _path: &str) -> Result<u64> {
        Err(PluginError::Unsupported)
    }

    /// Apply a batch of inserts/updates/deletes to one table atomically (in a
    /// single transaction). Plugins that don't support editing return
    /// [`PluginError::Unsupported`].
    async fn apply_changes(
        &self,
        _conn: Arc<dyn Connection>,
        _schema: &str,
        _table: &str,
        _changes: RowChanges,
    ) -> Result<ApplyResult> {
        Err(PluginError::Unsupported)
    }
}

/// Downcast a `Arc<dyn Connection>` to a plugin's concrete connection type.
pub fn downcast_conn<T: 'static>(conn: &Arc<dyn Connection>) -> Result<&T> {
    conn.as_any()
        .downcast_ref::<T>()
        .ok_or_else(|| PluginError::Backend("connection type mismatch".into()))
}

// ---------------------------------------------------------------------------
// CSV export (shared by RDBMS plugins implementing `export_csv`)
// ---------------------------------------------------------------------------

use std::fs::File;
use std::io::{BufWriter, Write as _};
use std::path::Path;

/// Render one JSON cell as a CSV field (unescaped): `Null` -> empty string,
/// `String` -> its text, anything else -> its compact JSON rendering (so a
/// number stays `42`, a bool `true`, an array/object its JSON text). The
/// returned string is escaped by [`CsvWriter`] as needed.
fn cell_to_csv(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Escape a CSV field per RFC 4180: wrap in double quotes (doubling any
/// internal quote) when it contains a comma, quote, or newline.
fn escape_csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// A streaming CSV writer over a buffered file. RDBMS plugins implementing
/// [`RdbmsPlugin::export_csv`] create one, write the header, then push rows as
/// they stream from the backend — keeping memory bounded on large exports.
///
/// Each row is `&[serde_json::Value]` (the same cell representation the grid
/// uses, via the plugin's value decoder), so dialects share the formatting and
/// escaping. Lines are terminated with `\r\n` (RFC 4180); cells use
/// [`cell_to_csv`] + [`escape_csv_field`].
pub struct CsvWriter {
    out: BufWriter<File>,
    cols: usize,
}

impl CsvWriter {
    /// Create the file at `path` and write the header row built from `columns`.
    /// The number of columns fixes the field count for subsequent rows.
    pub fn create(path: impl AsRef<Path>, columns: &[ColumnMeta]) -> Result<Self> {
        let file = File::create(path).map_err(|e| PluginError::Backend(e.to_string()))?;
        let mut w = CsvWriter {
            out: BufWriter::new(file),
            cols: columns.len(),
        };
        let header: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        w.write_line(header.iter().map(|s| escape_csv_field(s)))?;
        Ok(w)
    }

    /// Write one data row. Extra cells beyond the header width are ignored and
    /// missing ones are left blank, so a ragged row never corrupts the file.
    pub fn write_row(&mut self, cells: &[serde_json::Value]) -> Result<()> {
        let fields = (0..self.cols).map(|i| {
            cells
                .get(i)
                .map(|v| escape_csv_field(&cell_to_csv(v)))
                .unwrap_or_default()
        });
        self.write_line(fields)
    }

    /// Flush any buffered bytes to disk. Call once after the last row.
    pub fn finish(mut self) -> Result<()> {
        self.out
            .flush()
            .map_err(|e| PluginError::Backend(e.to_string()))
    }

    fn write_line(&mut self, fields: impl Iterator<Item = String>) -> Result<()> {
        let line = fields.collect::<Vec<_>>().join(",");
        self.out
            .write_all(line.as_bytes())
            .and_then(|_| self.out.write_all(b"\r\n"))
            .map_err(|e| PluginError::Backend(e.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Capability dispatch (sidecar plugin <- host `call`)
// ---------------------------------------------------------------------------

use polyclient_plugin_runtime::Dispatcher;
use serde_json::Value;

fn parse<T: for<'de> Deserialize<'de>>(params: Value) -> Result<T> {
    serde_json::from_value(params).map_err(|e| PluginError::Config(format!("invalid params: {e}")))
}

fn to_value<T: Serialize>(value: T) -> Result<Value> {
    serde_json::to_value(value).map_err(|e| PluginError::Backend(e.to_string()))
}

#[derive(Deserialize)]
struct ListTablesParams {
    schema: String,
}

#[derive(Deserialize)]
struct DescribeTableParams {
    schema: String,
    table: String,
    /// Whether to include the table's indexes (structure view); the column-only
    /// callers (`ddl_statement`, edit context) omit it and take the default.
    #[serde(default)]
    include_indexes: bool,
}

#[derive(Deserialize)]
struct BrowseTableParams {
    schema: String,
    table: String,
    spec: BrowseSpec,
}

#[derive(Deserialize)]
struct ExecuteParams {
    sql: String,
}

#[derive(Deserialize)]
struct ExportCsvParams {
    sql: String,
    path: String,
}

#[derive(Deserialize)]
struct UseDatabaseParams {
    database: String,
}

#[derive(Deserialize)]
struct ApplyChangesParams {
    schema: String,
    table: String,
    changes: RowChanges,
}

/// Map an `rdbms.*` op (forwarded opaquely by the host) onto an [`RdbmsPlugin`]
/// method, returning the JSON-serialized result. Unknown ops are
/// [`PluginError::Unsupported`].
pub async fn dispatch_rdbms(
    op: &str,
    params: Value,
    plugin: &dyn RdbmsPlugin,
    conn: Arc<dyn Connection>,
) -> Result<Value> {
    match op {
        "rdbms.list_schemas" => to_value(plugin.list_schemas(conn).await?),
        "rdbms.list_databases" => to_value(plugin.list_databases(conn).await?),
        "rdbms.use_database" => {
            let p: UseDatabaseParams = parse(params)?;
            plugin.use_database(conn, &p.database).await?;
            to_value(())
        }
        "rdbms.list_tables" => {
            let p: ListTablesParams = parse(params)?;
            to_value(plugin.list_tables(conn, &p.schema).await?)
        }
        "rdbms.describe_table" => {
            let p: DescribeTableParams = parse(params)?;
            to_value(
                plugin
                    .describe_table(conn, &p.schema, &p.table, p.include_indexes)
                    .await?,
            )
        }
        "rdbms.ddl_statement" => {
            let p: DescribeTableParams = parse(params)?;
            to_value(plugin.ddl_statement(conn, &p.schema, &p.table).await?)
        }
        "rdbms.browse_table" => {
            let p: BrowseTableParams = parse(params)?;
            to_value(
                plugin
                    .browse_table(conn, &p.schema, &p.table, p.spec)
                    .await?,
            )
        }
        "rdbms.execute" => {
            let p: ExecuteParams = parse(params)?;
            to_value(plugin.execute(conn, &p.sql).await?)
        }
        "rdbms.export_csv" => {
            let p: ExportCsvParams = parse(params)?;
            to_value(plugin.export_csv(conn, &p.sql, &p.path).await?)
        }
        "rdbms.apply_changes" => {
            let p: ApplyChangesParams = parse(params)?;
            to_value(
                plugin
                    .apply_changes(conn, &p.schema, &p.table, p.changes)
                    .await?,
            )
        }
        _ => Err(PluginError::Unsupported),
    }
}

/// Adapts any [`RdbmsPlugin`] into a runtime [`Dispatcher`] by routing through
/// [`dispatch_rdbms`]. A plugin binary wires it up with
/// `run(plugin, RdbmsDispatcher(plugin))`.
pub struct RdbmsDispatcher<P: RdbmsPlugin>(pub P);

#[async_trait]
impl<P: RdbmsPlugin + 'static> Dispatcher for RdbmsDispatcher<P> {
    async fn dispatch(&self, op: &str, params: Value, conn: Arc<dyn Connection>) -> Result<Value> {
        dispatch_rdbms(op, params, &self.0, conn).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dummy;

    #[async_trait]
    impl RdbmsPlugin for Dummy {
        async fn list_schemas(&self, _conn: Arc<dyn Connection>) -> Result<Vec<Schema>> {
            Ok(vec![Schema {
                name: "public".into(),
            }])
        }
        async fn list_tables(&self, _c: Arc<dyn Connection>, _s: &str) -> Result<Vec<Table>> {
            Ok(vec![])
        }
        async fn describe_table(
            &self,
            _c: Arc<dyn Connection>,
            _s: &str,
            _t: &str,
            _include_indexes: bool,
        ) -> Result<TableDescription> {
            Ok(TableDescription {
                columns: vec![],
                indexes: vec![],
            })
        }
        async fn execute(&self, _c: Arc<dyn Connection>, _sql: &str) -> Result<Vec<QueryResult>> {
            Ok(vec![QueryResult {
                columns: vec![],
                rows: vec![],
                rows_affected: Some(0),
                elapsed_ms: 0,
                result_truncated: false,
                sql: None,
            }])
        }
    }

    struct NoConn;
    impl Connection for NoConn {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[tokio::test]
    async fn maps_known_op() {
        let conn: Arc<dyn Connection> = Arc::new(NoConn);
        let v = dispatch_rdbms("rdbms.list_schemas", Value::Null, &Dummy, conn)
            .await
            .unwrap();
        assert_eq!(v, serde_json::json!([{ "name": "public" }]));
    }

    #[tokio::test]
    async fn rejects_unknown_op() {
        let conn: Arc<dyn Connection> = Arc::new(NoConn);
        let err = dispatch_rdbms("rdbms.bogus", Value::Null, &Dummy, conn)
            .await
            .unwrap_err();
        assert!(matches!(err, PluginError::Unsupported));
    }
}

use async_trait::async_trait;
use futures_util::TryStreamExt;
use polyclient_core::{
    append_options, cfg_secret, ConfigField, ConfigFieldType, Connection, ConnectionConfig, Plugin,
    PluginError, PluginInfo, PluginKind, Result,
};
use polyclient_rdbms_common::{
    downcast_conn, ApplyResult, BrowseFilter, BrowseSpec, Column, ColumnMeta, ColumnValue,
    CsvWriter, ForeignKey, Index, QueryResult, RdbmsPlugin, RowChanges, Schema, Table,
    TableDescription, TableKind,
};
use sqlx::postgres::{PgDatabaseError, PgErrorPosition, PgPool, PgPoolOptions, PgRow};
use sqlx::{AssertSqlSafe, SqlSafeStr};
use sqlx::{Column as _, Executor, Row, TypeInfo};
use std::sync::{Arc, RwLock};
use std::time::Instant;

pub struct PostgresPlugin;

impl PostgresPlugin {
    pub fn new() -> Self {
        Self
    }

    /// Index metadata for the structure view. Helper for
    /// [`describe_table`](RdbmsPlugin::describe_table) when `include_indexes` is set.
    async fn list_indexes(
        &self,
        conn: &PostgresConnection,
        schema: &str,
        table: &str,
    ) -> Result<Vec<Index>> {
        // Each index's columns come back as a text[] of column expressions in
        // key order. `indkey` is a 0-based int2vector, but pg_get_indexdef's
        // column number is 1-based (and 0 means "the whole index" → it would
        // return the full CREATE INDEX statement), so offset the subscript by 1.
        let rows: Vec<(String, String, bool, bool, Vec<String>)> = sqlx::query_as(
            "select i.relname, am.amname, ix.indisunique, ix.indisprimary, \
                    array(select pg_get_indexdef(ix.indexrelid, k.n + 1, true) \
                          from generate_subscripts(ix.indkey, 1) as k(n) \
                          order by k.n) \
             from pg_index ix \
             join pg_class i on i.oid = ix.indexrelid \
             join pg_class t on t.oid = ix.indrelid \
             join pg_namespace n on n.oid = t.relnamespace \
             join pg_am am on am.oid = i.relam \
             where n.nspname = $1 and t.relname = $2 \
             order by ix.indisprimary desc, i.relname",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&conn.pool())
        .await
        .map_err(|e| db_backend_err(e, None))?;

        Ok(rows
            .into_iter()
            .map(|(name, method, unique, primary, columns)| Index {
                name,
                method,
                unique,
                primary,
                columns,
            })
            .collect())
    }
}

impl Default for PostgresPlugin {
    fn default() -> Self {
        Self::new()
    }
}

/// A live connection to one database on a Postgres server.
///
/// The pool sits behind a lock so the active database can be switched in place
/// (Postgres binds a connection to a single database, so switching means
/// building a fresh pool) without changing the connection's id. `config` keeps
/// the original host/credentials so a new pool can be built for another
/// database on the same server.
pub struct PostgresConnection {
    pool: RwLock<PgPool>,
    config: ConnectionConfig,
}

impl PostgresConnection {
    /// A cheap clone of the current pool (sqlx pools are internally `Arc`d). The
    /// lock is released before the returned pool is awaited on, so a concurrent
    /// `use_database` swap never blocks in-flight queries.
    fn pool(&self) -> PgPool {
        self.pool.read().unwrap().clone()
    }
}

impl Connection for PostgresConnection {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn cfg_str(cfg: &ConnectionConfig, key: &str) -> Result<String> {
    cfg.get(key)
        .and_then(|v| v.as_str().map(str::to_string))
        .ok_or_else(|| PluginError::Config(format!("missing field: {key}")))
}

fn cfg_str_opt(cfg: &ConnectionConfig, key: &str) -> Option<String> {
    cfg.get(key).and_then(|v| v.as_str().map(str::to_string))
}

fn cfg_u16(cfg: &ConnectionConfig, key: &str, default: u16) -> u16 {
    cfg.get(key)
        .and_then(|v| v.as_u64())
        .map(|n| n as u16)
        .unwrap_or(default)
}

/// Cap on rows fetched per query. Bounds memory use for large result sets;
/// fetching stops at this count and the result is flagged as truncated.
const DEFAULT_MAX_ROW_COUNT: u64 = 10_000;

/// Build a `postgres://` URL from a config, pointed at `database`. The database
/// is passed explicitly (rather than read from `cfg`) so the same credentials
/// can be reused to connect to a different database when switching.
fn build_url(cfg: &ConnectionConfig, database: &str) -> Result<String> {
    let host = cfg_str(cfg, "host")?;
    let port = cfg_u16(cfg, "port", 5432);
    let user = cfg_str(cfg, "user")?;
    let password = cfg_secret(cfg, "password")?.unwrap_or_default();
    let ssl = cfg_str_opt(cfg, "ssl").unwrap_or_else(|| "prefer".into());
    Ok(format!(
        "postgres://{u}:{p}@{host}:{port}/{database}?sslmode={ssl}",
        u = urlencode(&user),
        p = urlencode(&password),
        database = urlencode(database),
    ))
}

async fn make_pool(url: &str) -> Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(8)
        .connect(url)
        .await
        .map_err(|e| PluginError::Connection(format_db_error(&e, None)))
}

/// Replace the database path segment in a `postgres://` URL with `database`.
/// Finds the path component (after the authority) and replaces it.
fn swap_database_in_url(base: &str, database: &str) -> Result<String> {
    // A postgres URL looks like: postgres://user:pass@host:port/dbname[?params]
    // Find the third '/' (end of authority) and replace everything up to '?'.
    let after_scheme = base
        .find("://")
        .map(|i| i + 3)
        .ok_or_else(|| PluginError::Config("invalid connection string: no scheme".into()))?;
    let path_start = base[after_scheme..]
        .find('/')
        .map(|i| after_scheme + i)
        .ok_or_else(|| PluginError::Config("invalid connection string: no database path".into()))?;
    let query_start = base[path_start..].find('?').map(|i| path_start + i);
    let suffix = query_start.map(|i| &base[i..]).unwrap_or("");
    Ok(format!(
        "{}/{}{suffix}",
        &base[..path_start],
        urlencode(database),
    ))
}

#[async_trait]
impl Plugin for PostgresPlugin {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            id: "postgres".into(),
            name: "PostgreSQL".into(),
            kind: PluginKind::Rdbms,
            version: env!("CARGO_PKG_VERSION").into(),
            description: "Connect to PostgreSQL 12+ databases.".into(),
            ui_module: Some("rdbms".into()),
            protocol_version: polyclient_core::PROTOCOL_VERSION,
            config_schema: vec![
                ConfigField {
                    key: "mode".into(),
                    label: "Input mode".into(),
                    field_type: ConfigFieldType::Select {
                        options: vec!["individual".into(), "url".into()],
                    },
                    required: false,
                    default: Some(serde_json::json!("individual")),
                    placeholder: None,
                    show_if: None,
                },
                ConfigField {
                    key: "connection_string".into(),
                    label: "Connection string".into(),
                    field_type: ConfigFieldType::Text,
                    required: true,
                    default: Some("postgres://user:password@localhost:5432/dbname".into()),
                    placeholder: Some("postgres://user:password@localhost:5432/dbname".into()),
                    show_if: Some(polyclient_core::ShowIf {
                        field: "mode".into(),
                        equals: "url".into(),
                    }),
                },
                ConfigField {
                    key: "host".into(),
                    label: "Host".into(),
                    field_type: ConfigFieldType::Text,
                    required: true,
                    default: Some(serde_json::json!("localhost")),
                    placeholder: Some("localhost".into()),
                    show_if: Some(polyclient_core::ShowIf {
                        field: "mode".into(),
                        equals: "individual".into(),
                    }),
                },
                ConfigField {
                    key: "port".into(),
                    label: "Port".into(),
                    field_type: ConfigFieldType::Number,
                    required: true,
                    default: Some(serde_json::json!(5432)),
                    placeholder: None,
                    show_if: Some(polyclient_core::ShowIf {
                        field: "mode".into(),
                        equals: "individual".into(),
                    }),
                },
                ConfigField {
                    key: "database".into(),
                    label: "Database".into(),
                    field_type: ConfigFieldType::Text,
                    required: true,
                    default: None,
                    placeholder: Some("postgres".into()),
                    show_if: Some(polyclient_core::ShowIf {
                        field: "mode".into(),
                        equals: "individual".into(),
                    }),
                },
                ConfigField {
                    key: "schema".into(),
                    label: "Schema".into(),
                    field_type: ConfigFieldType::Text,
                    required: false,
                    default: None,
                    placeholder: Some("public".into()),
                    // No show_if: applies to both connection modes; it only
                    // controls which schema the UI expands by default.
                    show_if: None,
                },
                ConfigField {
                    key: "user".into(),
                    label: "User".into(),
                    field_type: ConfigFieldType::Text,
                    required: true,
                    default: None,
                    placeholder: Some("postgres".into()),
                    show_if: Some(polyclient_core::ShowIf {
                        field: "mode".into(),
                        equals: "individual".into(),
                    }),
                },
                ConfigField {
                    key: "password".into(),
                    label: "Password".into(),
                    field_type: ConfigFieldType::Password,
                    required: false,
                    default: None,
                    placeholder: None,
                    show_if: Some(polyclient_core::ShowIf {
                        field: "mode".into(),
                        equals: "individual".into(),
                    }),
                },
                ConfigField {
                    key: "ssl".into(),
                    label: "SSL".into(),
                    field_type: ConfigFieldType::Select {
                        options: vec!["disable".into(), "prefer".into(), "require".into()],
                    },
                    required: false,
                    default: Some(serde_json::json!("prefer")),
                    placeholder: None,
                    show_if: Some(polyclient_core::ShowIf {
                        field: "mode".into(),
                        equals: "individual".into(),
                    }),
                },
                ConfigField {
                    key: "options".into(),
                    label: "Additional Options".into(),
                    field_type: ConfigFieldType::KeyValue,
                    required: false,
                    default: Some(serde_json::json!({})),
                    placeholder: Some("connect_timeout=10".into()),
                    show_if: None,
                },
            ],
        }
    }

    async fn connect(&self, cfg: ConnectionConfig) -> Result<Arc<dyn Connection>> {
        let url = if cfg_str_opt(&cfg, "mode").as_deref() == Some("url") {
            cfg_str(&cfg, "connection_string")?
        } else {
            let database = cfg_str(&cfg, "database")?;
            build_url(&cfg, &database)?
        };
        let url = append_options(url, &cfg);
        // Never log `url` — it carries the password. Log only safe fields.
        tracing::info!(
            "connecting to postgres host={} port={} db={}",
            cfg_str_opt(&cfg, "host").as_deref().unwrap_or("?"),
            cfg_str_opt(&cfg, "port")
                .or_else(|| cfg.get("port").map(|v| v.to_string()))
                .as_deref()
                .unwrap_or("?"),
            cfg_str_opt(&cfg, "database").as_deref().unwrap_or("?"),
        );
        let pool = make_pool(&url).await.inspect_err(|e| {
            tracing::warn!("postgres connection failed: {e}");
        })?;
        tracing::info!("postgres pool established");
        Ok(Arc::new(PostgresConnection {
            pool: RwLock::new(pool),
            config: cfg,
        }))
    }
}

#[async_trait]
impl RdbmsPlugin for PostgresPlugin {
    async fn list_schemas(&self, conn: Arc<dyn Connection>) -> Result<Vec<Schema>> {
        let conn = downcast_conn::<PostgresConnection>(&conn)?;
        let rows: Vec<(String,)> = sqlx::query_as(
            "select schema_name from information_schema.schemata \
             where schema_name not in ('pg_catalog','information_schema') \
             and schema_name not like 'pg_toast%' and schema_name not like 'pg_temp%' \
             order by schema_name",
        )
        .fetch_all(&conn.pool())
        .await
        .map_err(|e| db_backend_err(e, None))?;
        Ok(rows.into_iter().map(|(n,)| Schema { name: n }).collect())
    }

    async fn list_databases(&self, conn: Arc<dyn Connection>) -> Result<Vec<String>> {
        let conn = downcast_conn::<PostgresConnection>(&conn)?;
        let rows: Vec<(String,)> = sqlx::query_as(
            "select datname from pg_database \
             where datistemplate = false and datallowconn = true \
             order by datname",
        )
        .fetch_all(&conn.pool())
        .await
        .map_err(|e| db_backend_err(e, None))?;
        Ok(rows.into_iter().map(|(n,)| n).collect())
    }

    async fn use_database(&self, conn: Arc<dyn Connection>, database: &str) -> Result<()> {
        let conn = downcast_conn::<PostgresConnection>(&conn)?;
        tracing::info!("switching database to '{database}' (rebuilding pool)");
        // Build and validate the new pool before swapping, so a failed switch
        // leaves the existing connection untouched.
        let url = if cfg_str_opt(&conn.config, "mode").as_deref() == Some("url") {
            let base = cfg_str(&conn.config, "connection_string")?;
            swap_database_in_url(&base, database)?
        } else {
            build_url(&conn.config, database)?
        };
        let url = append_options(url, &conn.config);
        let pool = make_pool(&url).await?;
        *conn.pool.write().unwrap() = pool;
        Ok(())
    }

    async fn list_tables(&self, conn: Arc<dyn Connection>, schema: &str) -> Result<Vec<Table>> {
        let conn = downcast_conn::<PostgresConnection>(&conn)?;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "select table_name, table_type from information_schema.tables \
             where table_schema = $1 order by table_name",
        )
        .bind(schema)
        .fetch_all(&conn.pool())
        .await
        .map_err(|e| db_backend_err(e, None))?;

        Ok(rows
            .into_iter()
            .map(|(name, kind)| Table {
                schema: schema.to_string(),
                name,
                kind: match kind.as_str() {
                    "VIEW" => TableKind::View,
                    "MATERIALIZED VIEW" => TableKind::MaterializedView,
                    _ => TableKind::Table,
                },
            })
            .collect())
    }

    async fn describe_table(
        &self,
        conn: Arc<dyn Connection>,
        schema: &str,
        table: &str,
        include_indexes: bool,
    ) -> Result<TableDescription> {
        let conn = downcast_conn::<PostgresConnection>(&conn)?;
        let rows: Vec<(
            String,         // column_name
            String,         // data_type
            String,         // udt_name
            String,         // is_nullable
            Option<i32>,    // character_maximum_length
            Option<i32>,    // numeric_precision
            Option<i32>,    // numeric_scale
            Option<String>, // pk
            Option<String>, // column_default
            Option<String>, // unique (single-col UNIQUE constraint)
            Option<String>, // fk_table
            Option<String>, // fk_column
        )> = sqlx::query_as(
            "select
               c.column_name,
               c.data_type,
               c.udt_name,
               c.is_nullable,
               c.character_maximum_length,
               c.numeric_precision,
               c.numeric_scale,
               -- primary key
               (select 'YES'
                  from information_schema.table_constraints tc
                  join information_schema.key_column_usage kcu
                       using (constraint_name, table_schema)
                 where tc.constraint_type = 'PRIMARY KEY'
                   and tc.table_schema = c.table_schema
                   and tc.table_name   = c.table_name
                   and kcu.column_name = c.column_name
                 limit 1),
               c.column_default,
               -- single-column unique constraint (excludes PK-backed unique indexes)
               (select 'YES'
                  from information_schema.table_constraints tc
                  join information_schema.key_column_usage kcu
                       using (constraint_name, table_schema)
                 where tc.constraint_type = 'UNIQUE'
                   and tc.table_schema = c.table_schema
                   and tc.table_name   = c.table_name
                   and kcu.column_name = c.column_name
                   and (select count(*)
                          from information_schema.key_column_usage kcu2
                         where kcu2.constraint_name = tc.constraint_name
                           and kcu2.table_schema    = tc.table_schema) = 1
                 limit 1),
               -- foreign key: referenced table (via pg_catalog for correctness)
               (select ref_cls.relname
                  from pg_constraint con
                  join pg_class     src_cls on src_cls.oid = con.conrelid
                  join pg_namespace src_ns  on src_ns.oid  = src_cls.relnamespace
                  join pg_attribute src_att on src_att.attrelid = con.conrelid
                                           and src_att.attnum   = any(con.conkey)
                  join pg_class     ref_cls on ref_cls.oid = con.confrelid
                 where con.contype = 'f'
                   and src_ns.nspname  = c.table_schema
                   and src_cls.relname = c.table_name
                   and src_att.attname = c.column_name
                 limit 1),
               -- foreign key: referenced column
               (select ref_att.attname
                  from pg_constraint con
                  join pg_class     src_cls on src_cls.oid = con.conrelid
                  join pg_namespace src_ns  on src_ns.oid  = src_cls.relnamespace
                  join pg_attribute src_att on src_att.attrelid = con.conrelid
                                           and src_att.attnum   = any(con.conkey)
                  join pg_attribute ref_att on ref_att.attrelid = con.confrelid
                                           and ref_att.attnum   = con.confkey[
                                                 array_position(con.conkey, src_att.attnum)]
                 where con.contype = 'f'
                   and src_ns.nspname  = c.table_schema
                   and src_cls.relname = c.table_name
                   and src_att.attname = c.column_name
                 limit 1)
             from information_schema.columns c
            where c.table_schema = $1 and c.table_name = $2
            order by c.ordinal_position",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&conn.pool())
        .await
        .map_err(|e| db_backend_err(e, None))?;

        let columns = rows
            .into_iter()
            .map(
                |(
                    name,
                    dtype,
                    udt,
                    nullable,
                    char_len,
                    precision,
                    scale,
                    pk,
                    default_value,
                    unique_flag,
                    fk_table,
                    fk_col,
                )| {
                    let u = udt.to_ascii_lowercase();
                    let json = u == "json" || u == "jsonb";
                    let large = json || dtype == "text" || dtype == "xml" || u == "citext";
                    let foreign_key = fk_table
                        .zip(fk_col)
                        .map(|(table, column)| ForeignKey { table, column });
                    Column {
                        name,
                        data_type: dtype,
                        udt_name: Some(udt),
                        nullable: nullable == "YES",
                        primary_key: pk.is_some(),
                        unique: unique_flag.is_some(),
                        default_value,
                        foreign_key,
                        char_max_length: char_len,
                        numeric_precision: precision,
                        numeric_scale: scale,
                        json,
                        large,
                    }
                },
            )
            .collect();

        let indexes = if include_indexes {
            self.list_indexes(conn, schema, table).await?
        } else {
            Vec::new()
        };
        Ok(TableDescription { columns, indexes })
    }

    async fn ddl_statement(
        &self,
        conn: Arc<dyn Connection>,
        schema: &str,
        table: &str,
    ) -> Result<String> {
        let columns = self
            .describe_table(conn.clone(), schema, table, false)
            .await?
            .columns;
        if columns.is_empty() {
            return Err(PluginError::Backend(format!(
                "table {schema}.{table} not found or has no columns"
            )));
        }
        let conn = downcast_conn::<PostgresConnection>(&conn)?;

        // Column definitions, then the primary key (if any).
        let mut lines: Vec<String> = columns
            .iter()
            .map(|c| {
                let ty = match (c.data_type.as_str(), &c.udt_name) {
                    ("USER-DEFINED", Some(udt)) => udt.clone(),
                    _ => c.data_type.clone(),
                };
                let null = if c.nullable { "" } else { " NOT NULL" };
                format!("  \"{}\" {}{}", c.name, ty, null)
            })
            .collect();
        let pks: Vec<String> = columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| format!("\"{}\"", c.name))
            .collect();
        if !pks.is_empty() {
            lines.push(format!("  PRIMARY KEY ({})", pks.join(", ")));
        }
        let mut ddl = format!(
            "-- Generated DDL statements -- \n\n\n\
            -- Table: {schema}.{table} Definition -- \n\n\
            CREATE TABLE IF NOT EXISTS \"{schema}\".\"{table}\" (\n{}\n);",
            lines.join(",\n")
        );

        ddl.push_str("\n\n\n -- Indexes --");
        // Append non-primary-key indexes via pg_get_indexdef.
        let indexes: Vec<(String,)> = sqlx::query_as(
            "select pg_get_indexdef(ix.indexrelid) \
             from pg_index ix \
             join pg_class t on t.oid = ix.indrelid \
             join pg_namespace n on n.oid = t.relnamespace \
             where n.nspname = $1 and t.relname = $2 and not ix.indisprimary \
             order by ix.indexrelid::regclass::text",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&conn.pool())
        .await
        .map_err(|e| db_backend_err(e, None))?;

        for (def,) in indexes {
            ddl.push_str("\n\n");
            ddl.push_str(&def);
            ddl.push(';');
        }
        Ok(ddl)
    }

    async fn browse_table(
        &self,
        conn: Arc<dyn Connection>,
        schema: &str,
        table: &str,
        spec: BrowseSpec,
    ) -> Result<QueryResult> {
        let conn = downcast_conn::<PostgresConnection>(&conn)?;
        let pool = conn.pool();

        let (sql, params) = build_browse(schema, table, &spec)?;
        let start = Instant::now();

        // Bind the structured-filter values as text params; the surrounding
        // CASTs coerce them. Mirrors `run_dml`'s binding.
        let mut q = sqlx::query(AssertSqlSafe(sql.clone()));
        for p in &params {
            q = q.bind(p);
        }
        let rows = q
            .fetch_all(&pool)
            .await
            .map_err(|e| db_backend_err(e, Some(&sql)))?;

        // Only a parameterless browse SQL is re-runnable as-is for export;
        // structured filters bind `$n` params that `export_csv` can't supply.
        // Capture it before `sql` may be consumed by the describe path below.
        let export_sql = params.is_empty().then(|| sql.clone());

        // Headers from the first row, or by describing the statement when empty
        // (so the grid still shows columns). The describe path can't bind, but
        // an empty result means the filters excluded everything — the column
        // shape is independent of the bound values, so the bare SQL describes fine.
        let columns = if let Some(first) = rows.first() {
            columns_of(first)
        } else {
            match pool.describe(AssertSqlSafe(sql).into_sql_str()).await {
                Ok(d) => d
                    .columns
                    .iter()
                    .map(|c| ColumnMeta {
                        name: c.name().to_string(),
                        data_type: c.type_info().name().to_string(),
                    })
                    .collect(),
                Err(_) => Vec::new(),
            }
        };
        let data = rows.iter().map(row_to_json).collect();
        Ok(QueryResult {
            columns,
            rows: data,
            rows_affected: None,
            elapsed_ms: start.elapsed().as_millis(),
            // Browsing is page-based (LIMIT/OFFSET from BrowseSpec), so the
            // per-query row cap doesn't apply.
            result_truncated: false,
            sql: export_sql,
        })
    }

    async fn execute(&self, conn: Arc<dyn Connection>, sql: &str) -> Result<Vec<QueryResult>> {
        let conn = downcast_conn::<PostgresConnection>(&conn)?;
        let pool = conn.pool();

        // A script may hold several statements; run them in order on the same
        // pool and return one result each. SELECT-ish statements yield a row
        // grid; other statements yield their affected-row count.
        let mut results = Vec::new();
        for stmt in split_statements(sql) {
            let start = Instant::now();
            if is_select(&stmt) {
                let (columns, rows, result_truncated) =
                    select_result(&pool, &stmt, DEFAULT_MAX_ROW_COUNT)
                        .await
                        .inspect_err(|e| {
                            tracing::warn!("postgres select failed: {e}");
                        })?;
                let elapsed_ms = start.elapsed().as_millis();
                tracing::debug!(
                    "select returned {} row(s){} in {elapsed_ms}ms",
                    rows.len(),
                    if result_truncated { " (truncated)" } else { "" }
                );
                results.push(QueryResult {
                    columns,
                    rows,
                    rows_affected: None,
                    elapsed_ms,
                    result_truncated,
                    sql: Some(stmt.clone()),
                });
            } else {
                let res = sqlx::query(AssertSqlSafe(stmt.clone()))
                    .execute(&pool)
                    .await
                    .map_err(|e| {
                        tracing::warn!(
                            "postgres statement failed: {}",
                            format_db_error(&e, Some(&stmt))
                        );
                        db_backend_err(e, Some(&stmt))
                    })?;
                let elapsed_ms = start.elapsed().as_millis();
                tracing::debug!(
                    "statement affected {} row(s) in {elapsed_ms}ms",
                    res.rows_affected()
                );
                results.push(QueryResult {
                    columns: Vec::new(),
                    rows: Vec::new(),
                    rows_affected: Some(res.rows_affected()),
                    elapsed_ms,
                    result_truncated: false,
                    sql: None,
                });
            }
        }
        Ok(results)
    }

    async fn export_csv(&self, conn: Arc<dyn Connection>, sql: &str, path: &str) -> Result<u64> {
        let conn = downcast_conn::<PostgresConnection>(&conn)?;
        let pool = conn.pool();

        // Stream the full result set (no row cap) straight to the file so a
        // large export never materialises in memory. Column headers come from
        // the first row, or `describe` when the result is empty.
        let mut stream = sqlx::query(AssertSqlSafe(sql)).fetch(&pool);
        let mut writer: Option<CsvWriter> = None;
        let mut count: u64 = 0;
        while let Some(row) = stream.try_next().await.map_err(|e| {
            tracing::warn!(
                "postgres export query failed: {}",
                format_db_error(&e, Some(sql))
            );
            db_backend_err(e, Some(sql))
        })? {
            if writer.is_none() {
                writer = Some(CsvWriter::create(path, &columns_of(&row))?);
            }
            // Safe: set on the first iteration above.
            writer.as_mut().unwrap().write_row(&row_to_json(&row))?;
            count += 1;
        }
        drop(stream);

        // Empty result: still write a header-only file using the described columns.
        let writer = match writer {
            Some(w) => w,
            None => {
                let columns = match pool.describe(AssertSqlSafe(sql).into_sql_str()).await {
                    Ok(d) => d
                        .columns
                        .iter()
                        .map(|c| ColumnMeta {
                            name: c.name().to_string(),
                            data_type: c.type_info().name().to_string(),
                        })
                        .collect::<Vec<_>>(),
                    Err(_) => Vec::new(),
                };
                CsvWriter::create(path, &columns)?
            }
        };
        writer.finish()?;
        tracing::debug!("exported {count} row(s) to {path}");
        Ok(count)
    }

    async fn apply_changes(
        &self,
        conn: Arc<dyn Connection>,
        schema: &str,
        table: &str,
        changes: RowChanges,
    ) -> Result<ApplyResult> {
        let conn = downcast_conn::<PostgresConnection>(&conn)?;
        let mut tx = conn
            .pool()
            .begin()
            .await
            .map_err(|e| db_backend_err(e, None))?;

        let mut result = ApplyResult::default();

        for u in &changes.updates {
            let (sql, params) = build_update(schema, table, &u.pk, &u.changes)?;
            result.updated += run_dml(&mut tx, &sql, &params).await?;
        }
        for values in &changes.inserts {
            let (sql, params) = build_insert(schema, table, values)?;
            result.inserted += run_dml(&mut tx, &sql, &params).await?;
        }
        for pk in &changes.deletes {
            let (sql, params) = build_delete(schema, table, pk)?;
            result.deleted += run_dml(&mut tx, &sql, &params).await?;
        }

        tx.commit().await.map_err(|e| db_backend_err(e, None))?;
        Ok(result)
    }
}

/// Bind `params` to `sql` and execute it on the transaction, returning the
/// number of affected rows.
async fn run_dml(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    sql: &str,
    params: &[String],
) -> Result<u64> {
    let mut q = sqlx::query(AssertSqlSafe(sql));
    for p in params {
        q = q.bind(p);
    }
    let res = q
        .execute(&mut **tx)
        .await
        .map_err(|e| db_backend_err(e, Some(sql)))?;
    Ok(res.rows_affected())
}

/// Build an `UPDATE ... SET ... WHERE ...` and its bound parameters.
fn build_update(
    schema: &str,
    table: &str,
    pk: &[ColumnValue],
    changes: &[ColumnValue],
) -> Result<(String, Vec<String>)> {
    if changes.is_empty() {
        return Err(PluginError::Config("no columns to update".into()));
    }
    let mut params: Vec<String> = Vec::new();
    let mut set_parts = Vec::new();
    for cv in changes {
        set_parts.push(format!(
            "{} = {}",
            quote_ident(&cv.column),
            value_expr(cv, &mut params)?
        ));
    }
    let where_sql = build_where(pk, &mut params)?;
    let sql = format!(
        "UPDATE {}.{} SET {} WHERE {}",
        quote_ident(schema),
        quote_ident(table),
        set_parts.join(", "),
        where_sql,
    );
    Ok((sql, params))
}

/// Build an `INSERT INTO ...` and its bound parameters.
fn build_insert(
    schema: &str,
    table: &str,
    values: &[ColumnValue],
) -> Result<(String, Vec<String>)> {
    let mut params: Vec<String> = Vec::new();
    let mut cols = Vec::new();
    let mut vals = Vec::new();
    for cv in values {
        cols.push(quote_ident(&cv.column));
        vals.push(value_expr(cv, &mut params)?);
    }
    let sql = if cols.is_empty() {
        format!(
            "INSERT INTO {}.{} DEFAULT VALUES",
            quote_ident(schema),
            quote_ident(table),
        )
    } else {
        format!(
            "INSERT INTO {}.{} ({}) VALUES ({})",
            quote_ident(schema),
            quote_ident(table),
            cols.join(", "),
            vals.join(", "),
        )
    };
    Ok((sql, params))
}

/// Build a `DELETE FROM ... WHERE ...` and its bound parameters.
fn build_delete(schema: &str, table: &str, pk: &[ColumnValue]) -> Result<(String, Vec<String>)> {
    let mut params: Vec<String> = Vec::new();
    let where_sql = build_where(pk, &mut params)?;
    let sql = format!(
        "DELETE FROM {}.{} WHERE {}",
        quote_ident(schema),
        quote_ident(table),
        where_sql,
    );
    Ok((sql, params))
}

/// Build the browse `SELECT * FROM s.t [WHERE ...] [ORDER BY ...] LIMIT n OFFSET m`
/// and its bound parameters. Structured filters bind their values (via
/// `value_expr`); the raw `where_sql` fragment, if any, is AND'd in verbatim.
fn build_browse(schema: &str, table: &str, spec: &BrowseSpec) -> Result<(String, Vec<String>)> {
    let mut params: Vec<String> = Vec::new();
    let mut conds: Vec<String> = Vec::new();

    for f in &spec.filters {
        conds.push(filter_expr(f, &mut params)?);
    }
    if let Some(w) = spec
        .where_sql
        .as_deref()
        .map(str::trim)
        .filter(|w| !w.is_empty())
    {
        conds.push(format!("({w})"));
    }

    let mut sql = format!(
        "SELECT * FROM {}.{}",
        quote_ident(schema),
        quote_ident(table),
    );
    if !conds.is_empty() {
        sql.push_str(&format!(" WHERE {}", conds.join(" AND ")));
    }
    if !spec.sorts.is_empty() {
        let terms: Vec<String> = spec
            .sorts
            .iter()
            .map(|s| {
                format!(
                    "{} {}",
                    quote_ident(&s.column),
                    if s.descending { "DESC" } else { "ASC" }
                )
            })
            .collect();
        sql.push_str(&format!(" ORDER BY {}", terms.join(", ")));
    }
    // limit/offset are u32, so they're safe to interpolate directly.
    sql.push_str(&format!(" LIMIT {} OFFSET {}", spec.limit, spec.offset));
    Ok((sql, params))
}

/// Render one structured filter as a SQL condition, binding its value when the
/// operator takes one. The operator comes from the fixed allow-list
/// ([`BrowseFilter::OPS`]); anything else is rejected so no operator text is
/// interpolated unchecked.
fn filter_expr(f: &BrowseFilter, params: &mut Vec<String>) -> Result<String> {
    let col = quote_ident(&f.column);
    match f.op.as_str() {
        "is_null" => Ok(format!("{col} IS NULL")),
        "is_not_null" => Ok(format!("{col} IS NOT NULL")),
        op => {
            let sql_op = match op {
                "eq" => "=",
                "ne" => "<>",
                "lt" => "<",
                "lte" => "<=",
                "gt" => ">",
                "gte" => ">=",
                "like" => "LIKE",
                "ilike" => "ILIKE",
                _ => {
                    return Err(PluginError::Config(format!(
                        "invalid filter operator: {op}"
                    )))
                }
            };
            // Reuse the edit path's value binding: CAST($n AS type). A null
            // value with a comparison operator never matches, which is the
            // sensible result for "= NULL" via the UI.
            let cv = ColumnValue {
                column: f.column.clone(),
                type_name: f.type_name.clone(),
                value: f.value.clone(),
            };
            Ok(format!("{col} {sql_op} {}", value_expr(&cv, params)?))
        }
    }
}

/// Quote an SQL identifier, escaping embedded double quotes.
fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Reject type names that aren't a plausible SQL type, so they can be
/// interpolated into a `CAST(... AS <type>)` without opening an injection hole.
/// Postgres type names are letters/digits/underscores plus spaces, parens and
/// brackets (`character varying`, `numeric(10,2)`, `int[]`).
fn validate_type(t: &str) -> Result<&str> {
    let ok = !t.is_empty()
        && t.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, ' ' | '_' | '(' | ')' | '[' | ']' | ',' | '.')
        });
    if ok {
        Ok(t)
    } else {
        Err(PluginError::Config(format!("invalid column type: {t}")))
    }
}

/// Render the SQL expression for a value: `NULL`, or a `CAST($n AS <type>)`
/// placeholder whose bound text is appended to `params`.
fn value_expr(cv: &ColumnValue, params: &mut Vec<String>) -> Result<String> {
    if cv.value.is_null() {
        return Ok("NULL".into());
    }
    let ty = validate_type(&cv.type_name)?;
    params.push(bind_text(&cv.value));
    Ok(format!("CAST(${} AS {})", params.len(), ty))
}

/// Build a `WHERE` clause that matches a single row by the given key columns.
fn build_where(key: &[ColumnValue], params: &mut Vec<String>) -> Result<String> {
    if key.is_empty() {
        return Err(PluginError::Config(
            "cannot identify row: no key columns".into(),
        ));
    }
    let mut parts = Vec::new();
    for cv in key {
        if cv.value.is_null() {
            parts.push(format!("{} IS NULL", quote_ident(&cv.column)));
        } else {
            parts.push(format!(
                "{} = {}",
                quote_ident(&cv.column),
                value_expr(cv, params)?
            ));
        }
    }
    Ok(parts.join(" AND "))
}

/// Text form of a JSON value to bind as a string parameter; Postgres coerces it
/// via the surrounding `CAST`. Objects/arrays serialize back to JSON text (for
/// `json`/`jsonb`/array columns).
fn bind_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn is_select(sql: &str) -> bool {
    let head = sql
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(head.as_str(), "select" | "with" | "show" | "explain")
}

/// Map a 1-based Postgres cursor `position` (in characters) to a
/// `(line, column)` pair within `sql` (both 1-based). Positions past the end
/// clamp to the end of the string; position 0 maps to (1, 1).
fn line_col(sql: &str, position: usize) -> (usize, usize) {
    let idx = position.saturating_sub(1);
    let mut line = 1usize;
    let mut col = 1usize;
    for (i, ch) in sql.chars().enumerate() {
        if i >= idx {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// Render a `sqlx::Error` for the user.
///
/// `sqlx`'s `Display` for `PgDatabaseError` appends the *server source-code*
/// line that raised the error (e.g. `syntax error at or near "fromm" at line
/// 1244`), which reads as a SQL line number but isn't one. Build the message
/// from the server's `message()` plus its SQL cursor `position()` (mapped to a
/// line/column in `sql` when available) instead, and surface `detail`/`hint`
/// when present. The server's internal `file`/`line`/`routine` fields are
/// deliberately omitted.
fn format_db_error(e: &sqlx::Error, sql: Option<&str>) -> String {
    if let Some(db) = e.as_database_error() {
        if let Some(pg) = db.try_downcast_ref::<PgDatabaseError>() {
            let mut msg = pg.message().to_owned();
            match pg.position() {
                Some(PgErrorPosition::Original(pos)) => {
                    if let Some(sql) = sql {
                        let (line, col) = line_col(sql, pos);
                        msg.push_str(&format!(" (line {line}, column {col})"));
                    } else {
                        msg.push_str(&format!(" (at character {pos})"));
                    }
                }
                Some(PgErrorPosition::Internal { position, .. }) => {
                    msg.push_str(&format!(" (in internal query at character {position})"));
                }
                None => {}
            }
            if !pg.code().is_empty() {
                msg.push_str(&format!(" [{}]", pg.code()));
            }
            if let Some(detail) = pg.detail() {
                msg.push_str(&format!(" Detail: {detail}"));
            }
            if let Some(hint) = pg.hint() {
                msg.push_str(&format!(" Hint: {hint}"));
            }
            if let Some(where_) = pg.r#where() {
                msg.push_str(&format!(" Where: {where_}"));
            }
            return msg;
        }
        return e.to_string();
    }
    e.to_string()
}

/// Map a `sqlx::Error` from running `sql` (when available) to a `Backend`
/// error without Postgres server internals. See [`format_db_error`].
fn db_backend_err(e: sqlx::Error, sql: Option<&str>) -> PluginError {
    PluginError::Backend(format_db_error(&e, sql))
}

/// Run one SELECT-ish statement and return its column metadata + JSON rows,
/// plus whether the result was truncated. Streams rows and stops once `max`
/// have been collected (fetching one extra to detect that more remain), so a
/// huge result set never materializes fully in memory. When zero rows come back
/// we `describe` the statement so the grid still shows the column headers rather
/// than nothing.
async fn select_result(
    pool: &PgPool,
    stmt: &str,
    max: u64,
) -> Result<(Vec<ColumnMeta>, Vec<Vec<serde_json::Value>>, bool)> {
    let mut stream = sqlx::query(AssertSqlSafe(stmt)).fetch(pool);
    let mut rows: Vec<PgRow> = Vec::new();
    let mut truncated = false;
    // Fetch up to `max` rows; pull one more only to learn whether the result
    // extends past the cap, then discard it.
    while let Some(row) = stream
        .try_next()
        .await
        .map_err(|e| db_backend_err(e, Some(stmt)))?
    {
        if rows.len() as u64 >= max {
            truncated = true;
            break;
        }
        rows.push(row);
    }
    // Drop the stream (and its borrow of the pool) before any further awaits.
    drop(stream);

    let columns: Vec<ColumnMeta> = if let Some(first) = rows.first() {
        columns_of(first)
    } else {
        match pool.describe(AssertSqlSafe(stmt).into_sql_str()).await {
            Ok(d) => d
                .columns
                .iter()
                .map(|c| ColumnMeta {
                    name: c.name().to_string(),
                    data_type: c.type_info().name().to_string(),
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    };
    let data = rows.iter().map(row_to_json).collect();
    Ok((columns, data, truncated))
}

/// Column metadata (name + type) for a fetched row.
fn columns_of(row: &PgRow) -> Vec<ColumnMeta> {
    row.columns()
        .iter()
        .map(|c| ColumnMeta {
            name: c.name().to_string(),
            data_type: c.type_info().name().to_string(),
        })
        .collect()
}

/// Split a SQL script into individual statements on top-level semicolons,
/// ignoring `;` inside single-quoted strings, double-quoted identifiers,
/// dollar-quoted bodies (`$tag$ … $tag$`), and line/block comments. Returns
/// trimmed, non-empty statements. (Standard strings use `''` to escape a quote;
/// backslash escapes in `E'…'` strings are not special-cased.)
fn split_statements(sql: &str) -> Vec<String> {
    let b = sql.as_bytes();
    let n = b.len();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < n {
        match b[i] {
            b'\'' | b'"' => {
                let q = b[i];
                i += 1;
                while i < n {
                    if b[i] == q {
                        // Doubled quote is an escaped quote, not a terminator.
                        if i + 1 < n && b[i + 1] == q {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'-' if i + 1 < n && b[i + 1] == b'-' => {
                i += 2;
                while i < n && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < n && b[i + 1] == b'*' => {
                i += 2;
                let mut depth = 1; // Postgres block comments nest.
                while i < n && depth > 0 {
                    if b[i] == b'/' && i + 1 < n && b[i + 1] == b'*' {
                        depth += 1;
                        i += 2;
                    } else if b[i] == b'*' && i + 1 < n && b[i + 1] == b'/' {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            b'$' => {
                if let Some(tag_end) = dollar_tag_end(b, i) {
                    let tag = &b[i..=tag_end];
                    i = tag_end + 1;
                    while i < n {
                        if b[i] == b'$' && b[i..].starts_with(tag) {
                            i += tag.len();
                            break;
                        }
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
            b';' => {
                let stmt = sql[start..i].trim();
                if !stmt.is_empty() {
                    out.push(stmt.to_string());
                }
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    let tail = sql[start..].trim();
    if !tail.is_empty() {
        out.push(tail.to_string());
    }
    out
}

/// If a dollar-quote tag opens at `start` (`b[start] == b'$'`), return the index
/// of its closing `$` (so `b[start..=end]` is the full `$tag$`). A tag is `$$`
/// or `$ident$` where `ident` is `[A-Za-z_][A-Za-z0-9_]*` — this rules out `$1`
/// style placeholders.
fn dollar_tag_end(b: &[u8], start: usize) -> Option<usize> {
    let mut j = start + 1;
    while j < b.len() {
        let c = b[j];
        if c == b'$' {
            return Some(j);
        }
        if c == b'_' || c.is_ascii_alphabetic() || (j > start + 1 && c.is_ascii_digit()) {
            j += 1;
        } else {
            return None;
        }
    }
    None
}

fn row_to_json(row: &PgRow) -> Vec<serde_json::Value> {
    (0..row.len()).map(|i| value_to_json(row, i)).collect()
}

/// Convert one cell to JSON, decoding by the column's Postgres type so numbers
/// stay numbers and dates/uuids/etc. render as readable text. sqlx is strict
/// about Rust↔SQL type pairing (e.g. `int4` is `i32`, not `i64`), so we match on
/// the type name and decode the right Rust type. Anything we don't recognise
/// falls back to its text encoding, then a `<type>` placeholder, so a cell is
/// never silently blank.
fn value_to_json(row: &PgRow, i: usize) -> serde_json::Value {
    use serde_json::Value;
    use sqlx::types::chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
    use sqlx::types::{BigDecimal, Uuid};
    use sqlx::ValueRef;

    let raw = match row.try_get_raw(i) {
        Ok(r) => r,
        Err(_) => return Value::Null,
    };
    if raw.is_null() {
        return Value::Null;
    }
    let type_name = raw.type_info().name().to_string();

    // Decode column `i` as `$t` and, on success, return `$conv(value)`. A decode
    // error falls through to the next arm / the text fallback below.
    macro_rules! decode {
        ($t:ty, $conv:expr) => {
            if let Ok(v) = row.try_get::<$t, _>(i) {
                return $conv(v);
            }
        };
    }
    let num_f64 = |v: f64| {
        serde_json::Number::from_f64(v)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    };
    let to_str = |v: String| Value::String(v);

    match type_name.to_uppercase().as_str() {
        "BOOL" => decode!(bool, Value::Bool),
        "INT2" | "SMALLINT" | "SMALLSERIAL" => decode!(i16, |v: i16| Value::Number(v.into())),
        "INT4" | "INT" | "INTEGER" | "SERIAL" => decode!(i32, |v: i32| Value::Number(v.into())),
        "INT8" | "BIGINT" | "BIGSERIAL" => decode!(i64, |v: i64| Value::Number(v.into())),
        "OID" => decode!(
            sqlx::postgres::types::Oid,
            |v: sqlx::postgres::types::Oid| { Value::Number(v.0.into()) }
        ),
        "FLOAT4" | "REAL" => decode!(f32, |v: f32| num_f64(v as f64)),
        "FLOAT8" | "DOUBLE PRECISION" => decode!(f64, num_f64),
        "NUMERIC" | "DECIMAL" | "MONEY" => {
            decode!(BigDecimal, |v: BigDecimal| { Value::String(v.to_string()) })
        }
        "TIMESTAMPTZ" => decode!(DateTime<Utc>, |v: DateTime<Utc>| {
            Value::String(v.to_rfc3339())
        }),
        "TIMESTAMP" => decode!(NaiveDateTime, |v: NaiveDateTime| Value::String(
            v.to_string()
        )),
        "DATE" => decode!(NaiveDate, |v: NaiveDate| Value::String(v.to_string())),
        "TIME" | "TIMETZ" => decode!(NaiveTime, |v: NaiveTime| Value::String(v.to_string())),
        "UUID" => decode!(Uuid, |v: Uuid| Value::String(v.to_string())),
        "JSON" | "JSONB" => decode!(Value, |v| v),
        "BYTEA" => decode!(Vec<u8>, |v: Vec<u8>| Value::String(bytea_hex(&v))),
        // Array types are named `_int4`, `_text`, … (or `INT4[]`); decode the
        // common element types into a JSON array.
        "_BOOL" => decode!(Vec<bool>, |v: Vec<bool>| json_array(v, Value::Bool)),
        "_INT2" => decode!(Vec<i16>, |v: Vec<i16>| json_array(v, |x: i16| {
            Value::Number(x.into())
        })),
        "_INT4" => decode!(Vec<i32>, |v: Vec<i32>| json_array(v, |x: i32| {
            Value::Number(x.into())
        })),
        "_INT8" => decode!(Vec<i64>, |v: Vec<i64>| json_array(v, |x: i64| {
            Value::Number(x.into())
        })),
        "_FLOAT8" => decode!(Vec<f64>, |v: Vec<f64>| json_array(v, num_f64)),
        "_TEXT" | "_VARCHAR" => decode!(Vec<String>, |v: Vec<String>| json_array(v, to_str)),
        _ => {}
    }

    // Text-family types (text/varchar/bpchar/name/enum-as-text, etc.).
    decode!(String, to_str);

    // Last resort: many remaining types (enums, inet, etc.) send their value as
    // UTF-8 text; surface that. If it isn't valid text, show the type name so
    // the cell isn't blank.
    if let Ok(bytes) = raw.as_bytes() {
        if let Ok(s) = std::str::from_utf8(bytes) {
            return Value::String(s.to_string());
        }
    }
    Value::String(format!("<{type_name}>"))
}

/// Hex-encode bytes as a Postgres `\x…` bytea literal for display.
fn bytea_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(2 + bytes.len() * 2);
    s.push_str("\\x");
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Map a decoded Postgres array into a JSON array via `conv`.
fn json_array<T>(items: Vec<T>, conv: impl Fn(T) -> serde_json::Value) -> serde_json::Value {
    serde_json::Value::Array(items.into_iter().map(conv).collect())
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

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
use sqlx::mysql::{MySqlPool, MySqlPoolOptions, MySqlRow};
use sqlx::{AssertSqlSafe, SqlSafeStr};
use sqlx::{Column as _, Executor, Row, TypeInfo};
use std::sync::{Arc, RwLock};
use std::time::Instant;

pub struct MysqlPlugin;

impl MysqlPlugin {
    pub fn new() -> Self {
        Self
    }

    /// Index metadata for the structure view. Helper for
    /// [`describe_table`](RdbmsPlugin::describe_table) when `include_indexes` is set.
    async fn list_indexes(
        &self,
        conn: &MysqlConnection,
        schema: &str,
        table: &str,
    ) -> Result<Vec<Index>> {
        // statistics has one row per index column; aggregate into one Index per
        // index name with columns ordered by seq_in_index.
        let rows: Vec<(String, String, i64, String)> = sqlx::query_as(
            // non_unique is unsigned on some servers, signed on others; CAST to
            // SIGNED for a deterministic decode. seq_in_index is only needed for
            // ordering, so it stays in ORDER BY and out of the projection.
            "select index_name, column_name, cast(non_unique as signed), index_type \
             from information_schema.statistics \
             where table_schema = ? and table_name = ? \
             order by (index_name = 'PRIMARY') desc, index_name, seq_in_index",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&conn.pool())
        .await
        .map_err(|e| PluginError::Backend(e.to_string()))?;

        // Group consecutive rows by index_name (the query orders by name).
        let mut out: Vec<Index> = Vec::new();
        for (name, column, non_unique, method) in rows {
            if let Some(last) = out.last_mut().filter(|ix| ix.name == name) {
                last.columns.push(column);
            } else {
                out.push(Index {
                    primary: name == "PRIMARY",
                    unique: non_unique == 0,
                    method,
                    columns: vec![column],
                    name,
                });
            }
        }
        Ok(out)
    }
}

impl Default for MysqlPlugin {
    fn default() -> Self {
        Self::new()
    }
}

/// A live connection to one database on a MySQL server.
///
/// In MySQL a "schema" *is* a database, and a connection is scoped to one
/// default database. The pool sits behind a lock so the active database can be
/// switched in place (by building a fresh pool pointed at the new database)
/// without changing the connection's id. `config` keeps the original
/// host/credentials so a new pool can be built for another database.
pub struct MysqlConnection {
    pool: RwLock<MySqlPool>,
    config: ConnectionConfig,
}

impl MysqlConnection {
    /// A cheap clone of the current pool (sqlx pools are internally `Arc`d). The
    /// lock is released before the returned pool is awaited on, so a concurrent
    /// `use_database` swap never blocks in-flight queries.
    fn pool(&self) -> MySqlPool {
        self.pool.read().unwrap().clone()
    }
}

impl Connection for MysqlConnection {
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

/// Build a `mysql://` URL from a config, pointed at `database`. The database is
/// passed explicitly (rather than read from `cfg`) so the same credentials can
/// be reused to connect to a different database when switching.
fn build_url(cfg: &ConnectionConfig, database: &str) -> Result<String> {
    let host = cfg_str(cfg, "host")?;
    let port = cfg_u16(cfg, "port", 3306);
    let user = cfg_str(cfg, "user")?;
    let password = cfg_secret(cfg, "password")?.unwrap_or_default();
    let ssl = cfg_str_opt(cfg, "ssl").unwrap_or_else(|| "preferred".into());
    Ok(format!(
        "mysql://{u}:{p}@{host}:{port}/{database}?ssl-mode={ssl}",
        u = urlencode(&user),
        p = urlencode(&password),
        database = urlencode(database),
    ))
}

async fn make_pool(url: &str) -> Result<MySqlPool> {
    MySqlPoolOptions::new()
        .max_connections(8)
        .connect(url)
        .await
        .map_err(|e| PluginError::Connection(e.to_string()))
}

/// Replace the database path segment in a `mysql://` URL with `database`.
fn swap_database_in_url(base: &str, database: &str) -> Result<String> {
    // A mysql URL looks like: mysql://user:pass@host:port/dbname[?params]
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
        urlencode(database)
    ))
}

/// The default database for a connection: the `database` field, or the one
/// embedded in a connection-string URL.
fn current_database(cfg: &ConnectionConfig) -> Result<String> {
    if cfg_str_opt(cfg, "mode").as_deref() == Some("url") {
        let url = cfg_str(cfg, "connection_string")?;
        let after_scheme = url
            .find("://")
            .map(|i| i + 3)
            .ok_or_else(|| PluginError::Config("invalid connection string: no scheme".into()))?;
        let path_start = url[after_scheme..]
            .find('/')
            .map(|i| after_scheme + i + 1)
            .ok_or_else(|| PluginError::Config("connection string has no database".into()))?;
        let end = url[path_start..]
            .find('?')
            .map(|i| path_start + i)
            .unwrap_or(url.len());
        Ok(urldecode(&url[path_start..end]))
    } else {
        cfg_str(cfg, "database")
    }
}

#[async_trait]
impl Plugin for MysqlPlugin {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            id: "mysql".into(),
            name: "MySQL".into(),
            kind: PluginKind::Rdbms,
            version: env!("CARGO_PKG_VERSION").into(),
            description: "Connect to MySQL 8.0+ and MariaDB databases.".into(),
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
                    default: Some("mysql://user:password@localhost:3306/dbname".into()),
                    placeholder: Some("mysql://user:password@localhost:3306/dbname".into()),
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
                    default: Some(serde_json::json!(3306)),
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
                    placeholder: Some("mysql".into()),
                    show_if: Some(polyclient_core::ShowIf {
                        field: "mode".into(),
                        equals: "individual".into(),
                    }),
                },
                ConfigField {
                    key: "user".into(),
                    label: "User".into(),
                    field_type: ConfigFieldType::Text,
                    required: true,
                    default: Some(serde_json::json!("root")),
                    placeholder: Some("root".into()),
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
                    label: "SSL mode".into(),
                    field_type: ConfigFieldType::Select {
                        options: vec!["disabled".into(), "preferred".into(), "required".into()],
                    },
                    required: false,
                    default: Some(serde_json::json!("preferred")),
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
            "connecting to mysql host={} port={} db={}",
            cfg_str_opt(&cfg, "host").as_deref().unwrap_or("?"),
            cfg.get("port")
                .map(|v| v.to_string())
                .as_deref()
                .unwrap_or("?"),
            cfg_str_opt(&cfg, "database").as_deref().unwrap_or("?"),
        );
        let pool = make_pool(&url).await.inspect_err(|e| {
            tracing::warn!("mysql connection failed: {e}");
        })?;
        tracing::info!("mysql pool established");
        Ok(Arc::new(MysqlConnection {
            pool: RwLock::new(pool),
            config: cfg,
        }))
    }
}

#[async_trait]
impl RdbmsPlugin for MysqlPlugin {
    async fn list_schemas(&self, conn: Arc<dyn Connection>) -> Result<Vec<Schema>> {
        // In MySQL a schema is a database. Expose only the connection's current
        // database as the browsable "schema" (the database picker, via
        // list_databases, covers the rest), so the tree matches what the active
        // pool can actually query.
        let conn = downcast_conn::<MysqlConnection>(&conn)?;
        let db = current_database(&conn.config)?;
        Ok(vec![Schema { name: db }])
    }

    async fn list_databases(&self, conn: Arc<dyn Connection>) -> Result<Vec<String>> {
        let conn = downcast_conn::<MysqlConnection>(&conn)?;
        let rows: Vec<(String,)> = sqlx::query_as(
            "select schema_name from information_schema.schemata \
             where schema_name not in \
               ('mysql','information_schema','performance_schema','sys') \
             order by schema_name",
        )
        .fetch_all(&conn.pool())
        .await
        .map_err(|e| PluginError::Backend(e.to_string()))?;
        Ok(rows.into_iter().map(|(n,)| n).collect())
    }

    async fn use_database(&self, conn: Arc<dyn Connection>, database: &str) -> Result<()> {
        let conn = downcast_conn::<MysqlConnection>(&conn)?;
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
        let conn = downcast_conn::<MysqlConnection>(&conn)?;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "select table_name, table_type from information_schema.tables \
             where table_schema = ? order by table_name",
        )
        .bind(schema)
        .fetch_all(&conn.pool())
        .await
        .map_err(|e| PluginError::Backend(e.to_string()))?;

        Ok(rows
            .into_iter()
            .map(|(name, kind)| Table {
                schema: schema.to_string(),
                name,
                // MySQL reports BASE TABLE / VIEW / SYSTEM VIEW. There are no
                // materialized views in MySQL.
                kind: match kind.as_str() {
                    "VIEW" | "SYSTEM VIEW" => TableKind::View,
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
        let conn = downcast_conn::<MysqlConnection>(&conn)?;
        let rows: Vec<(
            String,         // column_name
            String,         // data_type (e.g. "int", "varchar")
            String,         // column_type (e.g. "int unsigned", "varchar(255)")
            String,         // is_nullable (YES/NO)
            Option<i64>,    // character_maximum_length
            Option<i64>,    // numeric_precision
            Option<i64>,    // numeric_scale
            String,         // column_key (PRI/UNI/MUL/'')
            Option<String>, // column_default
            Option<String>, // fk_table
            Option<String>, // fk_column
        )> = sqlx::query_as(
            // The numeric_* / length columns are typed inconsistently across
            // MySQL and MariaDB versions (signed vs unsigned BIGINT), so CAST
            // them to SIGNED for a deterministic wire type sqlx can decode.
            "select
               c.column_name,
               c.data_type,
               c.column_type,
               c.is_nullable,
               cast(c.character_maximum_length as signed),
               cast(c.numeric_precision as signed),
               cast(c.numeric_scale as signed),
               c.column_key,
               c.column_default,
               kcu.referenced_table_name,
               kcu.referenced_column_name
             from information_schema.columns c
             left join information_schema.key_column_usage kcu
               on  kcu.table_schema = c.table_schema
               and kcu.table_name   = c.table_name
               and kcu.column_name  = c.column_name
               and kcu.referenced_table_name is not null
             where c.table_schema = ? and c.table_name = ?
             order by c.ordinal_position",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(&conn.pool())
        .await
        .map_err(|e| PluginError::Backend(e.to_string()))?;

        let columns = rows
            .into_iter()
            .map(
                |(
                    name,
                    dtype,
                    col_type,
                    nullable,
                    char_len,
                    precision,
                    scale,
                    col_key,
                    default_value,
                    fk_table,
                    fk_col,
                )| {
                    let d = dtype.to_ascii_lowercase();
                    let json = d == "json";
                    let large = json || d.contains("text") || d.contains("blob") || d == "longtext";
                    let foreign_key = fk_table
                        .zip(fk_col)
                        .map(|(table, column)| ForeignKey { table, column });
                    Column {
                        name,
                        data_type: dtype,
                        // MySQL has no separate udt; surface the full column type
                        // (e.g. "int unsigned", "varchar(255)") for display.
                        udt_name: Some(col_type),
                        nullable: nullable == "YES",
                        primary_key: col_key == "PRI",
                        unique: col_key == "UNI",
                        default_value,
                        foreign_key,
                        char_max_length: char_len.map(|n| n as i32),
                        numeric_precision: precision.map(|n| n as i32),
                        numeric_scale: scale.map(|n| n as i32),
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
        let conn = downcast_conn::<MysqlConnection>(&conn)?;
        // MySQL exposes the authoritative DDL directly.
        let sql = format!(
            "SHOW CREATE TABLE {}.{}",
            quote_ident(schema),
            quote_ident(table)
        );
        let row: MySqlRow = sqlx::query(AssertSqlSafe(sql))
            .fetch_one(&conn.pool())
            .await
            .map_err(|e| PluginError::Backend(e.to_string()))?;

        // `SHOW CREATE TABLE` returns ("Table","Create Table") for a base table,
        // or ("View","Create View",...) for a view. The create statement is the
        // second column in both cases.
        let ddl: String = row
            .try_get::<String, _>(1)
            .map_err(|e| PluginError::Backend(e.to_string()))?;

        Ok(format!(
            "-- Generated DDL statements -- \n\n\n\
            -- Table: {schema}.{table} Definition -- \n\n{ddl};"
        ))
    }

    async fn browse_table(
        &self,
        conn: Arc<dyn Connection>,
        schema: &str,
        table: &str,
        spec: BrowseSpec,
    ) -> Result<QueryResult> {
        let conn = downcast_conn::<MysqlConnection>(&conn)?;
        let pool = conn.pool();

        let (sql, params) = build_browse(schema, table, &spec)?;
        let start = Instant::now();

        let mut q = sqlx::query(AssertSqlSafe(sql.clone()));
        for p in &params {
            q = q.bind(p);
        }
        let rows = q
            .fetch_all(&pool)
            .await
            .map_err(|e| PluginError::Backend(e.to_string()))?;

        // Only a parameterless browse SQL is re-runnable as-is for export.
        let export_sql = params.is_empty().then(|| sql.clone());

        // Headers from the first row, or by describing the statement when empty.
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
            result_truncated: false,
            sql: export_sql,
        })
    }

    async fn execute(&self, conn: Arc<dyn Connection>, sql: &str) -> Result<Vec<QueryResult>> {
        let conn = downcast_conn::<MysqlConnection>(&conn)?;
        let pool = conn.pool();

        let mut results = Vec::new();
        for stmt in split_statements(sql) {
            let start = Instant::now();
            if is_select(&stmt) {
                let (columns, rows, result_truncated) =
                    select_result(&pool, &stmt, DEFAULT_MAX_ROW_COUNT)
                        .await
                        .inspect_err(|e| {
                            tracing::warn!("mysql select failed: {e}");
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
                        tracing::warn!("mysql statement failed: {e}");
                        PluginError::Backend(e.to_string())
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
        let conn = downcast_conn::<MysqlConnection>(&conn)?;
        let pool = conn.pool();

        // Stream the full result set (no row cap) straight to the file.
        let mut stream = sqlx::query(AssertSqlSafe(sql)).fetch(&pool);
        let mut writer: Option<CsvWriter> = None;
        let mut count: u64 = 0;
        while let Some(row) = stream.try_next().await.map_err(|e| {
            tracing::warn!("mysql export query failed: {e}");
            PluginError::Backend(e.to_string())
        })? {
            if writer.is_none() {
                writer = Some(CsvWriter::create(path, &columns_of(&row))?);
            }
            writer.as_mut().unwrap().write_row(&row_to_json(&row))?;
            count += 1;
        }
        drop(stream);

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
        let conn = downcast_conn::<MysqlConnection>(&conn)?;
        let mut tx = conn
            .pool()
            .begin()
            .await
            .map_err(|e| PluginError::Backend(e.to_string()))?;

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

        tx.commit()
            .await
            .map_err(|e| PluginError::Backend(e.to_string()))?;
        Ok(result)
    }
}

/// Bind `params` to `sql` and execute it on the transaction, returning the
/// number of affected rows.
async fn run_dml(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
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
        .map_err(|e| PluginError::Backend(e.to_string()))?;
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
            "INSERT INTO {}.{} () VALUES ()",
            quote_ident(schema),
            quote_ident(table)
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

/// Build the browse `SELECT * FROM `s`.`t` [WHERE ...] [ORDER BY ...]
/// LIMIT n OFFSET m` and its bound parameters.
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
        quote_ident(table)
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
/// operator takes one. MySQL has no `ILIKE`; `like`/`ilike` both map to `LIKE`,
/// which is case-insensitive under the common `*_ci` collations.
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
                "like" | "ilike" => "LIKE",
                _ => {
                    return Err(PluginError::Config(format!(
                        "invalid filter operator: {op}"
                    )))
                }
            };
            let cv = ColumnValue {
                column: f.column.clone(),
                type_name: f.type_name.clone(),
                value: f.value.clone(),
            };
            Ok(format!("{col} {sql_op} {}", value_expr(&cv, params)?))
        }
    }
}

/// Quote an SQL identifier with backticks, escaping embedded backticks.
fn quote_ident(s: &str) -> String {
    format!("`{}`", s.replace('`', "``"))
}

/// Reject type names that aren't a plausible SQL type, so they can be
/// interpolated into a `CAST(... AS <type>)` without opening an injection hole.
fn validate_type(t: &str) -> Result<&str> {
    let ok = !t.is_empty()
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '_' | '(' | ')' | ','));
    if ok {
        Ok(t)
    } else {
        Err(PluginError::Config(format!("invalid column type: {t}")))
    }
}

/// MySQL's `CAST` only accepts a restricted set of target types (e.g. `CHAR`,
/// `SIGNED`, `UNSIGNED`, `DECIMAL`, `DATE`, `DATETIME`, `JSON`), not arbitrary
/// column types like `varchar(255)` or `int`. Map the column's declared type to
/// the nearest valid CAST target; default to `CHAR` so the bound string is
/// compared/stored as text and let MySQL coerce it.
fn cast_target(type_name: &str) -> &'static str {
    let t = type_name.to_ascii_lowercase();
    if t.contains("int") || t == "bit" || t == "bool" || t == "boolean" {
        "SIGNED"
    } else if t.contains("decimal")
        || t.contains("numeric")
        || t.contains("float")
        || t.contains("double")
    {
        "DECIMAL(65,30)"
    } else if t == "date" {
        "DATE"
    } else if t == "datetime" || t == "timestamp" {
        "DATETIME"
    } else if t == "time" {
        "TIME"
    } else if t == "json" {
        "JSON"
    } else {
        "CHAR"
    }
}

/// Render the SQL expression for a value: `NULL`, or a `CAST(? AS <target>)`
/// placeholder whose bound text is appended to `params`. The `?` placeholder is
/// MySQL's positional bind marker.
fn value_expr(cv: &ColumnValue, params: &mut Vec<String>) -> Result<String> {
    if cv.value.is_null() {
        return Ok("NULL".into());
    }
    // Validate the declared type (defensive — it isn't interpolated directly,
    // but keeps malformed input out), then bind via a safe CAST target.
    validate_type(&cv.type_name)?;
    params.push(bind_text(&cv.value));
    Ok(format!("CAST(? AS {})", cast_target(&cv.type_name)))
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

/// Text form of a JSON value to bind as a string parameter; MySQL coerces it via
/// the surrounding `CAST`. Objects/arrays serialize back to JSON text.
fn bind_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => {
            if *b {
                "1".into()
            } else {
                "0".into()
            }
        }
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
    matches!(
        head.as_str(),
        "select" | "with" | "show" | "explain" | "describe" | "desc"
    )
}

/// Run one SELECT-ish statement and return its column metadata + JSON rows, plus
/// whether the result was truncated. Streams rows and stops once `max` have been
/// collected. When zero rows come back we `describe` the statement so the grid
/// still shows the column headers.
async fn select_result(
    pool: &MySqlPool,
    stmt: &str,
    max: u64,
) -> Result<(Vec<ColumnMeta>, Vec<Vec<serde_json::Value>>, bool)> {
    let mut stream = sqlx::query(AssertSqlSafe(stmt)).fetch(pool);
    let mut rows: Vec<MySqlRow> = Vec::new();
    let mut truncated = false;
    while let Some(row) = stream
        .try_next()
        .await
        .map_err(|e| PluginError::Backend(e.to_string()))?
    {
        if rows.len() as u64 >= max {
            truncated = true;
            break;
        }
        rows.push(row);
    }
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
fn columns_of(row: &MySqlRow) -> Vec<ColumnMeta> {
    row.columns()
        .iter()
        .map(|c| ColumnMeta {
            name: c.name().to_string(),
            data_type: c.type_info().name().to_string(),
        })
        .collect()
}

/// Split a SQL script into individual statements on top-level semicolons,
/// ignoring `;` inside single/double-quoted strings, backtick-quoted
/// identifiers, and `--`/`#`/`/* */` comments. MySQL string literals escape an
/// embedded quote either by doubling it or with a backslash; both are handled.
/// MySQL block comments do not nest.
fn split_statements(sql: &str) -> Vec<String> {
    let b = sql.as_bytes();
    let n = b.len();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < n {
        match b[i] {
            b'\'' | b'"' | b'`' => {
                let q = b[i];
                i += 1;
                while i < n {
                    if b[i] == b'\\' && q != b'`' {
                        // Backslash escapes the next byte in string literals
                        // (not in backtick identifiers).
                        i += 2;
                        continue;
                    }
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
            b'#' => {
                i += 1;
                while i < n && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < n && b[i + 1] == b'*' => {
                i += 2;
                while i < n {
                    if b[i] == b'*' && i + 1 < n && b[i + 1] == b'/' {
                        i += 2;
                        break;
                    }
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

fn row_to_json(row: &MySqlRow) -> Vec<serde_json::Value> {
    (0..row.len()).map(|i| value_to_json(row, i)).collect()
}

/// Convert one cell to JSON, decoding by the column's MySQL type so numbers stay
/// numbers and dates/json/etc. render readably. sqlx is strict about Rust↔SQL
/// type pairing, so we match on the type name and decode the right Rust type.
/// Anything unrecognised falls back to its text/byte encoding so a cell is never
/// silently blank.
fn value_to_json(row: &MySqlRow, i: usize) -> serde_json::Value {
    use serde_json::Value;
    use sqlx::types::chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
    use sqlx::types::BigDecimal;
    use sqlx::ValueRef;

    let raw = match row.try_get_raw(i) {
        Ok(r) => r,
        Err(_) => return Value::Null,
    };
    if raw.is_null() {
        return Value::Null;
    }
    let type_name = raw.type_info().name().to_string();

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

    // MySQL surfaces unsigned ints with " UNSIGNED" appended to the type name.
    let upper = type_name.to_uppercase();
    let unsigned = upper.contains("UNSIGNED");
    let base = upper.split_whitespace().next().unwrap_or("");

    match base {
        "BOOLEAN" => decode!(bool, Value::Bool),
        // TINYINT(1) is the conventional boolean, but treat all tinyints as ints.
        "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "INTEGER" if unsigned => {
            decode!(u64, |v: u64| Value::Number(v.into()))
        }
        "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "INTEGER" => {
            decode!(i64, |v: i64| Value::Number(v.into()))
        }
        "BIGINT" if unsigned => decode!(u64, |v: u64| Value::Number(v.into())),
        "BIGINT" => decode!(i64, |v: i64| Value::Number(v.into())),
        "FLOAT" => decode!(f32, |v: f32| num_f64(v as f64)),
        "DOUBLE" => decode!(f64, num_f64),
        "DECIMAL" | "NUMERIC" => decode!(BigDecimal, |v: BigDecimal| Value::String(v.to_string())),
        "TIMESTAMP" => decode!(DateTime<Utc>, |v: DateTime<Utc>| Value::String(
            v.to_rfc3339()
        )),
        "DATETIME" => decode!(NaiveDateTime, |v: NaiveDateTime| Value::String(
            v.to_string()
        )),
        "DATE" => decode!(NaiveDate, |v: NaiveDate| Value::String(v.to_string())),
        "TIME" => decode!(NaiveTime, |v: NaiveTime| Value::String(v.to_string())),
        "JSON" => decode!(Value, |v| v),
        // BLOB/BINARY families: hex-encode for display.
        "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" | "BINARY" | "VARBINARY" => {
            decode!(Vec<u8>, |v: Vec<u8>| Value::String(bytes_hex(&v)))
        }
        _ => {}
    }

    // Text-family types (char/varchar/text/enum/set, etc.).
    decode!(String, to_str);

    // Last resort: decode as raw bytes (public decode path). Surface UTF-8 text
    // if valid, else a hex literal, so the cell is never silently blank.
    if let Ok(bytes) = row.try_get::<Vec<u8>, _>(i) {
        return match std::str::from_utf8(&bytes) {
            Ok(s) => Value::String(s.to_string()),
            Err(_) => Value::String(bytes_hex(&bytes)),
        };
    }
    Value::String(format!("<{type_name}>"))
}

/// Hex-encode bytes as an `0x…` literal for display.
fn bytes_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(2 + bytes.len() * 2);
    s.push_str("0x");
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
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

/// Decode percent-escapes in a URL path segment (enough to recover a database
/// name from a connection string).
fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

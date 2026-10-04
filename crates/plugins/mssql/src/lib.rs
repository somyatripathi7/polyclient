use async_trait::async_trait;
use bb8::Pool;
use bb8_tiberius::ConnectionManager;
use polyclient_core::{
    cfg_secret, ConfigField, ConfigFieldType, Connection, ConnectionConfig, Plugin, PluginError,
    PluginInfo, PluginKind, Result,
};
use polyclient_rdbms_common::{
    downcast_conn, ApplyResult, BrowseFilter, BrowseSpec, Column, ColumnMeta, ColumnValue,
    CsvWriter, ForeignKey, Index, QueryResult, RdbmsPlugin, RowChanges, Schema, Table,
    TableDescription, TableKind,
};
use std::sync::{Arc, RwLock};
use std::time::Instant;
use tiberius::{AuthMethod, ColumnType, Config, EncryptionLevel, Row};

type MssqlPool = Pool<ConnectionManager>;

pub struct MssqlPlugin;

impl MssqlPlugin {
    pub fn new() -> Self {
        Self
    }

    /// Index metadata for the structure view. Helper for
    /// [`describe_table`](RdbmsPlugin::describe_table) when `include_indexes` is set.
    async fn list_indexes(
        &self,
        conn: &MssqlConnection,
        schema: &str,
        table: &str,
    ) -> Result<Vec<Index>> {
        // One row per index column; the query orders by index then key ordinal,
        // so we aggregate consecutive rows into one Index.
        let sql = format!(
            "SELECT i.name AS index_name, col.name AS column_name, \
                    i.is_unique, i.is_primary_key, i.type_desc \
             FROM sys.indexes i \
             JOIN sys.objects o ON o.object_id = i.object_id \
             JOIN sys.schemas s ON s.schema_id = o.schema_id \
             JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
             JOIN sys.columns col ON col.object_id = ic.object_id AND col.column_id = ic.column_id \
             WHERE s.name = {sch} AND o.name = {tab} AND i.name IS NOT NULL \
             ORDER BY i.is_primary_key DESC, i.name, ic.key_ordinal",
            sch = quote_literal(schema),
            tab = quote_literal(table),
        );
        let rows = simple_rows(&conn.pool(), &sql).await?;

        let mut out: Vec<Index> = Vec::new();
        for r in &rows {
            let Some(name) = r.try_get::<&str, _>(0).ok().flatten() else {
                continue;
            };
            let Some(column) = r.try_get::<&str, _>(1).ok().flatten() else {
                continue;
            };
            let unique = r.try_get::<bool, _>(2).ok().flatten().unwrap_or(false);
            let primary = r.try_get::<bool, _>(3).ok().flatten().unwrap_or(false);
            let method = r
                .try_get::<&str, _>(4)
                .ok()
                .flatten()
                .unwrap_or("")
                .to_string();
            if let Some(last) = out.last_mut().filter(|ix| ix.name == name) {
                last.columns.push(column.to_string());
            } else {
                out.push(Index {
                    name: name.to_string(),
                    method,
                    unique,
                    primary,
                    columns: vec![column.to_string()],
                });
            }
        }
        Ok(out)
    }
}

impl Default for MssqlPlugin {
    fn default() -> Self {
        Self::new()
    }
}

/// A live connection to one database on a SQL Server instance.
///
/// The bb8 pool is bound to a single database (so `sys.*` / `INFORMATION_SCHEMA`
/// queries resolve against it); switching databases rebuilds the pool. It sits
/// behind a lock so the active database can be switched in place without
/// changing the connection's id. `config` keeps the original host/credentials so
/// a new pool can be built for another database.
pub struct MssqlConnection {
    pool: RwLock<MssqlPool>,
    config: ConnectionConfig,
}

impl MssqlConnection {
    /// A cheap clone of the current pool (bb8 pools are `Arc`-backed). The lock
    /// is released before the returned pool is awaited on, so a concurrent
    /// `use_database` swap never blocks in-flight queries.
    fn pool(&self) -> MssqlPool {
        self.pool.read().unwrap().clone()
    }
}

impl Connection for MssqlConnection {
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

fn cfg_bool(cfg: &ConnectionConfig, key: &str, default: bool) -> bool {
    cfg.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
}

/// Cap on rows returned per query. Bounds memory use for large result sets;
/// the result is flagged as truncated past this count.
const DEFAULT_MAX_ROW_COUNT: usize = 10_000;

/// Build a tiberius [`Config`] for a given database. The database is passed
/// explicitly (rather than read from `cfg`) so the same credentials can be
/// reused to connect to a different database when switching.
fn build_config(cfg: &ConnectionConfig, database: &str) -> Result<Config> {
    let host = cfg_str(cfg, "host")?;
    let port = cfg_u16(cfg, "port", 1433);
    let user = cfg_str(cfg, "user")?;
    let password = cfg_secret(cfg, "password")?.unwrap_or_default();
    let trust_cert = cfg_bool(cfg, "trust_cert", true);
    let encrypt = cfg_str_opt(cfg, "encrypt").unwrap_or_else(|| "required".into());

    let mut config = Config::new();
    config.host(host);
    config.port(port);
    config.database(database);
    config.authentication(AuthMethod::sql_server(user, password));
    config.encryption(match encrypt.as_str() {
        "off" => EncryptionLevel::Off,
        "on" => EncryptionLevel::On,
        "not_supported" => EncryptionLevel::NotSupported,
        _ => EncryptionLevel::Required,
    });
    if trust_cert {
        config.trust_cert();
    }
    Ok(config)
}

async fn make_pool(config: Config) -> Result<MssqlPool> {
    let mgr =
        ConnectionManager::build(config).map_err(|e| PluginError::Connection(e.to_string()))?;
    Pool::builder()
        .max_size(8)
        .build(mgr)
        .await
        .map_err(|e| PluginError::Connection(e.to_string()))
}

/// Run a parameterless query and return all rows of its first result set.
async fn simple_rows(pool: &MssqlPool, sql: &str) -> Result<Vec<Row>> {
    let mut conn = pool
        .get()
        .await
        .map_err(|e| PluginError::Backend(e.to_string()))?;
    let rows = conn
        .simple_query(sql.to_string())
        .await
        .map_err(|e| PluginError::Backend(e.to_string()))?
        .into_first_result()
        .await
        .map_err(|e| PluginError::Backend(e.to_string()))?;
    Ok(rows)
}

#[async_trait]
impl Plugin for MssqlPlugin {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            id: "mssql".into(),
            name: "SQL Server".into(),
            kind: PluginKind::Rdbms,
            version: env!("CARGO_PKG_VERSION").into(),
            description: "Connect to Microsoft SQL Server 2012+ and Azure SQL.".into(),
            ui_module: Some("rdbms".into()),
            protocol_version: polyclient_core::PROTOCOL_VERSION,
            config_schema: vec![
                ConfigField {
                    key: "host".into(),
                    label: "Host".into(),
                    field_type: ConfigFieldType::Text,
                    required: true,
                    default: Some(serde_json::json!("localhost")),
                    placeholder: Some("localhost".into()),
                    show_if: None,
                },
                ConfigField {
                    key: "port".into(),
                    label: "Port".into(),
                    field_type: ConfigFieldType::Number,
                    required: true,
                    default: Some(serde_json::json!(1433)),
                    placeholder: None,
                    show_if: None,
                },
                ConfigField {
                    key: "database".into(),
                    label: "Database".into(),
                    field_type: ConfigFieldType::Text,
                    required: true,
                    default: Some(serde_json::json!("master")),
                    placeholder: Some("master".into()),
                    show_if: None,
                },
                ConfigField {
                    key: "user".into(),
                    label: "User".into(),
                    field_type: ConfigFieldType::Text,
                    required: true,
                    default: Some(serde_json::json!("sa")),
                    placeholder: Some("sa".into()),
                    show_if: None,
                },
                ConfigField {
                    key: "password".into(),
                    label: "Password".into(),
                    field_type: ConfigFieldType::Password,
                    required: false,
                    default: None,
                    placeholder: None,
                    show_if: None,
                },
                ConfigField {
                    key: "encrypt".into(),
                    label: "Encryption".into(),
                    field_type: ConfigFieldType::Select {
                        options: vec![
                            "required".into(),
                            "on".into(),
                            "off".into(),
                            "not_supported".into(),
                        ],
                    },
                    required: false,
                    default: Some(serde_json::json!("required")),
                    placeholder: None,
                    show_if: None,
                },
                ConfigField {
                    key: "trust_cert".into(),
                    label: "Trust server certificate".into(),
                    field_type: ConfigFieldType::Boolean,
                    required: false,
                    default: Some(serde_json::json!(true)),
                    placeholder: None,
                    show_if: None,
                },
            ],
        }
    }

    async fn connect(&self, cfg: ConnectionConfig) -> Result<Arc<dyn Connection>> {
        let database = cfg_str(&cfg, "database")?;
        // Never log credentials. Log only safe fields.
        tracing::info!(
            "connecting to mssql host={} port={} db={}",
            cfg_str_opt(&cfg, "host").as_deref().unwrap_or("?"),
            cfg.get("port")
                .map(|v| v.to_string())
                .as_deref()
                .unwrap_or("?"),
            database
        );
        let config = build_config(&cfg, &database)?;
        let pool = make_pool(config).await.inspect_err(|e| {
            tracing::warn!("mssql connection failed: {e}");
        })?;
        // bb8 builds connections lazily; force one now so connect() fails fast
        // on bad credentials rather than on the first query.
        pool.get()
            .await
            .map_err(|e| PluginError::Connection(e.to_string()))?;
        tracing::info!("mssql pool established");
        Ok(Arc::new(MssqlConnection {
            pool: RwLock::new(pool),
            config: cfg,
        }))
    }
}

#[async_trait]
impl RdbmsPlugin for MssqlPlugin {
    async fn list_schemas(&self, conn: Arc<dyn Connection>) -> Result<Vec<Schema>> {
        let conn = downcast_conn::<MssqlConnection>(&conn)?;
        let rows = simple_rows(
            &conn.pool(),
            "SELECT s.name FROM sys.schemas s \
             WHERE s.name NOT IN \
               ('sys','INFORMATION_SCHEMA','guest','db_owner','db_accessadmin', \
                'db_securityadmin','db_ddladmin','db_backupoperator','db_datareader', \
                'db_datawriter','db_denydatareader','db_denydatawriter') \
             ORDER BY s.name",
        )
        .await?;
        Ok(rows
            .iter()
            .filter_map(|r| r.try_get::<&str, _>(0).ok().flatten())
            .map(|n| Schema {
                name: n.to_string(),
            })
            .collect())
    }

    async fn list_databases(&self, conn: Arc<dyn Connection>) -> Result<Vec<String>> {
        let conn = downcast_conn::<MssqlConnection>(&conn)?;
        let rows = simple_rows(
            &conn.pool(),
            "SELECT name FROM sys.databases \
             WHERE name NOT IN ('master','tempdb','model','msdb') \
             ORDER BY name",
        )
        .await?;
        Ok(rows
            .iter()
            .filter_map(|r| r.try_get::<&str, _>(0).ok().flatten())
            .map(str::to_string)
            .collect())
    }

    async fn use_database(&self, conn: Arc<dyn Connection>, database: &str) -> Result<()> {
        let conn = downcast_conn::<MssqlConnection>(&conn)?;
        tracing::info!("switching database to '{database}' (rebuilding pool)");
        // Build and validate the new pool before swapping, so a failed switch
        // leaves the existing connection untouched.
        let config = build_config(&conn.config, database)?;
        let pool = make_pool(config).await?;
        pool.get()
            .await
            .map_err(|e| PluginError::Connection(e.to_string()))?;
        *conn.pool.write().unwrap() = pool;
        Ok(())
    }

    async fn list_tables(&self, conn: Arc<dyn Connection>, schema: &str) -> Result<Vec<Table>> {
        let conn = downcast_conn::<MssqlConnection>(&conn)?;
        let sql = format!(
            "SELECT t.name, 'BASE TABLE' AS kind FROM sys.tables t \
             JOIN sys.schemas s ON s.schema_id = t.schema_id WHERE s.name = {sch} \
             UNION ALL \
             SELECT v.name, 'VIEW' FROM sys.views v \
             JOIN sys.schemas s ON s.schema_id = v.schema_id WHERE s.name = {sch} \
             ORDER BY 1",
            sch = quote_literal(schema)
        );
        let rows = simple_rows(&conn.pool(), &sql).await?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let name = r.try_get::<&str, _>(0).ok().flatten()?;
                let kind = r
                    .try_get::<&str, _>(1)
                    .ok()
                    .flatten()
                    .unwrap_or("BASE TABLE");
                Some(Table {
                    schema: schema.to_string(),
                    name: name.to_string(),
                    kind: match kind {
                        "VIEW" => TableKind::View,
                        _ => TableKind::Table,
                    },
                })
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
        let conn = downcast_conn::<MssqlConnection>(&conn)?;
        // sys.* catalog gives exact types, nullability, defaults; join in
        // PK/unique (from key constraints) and FK targets.
        let sql = format!(
            "SELECT
               c.name AS column_name,
               ty.name AS data_type,
               c.max_length,
               c.precision,
               c.scale,
               c.is_nullable,
               dc.definition AS column_default,
               CASE WHEN pk.column_id IS NOT NULL THEN 1 ELSE 0 END AS is_pk,
               CASE WHEN uq.column_id IS NOT NULL THEN 1 ELSE 0 END AS is_unique,
               fk_tab.name AS fk_table,
               fk_col.name AS fk_column
             FROM sys.columns c
             JOIN sys.objects o ON o.object_id = c.object_id
             JOIN sys.schemas s ON s.schema_id = o.schema_id
             JOIN sys.types ty ON ty.user_type_id = c.user_type_id
             LEFT JOIN sys.default_constraints dc ON dc.object_id = c.default_object_id
             LEFT JOIN (
               SELECT ic.object_id, ic.column_id
               FROM sys.indexes i
               JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id
               WHERE i.is_primary_key = 1
             ) pk ON pk.object_id = c.object_id AND pk.column_id = c.column_id
             LEFT JOIN (
               SELECT ic.object_id, ic.column_id
               FROM sys.indexes i
               JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id
               WHERE i.is_unique_constraint = 1
             ) uq ON uq.object_id = c.object_id AND uq.column_id = c.column_id
             LEFT JOIN sys.foreign_key_columns fkc
               ON fkc.parent_object_id = c.object_id AND fkc.parent_column_id = c.column_id
             LEFT JOIN sys.tables fk_tab ON fk_tab.object_id = fkc.referenced_object_id
             LEFT JOIN sys.columns fk_col
               ON fk_col.object_id = fkc.referenced_object_id AND fk_col.column_id = fkc.referenced_column_id
             WHERE s.name = {sch} AND o.name = {tab}
             ORDER BY c.column_id",
            sch = quote_literal(schema),
            tab = quote_literal(table),
        );
        let rows = simple_rows(&conn.pool(), &sql).await?;

        let columns: Vec<Column> = rows
            .iter()
            .filter_map(|r| {
                let name = r.try_get::<&str, _>(0).ok().flatten()?.to_string();
                let dtype = r
                    .try_get::<&str, _>(1)
                    .ok()
                    .flatten()
                    .unwrap_or("")
                    .to_string();
                let max_length = r.try_get::<i16, _>(2).ok().flatten();
                let precision = r.try_get::<u8, _>(3).ok().flatten();
                let scale = r.try_get::<u8, _>(4).ok().flatten();
                let nullable = r.try_get::<bool, _>(5).ok().flatten().unwrap_or(true);
                let default_value = r.try_get::<&str, _>(6).ok().flatten().map(str::to_string);
                let is_pk = r.try_get::<i32, _>(7).ok().flatten().unwrap_or(0) != 0;
                let is_unique = r.try_get::<i32, _>(8).ok().flatten().unwrap_or(0) != 0;
                let fk_table = r.try_get::<&str, _>(9).ok().flatten().map(str::to_string);
                let fk_column = r.try_get::<&str, _>(10).ok().flatten().map(str::to_string);

                let d = dtype.to_ascii_lowercase();
                // SQL Server stores JSON in nvarchar; there is no native JSON type.
                let json = false;
                let large = d.contains("text") || d == "xml" || d == "image";
                // nvarchar/nchar max_length is in bytes (2 per char); -1 means MAX.
                let char_max_length = match d.as_str() {
                    "nvarchar" | "nchar" => {
                        max_length.map(|n| if n < 0 { -1 } else { (n / 2) as i32 })
                    }
                    "varchar" | "char" | "varbinary" | "binary" => max_length.map(|n| n as i32),
                    _ => None,
                };
                let foreign_key = fk_table
                    .zip(fk_column)
                    .map(|(table, column)| ForeignKey { table, column });
                Some(Column {
                    name,
                    data_type: dtype,
                    udt_name: None,
                    nullable,
                    primary_key: is_pk,
                    unique: is_unique,
                    default_value,
                    foreign_key,
                    char_max_length,
                    numeric_precision: precision.map(|n| n as i32),
                    numeric_scale: scale.map(|n| n as i32),
                    json,
                    large,
                })
            })
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
        // SQL Server has no SHOW CREATE TABLE; reconstruct from describe_table
        // (column types, nullability, primary key).
        let columns = self
            .describe_table(conn.clone(), schema, table, false)
            .await?
            .columns;
        if columns.is_empty() {
            return Err(PluginError::Backend(format!(
                "table {schema}.{table} not found or has no columns"
            )));
        }

        let mut lines: Vec<String> = columns
            .iter()
            .map(|c| {
                let null = if c.nullable { " NULL" } else { " NOT NULL" };
                format!("  {} {}{}", quote_ident(&c.name), render_type(c), null)
            })
            .collect();

        let pks: Vec<String> = columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| quote_ident(&c.name))
            .collect();
        if !pks.is_empty() {
            lines.push(format!("  PRIMARY KEY ({})", pks.join(", ")));
        }

        Ok(format!(
            "-- Generated DDL statements -- \n\n\n\
            -- Table: {schema}.{table} Definition -- \n\n\
            CREATE TABLE {}.{} (\n{}\n);",
            quote_ident(schema),
            quote_ident(table),
            lines.join(",\n")
        ))
    }

    async fn browse_table(
        &self,
        conn: Arc<dyn Connection>,
        schema: &str,
        table: &str,
        spec: BrowseSpec,
    ) -> Result<QueryResult> {
        let conn = downcast_conn::<MssqlConnection>(&conn)?;
        let sql = build_browse(schema, table, &spec);
        let start = Instant::now();
        let rows = simple_rows(&conn.pool(), &sql).await?;
        let (columns, data) = rows_to_json(&rows);
        Ok(QueryResult {
            columns,
            rows: data,
            rows_affected: None,
            elapsed_ms: start.elapsed().as_millis(),
            result_truncated: false,
            sql: Some(sql),
        })
    }

    async fn execute(&self, conn: Arc<dyn Connection>, sql: &str) -> Result<Vec<QueryResult>> {
        let conn = downcast_conn::<MssqlConnection>(&conn)?;
        let pool = conn.pool();

        let mut results = Vec::new();
        for stmt in split_statements(sql) {
            let start = Instant::now();
            if is_select(&stmt) {
                let mut client = pool
                    .get()
                    .await
                    .map_err(|e| PluginError::Backend(e.to_string()))?;
                let rows = client
                    .simple_query(stmt.clone())
                    .await
                    .map_err(|e| {
                        tracing::warn!("mssql select failed: {e}");
                        PluginError::Backend(e.to_string())
                    })?
                    .into_first_result()
                    .await
                    .map_err(|e| PluginError::Backend(e.to_string()))?;
                drop(client);

                let (columns, mut data) = rows_to_json(&rows);
                let result_truncated = data.len() > DEFAULT_MAX_ROW_COUNT;
                data.truncate(DEFAULT_MAX_ROW_COUNT);
                let elapsed_ms = start.elapsed().as_millis();
                tracing::debug!(
                    "select returned {} row(s){} in {elapsed_ms}ms",
                    data.len(),
                    if result_truncated { " (truncated)" } else { "" }
                );
                results.push(QueryResult {
                    columns,
                    rows: data,
                    rows_affected: None,
                    elapsed_ms,
                    result_truncated,
                    sql: Some(stmt.clone()),
                });
            } else {
                let mut client = pool
                    .get()
                    .await
                    .map_err(|e| PluginError::Backend(e.to_string()))?;
                let res = client.execute(stmt.clone(), &[]).await.map_err(|e| {
                    tracing::warn!("mssql statement failed: {e}");
                    PluginError::Backend(e.to_string())
                })?;
                let affected = res.total();
                let elapsed_ms = start.elapsed().as_millis();
                tracing::debug!("statement affected {affected} row(s) in {elapsed_ms}ms");
                results.push(QueryResult {
                    columns: Vec::new(),
                    rows: Vec::new(),
                    rows_affected: Some(affected),
                    elapsed_ms,
                    result_truncated: false,
                    sql: None,
                });
            }
        }
        Ok(results)
    }

    async fn export_csv(&self, conn: Arc<dyn Connection>, sql: &str, path: &str) -> Result<u64> {
        let conn = downcast_conn::<MssqlConnection>(&conn)?;
        let rows = simple_rows(&conn.pool(), sql)
            .await
            .inspect_err(|e| tracing::warn!("mssql export query failed: {e}"))?;
        let (columns, data) = rows_to_json(&rows);

        let mut writer = CsvWriter::create(path, &columns)?;
        let mut count: u64 = 0;
        for row in &data {
            writer.write_row(row)?;
            count += 1;
        }
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
        let conn = downcast_conn::<MssqlConnection>(&conn)?;
        let pool = conn.pool();
        let mut client = pool
            .get()
            .await
            .map_err(|e| PluginError::Backend(e.to_string()))?;

        // bb8 hands out one connection; run the batch inside an explicit
        // transaction on it and roll back on the first failure.
        client
            .simple_query("BEGIN TRANSACTION")
            .await
            .map_err(|e| PluginError::Backend(e.to_string()))?;

        let outcome = async {
            let mut result = ApplyResult::default();
            for u in &changes.updates {
                result.updated += run_dml(
                    &mut client,
                    &build_update(schema, table, &u.pk, &u.changes)?,
                )
                .await?;
            }
            for values in &changes.inserts {
                result.inserted +=
                    run_dml(&mut client, &build_insert(schema, table, values)?).await?;
            }
            for pk in &changes.deletes {
                result.deleted += run_dml(&mut client, &build_delete(schema, table, pk)?).await?;
            }
            Ok::<_, PluginError>(result)
        }
        .await;

        match outcome {
            Ok(result) => {
                client
                    .simple_query("COMMIT TRANSACTION")
                    .await
                    .map_err(|e| PluginError::Backend(e.to_string()))?;
                Ok(result)
            }
            Err(e) => {
                let _ = client.simple_query("ROLLBACK TRANSACTION").await;
                Err(e)
            }
        }
    }
}

/// Run a single DML statement on a pooled client, returning affected rows.
async fn run_dml(
    client: &mut bb8::PooledConnection<'_, ConnectionManager>,
    sql: &str,
) -> Result<u64> {
    let res = client
        .execute(sql.to_string(), &[])
        .await
        .map_err(|e| PluginError::Backend(e.to_string()))?;
    Ok(res.total())
}

fn build_update(
    schema: &str,
    table: &str,
    pk: &[ColumnValue],
    changes: &[ColumnValue],
) -> Result<String> {
    if changes.is_empty() {
        return Err(PluginError::Config("no columns to update".into()));
    }
    let set_parts: Vec<String> = changes
        .iter()
        .map(|cv| format!("{} = {}", quote_ident(&cv.column), value_expr(cv)))
        .collect();
    Ok(format!(
        "UPDATE {}.{} SET {} WHERE {}",
        quote_ident(schema),
        quote_ident(table),
        set_parts.join(", "),
        build_where(pk)?,
    ))
}

fn build_insert(schema: &str, table: &str, values: &[ColumnValue]) -> Result<String> {
    if values.is_empty() {
        return Ok(format!(
            "INSERT INTO {}.{} DEFAULT VALUES",
            quote_ident(schema),
            quote_ident(table),
        ));
    }
    let cols: Vec<String> = values.iter().map(|cv| quote_ident(&cv.column)).collect();
    let vals: Vec<String> = values.iter().map(value_expr).collect();
    Ok(format!(
        "INSERT INTO {}.{} ({}) VALUES ({})",
        quote_ident(schema),
        quote_ident(table),
        cols.join(", "),
        vals.join(", "),
    ))
}

fn build_delete(schema: &str, table: &str, pk: &[ColumnValue]) -> Result<String> {
    Ok(format!(
        "DELETE FROM {}.{} WHERE {}",
        quote_ident(schema),
        quote_ident(table),
        build_where(pk)?,
    ))
}

/// Build a browse `SELECT * FROM [s].[t] [WHERE ...] ORDER BY ...
/// OFFSET n ROWS FETCH NEXT m ROWS ONLY`. SQL Server requires an `ORDER BY` for
/// `OFFSET/FETCH`; when the caller gave no sort we use the stable no-op
/// `ORDER BY (SELECT NULL)`.
fn build_browse(schema: &str, table: &str, spec: &BrowseSpec) -> String {
    let mut conds: Vec<String> = Vec::new();
    for f in &spec.filters {
        if let Some(c) = filter_expr(f) {
            conds.push(c);
        }
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
    if spec.sorts.is_empty() {
        sql.push_str(" ORDER BY (SELECT NULL)");
    } else {
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
    // limit/offset are u32, safe to interpolate directly.
    sql.push_str(&format!(
        " OFFSET {} ROWS FETCH NEXT {} ROWS ONLY",
        spec.offset, spec.limit
    ));
    sql
}

/// Render one structured filter as a SQL condition. SQL Server has no `ILIKE`;
/// `like`/`ilike` both map to `LIKE`, which is case-insensitive under the common
/// `*_CI_*` collations.
fn filter_expr(f: &BrowseFilter) -> Option<String> {
    let col = quote_ident(&f.column);
    match f.op.as_str() {
        "is_null" => Some(format!("{col} IS NULL")),
        "is_not_null" => Some(format!("{col} IS NOT NULL")),
        op => {
            let sql_op = match op {
                "eq" => "=",
                "ne" => "<>",
                "lt" => "<",
                "lte" => "<=",
                "gt" => ">",
                "gte" => ">=",
                "like" | "ilike" => "LIKE",
                _ => return None,
            };
            let cv = ColumnValue {
                column: f.column.clone(),
                type_name: f.type_name.clone(),
                value: f.value.clone(),
            };
            Some(format!("{col} {sql_op} {}", value_expr(&cv)))
        }
    }
}

/// Quote a SQL Server identifier with brackets, escaping `]` as `]]`.
fn quote_ident(s: &str) -> String {
    format!("[{}]", s.replace(']', "]]"))
}

/// Render a value as a single-quoted (Unicode) SQL string literal, doubling `'`.
/// SQL Server implicitly converts the literal to the target column type.
fn quote_literal(s: &str) -> String {
    format!("N'{}'", s.replace('\'', "''"))
}

/// Render the SQL expression for a column value: `NULL`, a bare numeric/bool, or
/// a quoted `N'...'` literal. tiberius parameter binding requires typed `ToSql`
/// values; inline (escaped) literals keep the edit/browse path dialect-simple
/// and match how the value arrives (already validated against the row's type by
/// the UI).
fn value_expr(cv: &ColumnValue) -> String {
    if cv.value.is_null() {
        return "NULL".into();
    }
    match &cv.value {
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => {
            if *b {
                "1".into()
            } else {
                "0".into()
            }
        }
        serde_json::Value::String(s) => quote_literal(s),
        other => quote_literal(&other.to_string()),
    }
}

fn build_where(key: &[ColumnValue]) -> Result<String> {
    if key.is_empty() {
        return Err(PluginError::Config(
            "cannot identify row: no key columns".into(),
        ));
    }
    let parts: Vec<String> = key
        .iter()
        .map(|cv| {
            if cv.value.is_null() {
                format!("{} IS NULL", quote_ident(&cv.column))
            } else {
                format!("{} = {}", quote_ident(&cv.column), value_expr(cv))
            }
        })
        .collect();
    Ok(parts.join(" AND "))
}

fn is_select(sql: &str) -> bool {
    let head = sql
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(head.as_str(), "select" | "with")
}

/// Convert a slice of rows into column metadata + a JSON cell matrix. Column
/// metadata comes from the first row (tiberius carries it per row).
fn rows_to_json(rows: &[Row]) -> (Vec<ColumnMeta>, Vec<Vec<serde_json::Value>>) {
    let columns: Vec<ColumnMeta> = match rows.first() {
        Some(first) => first
            .columns()
            .iter()
            .map(|c| ColumnMeta {
                name: c.name().to_string(),
                data_type: format!("{:?}", c.column_type()),
            })
            .collect(),
        None => Vec::new(),
    };
    let types: Vec<ColumnType> = rows
        .first()
        .map(|r| r.columns().iter().map(|c| c.column_type()).collect())
        .unwrap_or_default();
    let data = rows
        .iter()
        .map(|row| {
            (0..types.len())
                .map(|i| cell_to_json(row, i, types[i]))
                .collect()
        })
        .collect();
    (columns, data)
}

/// Convert one cell to JSON, decoding by the column's tiberius [`ColumnType`].
/// A SQL `NULL` decodes as `Ok(None)` and maps to `Value::Null`. Unknown or
/// failed decodes fall back to a string so a cell is never silently blank.
fn cell_to_json(row: &Row, i: usize, ct: ColumnType) -> serde_json::Value {
    use serde_json::Value;

    let num_f64 = |v: f64| {
        serde_json::Number::from_f64(v)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    };

    match ct {
        ColumnType::Bit | ColumnType::Bitn => row
            .try_get::<bool, _>(i)
            .ok()
            .flatten()
            .map(Value::Bool)
            .unwrap_or(Value::Null),
        ColumnType::Int1 => row
            .try_get::<u8, _>(i)
            .ok()
            .flatten()
            .map(|v| Value::Number(v.into()))
            .unwrap_or(Value::Null),
        ColumnType::Int2 => row
            .try_get::<i16, _>(i)
            .ok()
            .flatten()
            .map(|v| Value::Number(v.into()))
            .unwrap_or(Value::Null),
        ColumnType::Int4 | ColumnType::Intn => row
            .try_get::<i32, _>(i)
            .ok()
            .flatten()
            .map(|v| Value::Number(v.into()))
            .or_else(|| {
                // Intn covers tinyint..bigint; retry wider on a width mismatch.
                row.try_get::<i64, _>(i)
                    .ok()
                    .flatten()
                    .map(|v| Value::Number(v.into()))
            })
            .unwrap_or(Value::Null),
        ColumnType::Int8 => row
            .try_get::<i64, _>(i)
            .ok()
            .flatten()
            .map(|v| Value::Number(v.into()))
            .unwrap_or(Value::Null),
        ColumnType::Float4 => row
            .try_get::<f32, _>(i)
            .ok()
            .flatten()
            .map(|v| num_f64(v as f64))
            .unwrap_or(Value::Null),
        ColumnType::Float8 | ColumnType::Floatn => row
            .try_get::<f64, _>(i)
            .ok()
            .flatten()
            .map(num_f64)
            .unwrap_or(Value::Null),
        ColumnType::Money | ColumnType::Money4 | ColumnType::Decimaln | ColumnType::Numericn => row
            .try_get::<rust_decimal::Decimal, _>(i)
            .ok()
            .flatten()
            .map(|d| Value::String(d.to_string()))
            .unwrap_or(Value::Null),
        ColumnType::Guid => row
            .try_get::<uuid::Uuid, _>(i)
            .ok()
            .flatten()
            .map(|g| Value::String(g.to_string()))
            .unwrap_or(Value::Null),
        ColumnType::Daten => row
            .try_get::<chrono::NaiveDate, _>(i)
            .ok()
            .flatten()
            .map(|d| Value::String(d.to_string()))
            .unwrap_or(Value::Null),
        ColumnType::Timen => row
            .try_get::<chrono::NaiveTime, _>(i)
            .ok()
            .flatten()
            .map(|t| Value::String(t.to_string()))
            .unwrap_or(Value::Null),
        ColumnType::Datetime
        | ColumnType::Datetime4
        | ColumnType::Datetimen
        | ColumnType::Datetime2 => row
            .try_get::<chrono::NaiveDateTime, _>(i)
            .ok()
            .flatten()
            .map(|t| Value::String(t.to_string()))
            .unwrap_or(Value::Null),
        ColumnType::DatetimeOffsetn => row
            .try_get::<chrono::DateTime<chrono::FixedOffset>, _>(i)
            .ok()
            .flatten()
            .map(|t| Value::String(t.to_rfc3339()))
            .unwrap_or(Value::Null),
        ColumnType::BigBinary | ColumnType::BigVarBin | ColumnType::Image => row
            .try_get::<&[u8], _>(i)
            .ok()
            .flatten()
            .map(|b| Value::String(bytes_hex(b)))
            .unwrap_or(Value::Null),
        // Text-ish: NVarchar, NChar, NText, BigVarChar, BigChar, Text, Xml, Udt,
        // SSVariant, Null.
        _ => row
            .try_get::<&str, _>(i)
            .ok()
            .flatten()
            .map(|s| Value::String(s.to_string()))
            .unwrap_or(Value::Null),
    }
}

/// Render a column's SQL Server type for DDL, reattaching length/precision.
fn render_type(c: &Column) -> String {
    let t = c.data_type.to_ascii_lowercase();
    match t.as_str() {
        "varchar" | "char" | "varbinary" | "binary" | "nvarchar" | "nchar" => {
            match c.char_max_length {
                Some(-1) => format!("{}(max)", c.data_type),
                Some(n) => format!("{}({})", c.data_type, n),
                None => c.data_type.clone(),
            }
        }
        "decimal" | "numeric" => match (c.numeric_precision, c.numeric_scale) {
            (Some(p), Some(s)) => format!("{}({},{})", c.data_type, p, s),
            _ => c.data_type.clone(),
        },
        _ => c.data_type.clone(),
    }
}

/// Hex-encode bytes as a `0x…` literal for display.
fn bytes_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(2 + bytes.len() * 2);
    s.push_str("0x");
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Split a T-SQL script into statements on top-level semicolons, ignoring `;`
/// inside single-quoted strings, bracket-quoted identifiers, and `--`/`/* */`
/// comments. T-SQL strings escape a quote by doubling it; block comments nest.
fn split_statements(sql: &str) -> Vec<String> {
    let b = sql.as_bytes();
    let n = b.len();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < n {
        match b[i] {
            b'\'' => {
                i += 1;
                while i < n {
                    if b[i] == b'\'' {
                        if i + 1 < n && b[i + 1] == b'\'' {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'[' => {
                // Bracket-quoted identifier; `]]` escapes a literal `]`.
                i += 1;
                while i < n {
                    if b[i] == b']' {
                        if i + 1 < n && b[i + 1] == b']' {
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
                let mut depth = 1; // T-SQL block comments nest.
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

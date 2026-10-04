use async_trait::async_trait;
use polyclient_core::{
    cfg_secret, ConfigField, ConfigFieldType, Connection, ConnectionConfig, Plugin, PluginError,
    PluginInfo, PluginKind, Result, ShowIf,
};
use polyclient_rdbms_common::{
    downcast_conn, ApplyResult, BrowseFilter, BrowseSpec, Column, ColumnMeta, ColumnValue,
    CsvWriter, QueryResult, RdbmsPlugin, RowChanges, Schema, Table, TableDescription, TableKind,
};
use regex::Regex;
use snowflake_connector_rs::{
    AuthConfig, CellValue, Client, ClientConfig, KeyPairConfig, QueryConfig, Session, SessionConfig,
};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

pub struct SnowflakePlugin;

impl SnowflakePlugin {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SnowflakePlugin {
    fn default() -> Self {
        Self::new()
    }
}

/// A live connection to one database on a Snowflake account.
///
/// Snowflake's REST API is reached through a [`Client`] (cheap to clone)
/// that mints session tokens. The active session sits behind a lock so the
/// current database can be switched in place — re-logging in against the new
/// database — without changing the connection's id. A session is cloned out
/// under the read lock before any query awaits, so a concurrent database swap
/// never blocks in-flight queries.
pub struct SnowflakeConnection {
    session: RwLock<Arc<Session>>,
    config: ConnectionConfig,
}

impl SnowflakeConnection {
    fn session(&self) -> Arc<Session> {
        self.session.read().unwrap().clone()
    }
}

impl Connection for SnowflakeConnection {
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

const DEFAULT_MAX_ROW_COUNT: usize = 10_000;

/// Build the auth method described by the connection config. Supports
/// password auth and key-pair auth (a PEM private key file, optionally
/// passphrase-encrypted).
fn build_auth(cfg: &ConnectionConfig) -> Result<AuthConfig> {
    let mode = cfg_str_opt(cfg, "auth_mode").unwrap_or_else(|| "password".into());
    match mode.as_str() {
        "keypair" => {
            let path = cfg_str(cfg, "private_key")?;
            let pem = std::fs::read_to_string(&path)
                .map_err(|e| PluginError::Config(format!("cannot read private key {path}: {e}")))?;
            match cfg_secret(cfg, "private_key_passphrase")?.filter(|p| !p.is_empty()) {
                Some(pass) => Ok(AuthConfig::key_pair(KeyPairConfig::from_encrypted_pem(
                    pem,
                    pass.into_bytes(),
                ))),
                None => Ok(AuthConfig::key_pair(KeyPairConfig::from_pem(pem))),
            }
        }
        _ => {
            let password = cfg_secret(cfg, "password")?.unwrap_or_default();
            Ok(AuthConfig::password(password))
        }
    }
}

/// Build a [`Client`] for a given database. The database is part of the
/// client config, so switching databases means building a fresh client + session.
fn build_client(cfg: &ConnectionConfig, database: &str) -> Result<Client> {
    let account = cfg_str(cfg, "account")?;
    let user = cfg_str(cfg, "user")?;
    let warehouse = cfg_str(cfg, "warehouse")?;
    let role = cfg_str_opt(cfg, "role").unwrap_or_else(|| "SYSADMIN".into());
    let schema = cfg_str(cfg, "schema")?;
    let auth = build_auth(cfg)?;

    let session = SessionConfig::new()
        .with_role(role)
        .with_warehouse(warehouse)
        .with_database(database)
        .with_schema(schema);
    let query = QueryConfig::new().with_query_response_timeout(Duration::from_secs(30));

    Client::new(
        ClientConfig::new(user, account, auth)
            .with_session(session)
            .with_query(query),
    )
    .map_err(|e| PluginError::Connection(e.to_string()))
}

async fn open_session(client: &Client) -> Result<Session> {
    client
        .create_session()
        .await
        .map_err(|e| PluginError::Connection(e.to_string()))
}

#[async_trait]
impl Plugin for SnowflakePlugin {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            id: "snowflake".into(),
            name: "Snowflake".into(),
            kind: PluginKind::Rdbms,
            version: env!("CARGO_PKG_VERSION").into(),
            description: "Connect to Snowflake data warehouse.".into(),
            ui_module: Some("rdbms".into()),
            protocol_version: polyclient_core::PROTOCOL_VERSION,
            config_schema: vec![
                ConfigField {
                    key: "account".into(),
                    label: "Account".into(),
                    field_type: ConfigFieldType::Text,
                    required: true,
                    default: None,
                    placeholder: Some("xy12345.us-east-1.azure".into()),
                    show_if: None,
                },
                ConfigField {
                    key: "user".into(),
                    label: "User".into(),
                    field_type: ConfigFieldType::Text,
                    required: true,
                    default: None,
                    placeholder: Some("admin".into()),
                    show_if: None,
                },
                ConfigField {
                    key: "auth_mode".into(),
                    label: "Authentication".into(),
                    field_type: ConfigFieldType::Select {
                        options: vec!["password".into(), "keypair".into()],
                    },
                    required: true,
                    default: Some(serde_json::json!("password")),
                    placeholder: None,
                    show_if: None,
                },
                ConfigField {
                    key: "password".into(),
                    label: "Password".into(),
                    field_type: ConfigFieldType::Password,
                    required: false,
                    default: None,
                    placeholder: None,
                    show_if: Some(ShowIf {
                        field: "auth_mode".into(),
                        equals: "password".into(),
                    }),
                },
                ConfigField {
                    key: "private_key".into(),
                    label: "Private key (PEM)".into(),
                    field_type: ConfigFieldType::FilePath,
                    required: false,
                    default: None,
                    placeholder: Some("~/.snowflake/rsa_key.p8".into()),
                    show_if: Some(ShowIf {
                        field: "auth_mode".into(),
                        equals: "keypair".into(),
                    }),
                },
                ConfigField {
                    key: "private_key_passphrase".into(),
                    label: "Private key passphrase".into(),
                    field_type: ConfigFieldType::Password,
                    required: false,
                    default: None,
                    placeholder: Some("(leave blank if key is unencrypted)".into()),
                    show_if: Some(ShowIf {
                        field: "auth_mode".into(),
                        equals: "keypair".into(),
                    }),
                },
                ConfigField {
                    key: "warehouse".into(),
                    label: "Warehouse".into(),
                    field_type: ConfigFieldType::Text,
                    required: true,
                    default: None,
                    placeholder: Some("COMPUTE_WH".into()),
                    show_if: None,
                },
                ConfigField {
                    key: "role".into(),
                    label: "Role".into(),
                    field_type: ConfigFieldType::Text,
                    required: false,
                    default: Some(serde_json::json!("SYSADMIN")),
                    placeholder: Some("SYSADMIN".into()),
                    show_if: None,
                },
                ConfigField {
                    key: "database".into(),
                    label: "Database".into(),
                    field_type: ConfigFieldType::Text,
                    required: true,
                    default: None,
                    placeholder: Some("SAMPLE_DB".into()),
                    show_if: None,
                },
                ConfigField {
                    key: "schema".into(),
                    label: "Schema".into(),
                    field_type: ConfigFieldType::Text,
                    required: true,
                    default: None,
                    placeholder: Some("PUBLIC".into()),
                    show_if: None,
                },
            ],
        }
    }

    async fn connect(&self, cfg: ConnectionConfig) -> Result<Arc<dyn Connection>> {
        let database = cfg_str(&cfg, "database")?;

        tracing::info!(
            "connecting to snowflake account={} warehouse={} db={}",
            cfg_str_opt(&cfg, "account").as_deref().unwrap_or("?"),
            cfg_str_opt(&cfg, "warehouse").as_deref().unwrap_or("?"),
            database
        );

        let client = build_client(&cfg, &database)?;
        let session = open_session(&client).await.inspect_err(|e| {
            tracing::warn!("snowflake connection failed: {e}");
        })?;

        tracing::info!("snowflake session established");
        Ok(Arc::new(SnowflakeConnection {
            session: RwLock::new(Arc::new(session)),
            config: cfg,
        }))
    }
}

#[async_trait]
impl RdbmsPlugin for SnowflakePlugin {
    async fn list_schemas(&self, conn: Arc<dyn Connection>) -> Result<Vec<Schema>> {
        let conn = downcast_conn::<SnowflakeConnection>(&conn)?;
        let (_, rows) = run_query(
            &conn.session(),
            "SELECT schema_name FROM information_schema.schemata \
             WHERE schema_name NOT IN ('INFORMATION_SCHEMA') \
             ORDER BY schema_name",
        )
        .await?;

        Ok(rows
            .into_iter()
            .filter_map(|r| r.into_iter().next())
            .map(|v| Schema {
                name: json_to_string(&v),
            })
            .collect())
    }

    async fn list_databases(&self, conn: Arc<dyn Connection>) -> Result<Vec<String>> {
        let conn = downcast_conn::<SnowflakeConnection>(&conn)?;
        let (cols, rows) = run_query(&conn.session(), "SHOW DATABASES").await?;

        // `SHOW DATABASES` returns many columns; the database name lives in the
        // "name" column.
        let name_idx = cols
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case("name"))
            .unwrap_or(1);

        Ok(rows
            .into_iter()
            .filter_map(|r| r.get(name_idx).map(json_to_string))
            .collect())
    }

    async fn use_database(&self, conn: Arc<dyn Connection>, database: &str) -> Result<()> {
        let conn = downcast_conn::<SnowflakeConnection>(&conn)?;
        tracing::info!("switching database to '{database}' (re-opening session)");

        let client = build_client(&conn.config, database)?;
        let session = open_session(&client).await?;
        *conn.session.write().unwrap() = Arc::new(session);
        Ok(())
    }

    async fn list_tables(&self, conn: Arc<dyn Connection>, schema: &str) -> Result<Vec<Table>> {
        let conn = downcast_conn::<SnowflakeConnection>(&conn)?;
        let sql = format!(
            "SELECT table_name, table_type FROM information_schema.tables \
             WHERE table_schema = {} ORDER BY table_name",
            quote_literal(schema)
        );
        let (_, rows) = run_query(&conn.session(), &sql).await?;

        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let name = r.first().map(json_to_string)?;
                let kind = r.get(1).map(json_to_string).unwrap_or_default();
                Some(Table {
                    schema: schema.to_string(),
                    name,
                    kind: match kind.to_ascii_uppercase().as_str() {
                        "VIEW" => TableKind::View,
                        "MATERIALIZED VIEW" => TableKind::MaterializedView,
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
        _include_indexes: bool,
    ) -> Result<TableDescription> {
        let conn = downcast_conn::<SnowflakeConnection>(&conn)?;
        let sql = format!("DESCRIBE TABLE {}.{}", schema, table,);
        let (_, rows) = run_query(&conn.session(), &sql).await?;

        let columns = rows
            .into_iter()
            .filter_map(|r| {
                let name = r.first().map(json_to_string)?;

                let dtype_temp = r.get(1).map(json_to_string).unwrap_or_default();
                // Parse VARCHAR(255), NUMBER(38,0), etc. from dtype.
                let (dtype, char_len, precision, scale) = parse_column_type_metadata(&dtype_temp);

                let nullable = r.get(3).map(json_to_string).unwrap_or_default();

                let default_value = match r.get(4) {
                    Some(serde_json::Value::String(s)) => Some(s.into()),
                    _ => None,
                };

                let primary_key = r.get(5).map(json_to_string).unwrap_or_default();

                let unique = r.get(6).map(json_to_string).unwrap_or_default();

                let u = dtype.to_ascii_lowercase();
                let json = u.contains("variant") || u.contains("json") || u.contains("object");

                let large = json || u.contains("text") || u.contains("array");

                Some(Column {
                    name,
                    data_type: dtype.into(),
                    udt_name: None,
                    nullable: nullable.eq_ignore_ascii_case("Y"),
                    primary_key: primary_key.eq_ignore_ascii_case("Y"),
                    unique: unique.eq_ignore_ascii_case("Y"),
                    default_value,
                    foreign_key: None,
                    char_max_length: char_len,
                    numeric_precision: precision,
                    numeric_scale: scale,
                    json,
                    large,
                })
            })
            .collect();

        // Snowflake has no traditional indexes, so indexes are always empty.
        Ok(TableDescription {
            columns,
            indexes: Vec::new(),
        })
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

        let mut lines: Vec<String> = columns
            .iter()
            .map(|c| {
                let null = if c.nullable { "" } else { " NOT NULL" };
                format!("  \"{}\" {}{}", c.name, c.data_type, null)
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

        let ddl = format!(
            "-- Generated DDL statements -- \n\n\n\
            -- Table: {schema}.{table} Definition -- \n\n\
            CREATE TABLE IF NOT EXISTS \"{schema}\".\"{table}\" (\n{}\n);",
            lines.join(",\n")
        );

        Ok(ddl)
    }

    async fn browse_table(
        &self,
        conn: Arc<dyn Connection>,
        schema: &str,
        table: &str,
        spec: BrowseSpec,
    ) -> Result<QueryResult> {
        let conn = downcast_conn::<SnowflakeConnection>(&conn)?;
        let sql = build_browse(schema, table, &spec)?;
        let start = Instant::now();
        let (columns, rows) = run_query(&conn.session(), &sql).await?;

        Ok(QueryResult {
            columns,
            rows,
            rows_affected: None,
            elapsed_ms: start.elapsed().as_millis(),
            result_truncated: false,
            sql: Some(sql),
        })
    }

    async fn execute(&self, conn: Arc<dyn Connection>, sql: &str) -> Result<Vec<QueryResult>> {
        let conn = downcast_conn::<SnowflakeConnection>(&conn)?;
        let session = conn.session();

        let mut results = Vec::new();
        for stmt in split_statements(sql) {
            let start = Instant::now();
            if is_select(&stmt) {
                let (columns, mut rows) = run_query(&session, add_limit_in_select(&stmt).as_str())
                    .await
                    .inspect_err(|e| {
                        tracing::warn!("snowflake select failed: {e}");
                    })?;
                let result_truncated = rows.len() > DEFAULT_MAX_ROW_COUNT;
                rows.truncate(DEFAULT_MAX_ROW_COUNT);
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
                run_statement(&session, &stmt).await.inspect_err(|e| {
                    tracing::warn!("snowflake statement failed: {e}");
                })?;
                let elapsed_ms = start.elapsed().as_millis();
                tracing::debug!("statement completed in {elapsed_ms}ms");
                // The REST query API does not surface an affected-row count.
                results.push(QueryResult {
                    columns: Vec::new(),
                    rows: Vec::new(),
                    rows_affected: None,
                    elapsed_ms,
                    result_truncated: false,
                    sql: None,
                });
            }
        }
        Ok(results)
    }

    async fn export_csv(&self, conn: Arc<dyn Connection>, sql: &str, path: &str) -> Result<u64> {
        let conn = downcast_conn::<SnowflakeConnection>(&conn)?;
        let (columns, rows) = run_query(&conn.session(), sql)
            .await
            .inspect_err(|e| tracing::warn!("snowflake export query failed: {e}"))?;

        let mut writer = CsvWriter::create(path, &columns)?;
        let mut count: u64 = 0;
        for row in &rows {
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
        let conn = downcast_conn::<SnowflakeConnection>(&conn)?;
        let session = conn.session();

        // The connector exposes no transaction handle, so we drive an explicit
        // BEGIN / COMMIT and ROLLBACK on the first failure. The REST query API
        // doesn't report affected-row counts, so the ApplyResult tallies the
        // number of statements applied rather than rows touched.
        run_statement(&session, "BEGIN").await?;

        let result = async {
            let mut result = ApplyResult::default();
            for u in &changes.updates {
                run_statement(&session, &build_update(schema, table, &u.pk, &u.changes)?).await?;
                result.updated += 1;
            }
            for values in &changes.inserts {
                run_statement(&session, &build_insert(schema, table, values)?).await?;
                result.inserted += 1;
            }
            for pk in &changes.deletes {
                run_statement(&session, &build_delete(schema, table, pk)?).await?;
                result.deleted += 1;
            }
            Ok::<_, PluginError>(result)
        }
        .await;

        match result {
            Ok(result) => {
                run_statement(&session, "COMMIT").await?;
                Ok(result)
            }
            Err(e) => {
                // Best-effort rollback; preserve the original error.
                let _ = run_statement(&session, "ROLLBACK").await;
                Err(e)
            }
        }
    }
}

fn parse_column_type_metadata(dtype: &str) -> (&str, Option<i32>, Option<i32>, Option<i32>) {
    let upper = dtype.to_ascii_uppercase();

    if let Some(args) = upper
        .strip_prefix("VARCHAR(")
        .and_then(|s| s.strip_suffix(')'))
    {
        return ("VARCHAR", args.parse().ok(), None, None);
    }

    if let Some(args) = upper
        .strip_prefix("NUMBER(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let mut parts = args.split(',');
        let p = parts.next().and_then(|v| v.trim().parse().ok());
        let s = parts.next().and_then(|v| v.trim().parse().ok());
        return ("NUMBER", None, p, s);
    }

    let re = Regex::new(r"^([A-Z]+)\(").unwrap();
    let c = re
        .captures(dtype)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str());

    (c.unwrap_or(dtype), None, None, None)
}

/// Run a query and return its column metadata plus rows as JSON cell matrices.
async fn run_query(
    session: &Session,
    sql: &str,
) -> Result<(Vec<ColumnMeta>, Vec<Vec<serde_json::Value>>)> {
    let table = session
        .query(sql.to_string())
        .await
        .map_err(|e| PluginError::Backend(e.to_string()))?
        .collect_table()
        .await
        .map_err(|e| PluginError::Backend(e.to_string()))?;

    let columns: Vec<ColumnMeta> = table
        .schema()
        .columns()
        .iter()
        .map(|c| ColumnMeta {
            name: c.name().to_string(),
            data_type: c.ty().as_str().to_string(),
        })
        .collect();

    let data = table
        .dynamic_rows()
        .map_err(|e| PluginError::Backend(e.to_string()))?
        .map(|row| {
            let row = row.map_err(|e| PluginError::Backend(e.to_string()))?;
            Ok(row
                .into_parts()
                .1
                .into_vec()
                .into_iter()
                .map(CellValue::into_json_value)
                .collect())
        })
        .collect::<Result<Vec<_>>>()?;

    Ok((columns, data))
}

/// Run a statement for side effects, discarding any returned rows.
async fn run_statement(session: &Session, sql: &str) -> Result<()> {
    session
        .query(sql.to_string())
        .await
        .map(|_| ())
        .map_err(|e| PluginError::Backend(e.to_string()))
}

fn json_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
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
    let mut set_parts = Vec::new();
    for cv in changes {
        set_parts.push(format!("{} = {}", quote_ident(&cv.column), value_expr(cv)?));
    }
    Ok(format!(
        "UPDATE {}.{} SET {} WHERE {}",
        quote_ident(schema),
        quote_ident(table),
        set_parts.join(", "),
        build_where(pk)?,
    ))
}

fn build_insert(schema: &str, table: &str, values: &[ColumnValue]) -> Result<String> {
    let mut cols = Vec::new();
    let mut vals = Vec::new();
    for cv in values {
        cols.push(quote_ident(&cv.column));
        vals.push(value_expr(cv)?);
    }
    Ok(if cols.is_empty() {
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
    })
}

fn build_delete(schema: &str, table: &str, pk: &[ColumnValue]) -> Result<String> {
    Ok(format!(
        "DELETE FROM {}.{} WHERE {}",
        quote_ident(schema),
        quote_ident(table),
        build_where(pk)?,
    ))
}

fn build_browse(schema: &str, table: &str, spec: &BrowseSpec) -> Result<String> {
    let mut conds: Vec<String> = Vec::new();

    for f in &spec.filters {
        conds.push(filter_expr(f)?);
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
    sql.push_str(&format!(" LIMIT {} OFFSET {}", spec.limit, spec.offset));
    Ok(sql)
}

fn filter_expr(f: &BrowseFilter) -> Result<String> {
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
            let cv = ColumnValue {
                column: f.column.clone(),
                type_name: f.type_name.clone(),
                value: f.value.clone(),
            };
            Ok(format!("{col} {sql_op} {}", value_expr(&cv)?))
        }
    }
}

fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Render a value as a single-quoted SQL string literal (with `'` doubled).
fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

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

/// Render a column value as an inline SQL expression. The connector's simple
/// `query` API takes no bind parameters, so values are emitted as quoted
/// literals (optionally cast to the declared column type).
fn value_expr(cv: &ColumnValue) -> Result<String> {
    if cv.value.is_null() {
        return Ok("NULL".into());
    }
    let lit = quote_literal(&bind_text(&cv.value));
    if cv.type_name.is_empty() {
        Ok(lit)
    } else {
        let ty = validate_type(&cv.type_name)?;
        Ok(format!("CAST({lit} AS {ty})"))
    }
}

fn build_where(key: &[ColumnValue]) -> Result<String> {
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
            parts.push(format!("{} = {}", quote_ident(&cv.column), value_expr(cv)?));
        }
    }
    Ok(parts.join(" AND "))
}

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
    matches!(
        head.as_str(),
        "select" | "with" | "show" | "explain" | "desc" | "describe"
    )
}

fn add_limit_in_select(sql: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    if upper.starts_with("SELECT") || upper.starts_with("WITH") {
        return format!(
            "SELECT * FROM ({}) q LIMIT {};",
            sql,
            DEFAULT_MAX_ROW_COUNT + 1
        );
    }
    sql.into()
}

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
                let mut depth = 1;
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

#[cfg(test)]
mod tests {
    use super::*;
    use polyclient_core::test_utils::find_connection_config;
    use std::path::Path;

    /// Live integration test against a real Snowflake account.
    ///
    /// It is `#[ignore]`d because it needs network access and real
    /// credentials; it never runs as part of `cargo test` unless asked for
    /// explicitly. Run it with:
    ///
    /// ```sh
    /// POLYCLIENT_SNOWFLAKE_APP_DIR=/path/to/app-data-dir \
    /// POLYCLIENT_SNOWFLAKE_CONNECTION_ID=<saved-connection-uuid> \
    ///   cargo test -p polyclient-plugin-snowflake -- --ignored --nocapture execute_query_against_live_snowflake
    /// ```
    ///
    /// `POLYCLIENT_SNOWFLAKE_APP_DIR` points at the host's app-data directory. The test
    /// scans every `connections/*/connections.json` under it and picks the saved
    /// connection whose `id` equals `POLYCLIENT_SNOWFLAKE_CONNECTION_ID`, then uses that
    /// profile's `config` to connect. The query can be overridden with
    /// `POLYCLIENT_SNOWFLAKE_QUERY`.
    #[tokio::test]
    #[ignore = "requires a live Snowflake account; provide POLYCLIENT_SNOWFLAKE_APP_DIR and POLYCLIENT_SNOWFLAKE_CONNECTION_ID"]
    async fn execute_query_against_live_snowflake() {
        let app_dir: &str = "/tmp/polyclient-test-app-data";
        let connection_id: &str = "9777e771-4ee1-4a60-bfa9-06f430896b24";

        // Find the saved connection across all connections.json files and use
        // its config (a map of field-name -> JSON value, exactly what the host
        // hands the plugin's `connect`).
        let cfg = find_connection_config(Path::new(&app_dir), connection_id);

        let plugin = SnowflakePlugin::new();
        let conn = plugin
            .connect(cfg)
            .await
            .expect("failed to open snowflake connection");

        let results = plugin
            .describe_table(conn, "TEST_SCHEMA", "CALL_CENTER", false)
            .await
            .expect("failed to describe table");

        println!("{:#?}", results);
    }
}

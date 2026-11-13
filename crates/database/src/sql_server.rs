use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context as _, Result};
use futures::{AsyncRead, AsyncWrite, StreamExt as _};
use tiberius::{
    AuthMethod, ColumnData, ColumnType, Config, EncryptionLevel, FromSql, Query, QueryItem, Row,
    SqlBrowser as _,
};
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

use crate::{
    Authentication, ColumnInfo, ColumnSummary, ConnectionProfile, DatabaseObject, EditTarget,
    Editability, ObjectKind, SessionReset,
    catalog::{Catalog, ObjectName},
    check_affected,
    execution::{Execution, ExecutionEnd, ExecutionEvent, Item, ReadEnd, Settled, settle},
    is_connection_error,
    sql::{self, EditColumn, ParameterizedStatement, TableRef},
};

trait TdsStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> TdsStream for T {}

type TdsClient = tiberius::Client<Box<dyn TdsStream>>;

/// `SERVERPROPERTY('EngineEdition')` for Azure SQL Database (single databases and elastic pools).
const AZURE_SQL_DATABASE_EDITION: i32 = 5;
/// How long to keep reading a stopped result before sending an Attention.
const DRAIN_BEFORE_CANCEL: Duration = Duration::from_millis(250);
/// How long the server may take to acknowledge an Attention. It rolls back the statement's
/// changes first, so this is generous.
const CANCEL_TIMEOUT: Duration = Duration::from_secs(30);

enum Stopped {
    Finished,
    /// The rest of the result was read, so the session is already in sync.
    Drained(crate::execution::Interrupt),
    /// The request is still running on the server.
    Interrupted(crate::execution::Interrupt),
}

pub struct SqlServerClient {
    client: TdsClient,
    /// Whether the stream drops stray Attention acknowledgements (see [`AttentionAckFilter`]),
    /// which is what lets a cancelled session be reused.
    filters_attention_acks: bool,
}

enum Endpoint {
    Tcp {
        host: String,
        port: Option<u16>,
        instance: Option<String>,
    },
    NamedPipe(String),
    LocalDb(String),
}

fn parse_endpoint(host: &str, port: Option<u16>) -> Endpoint {
    let host = host.trim();
    if let Some(pipe) = host.strip_prefix("np:") {
        return Endpoint::NamedPipe(pipe.to_string());
    }
    if let Some((server, instance)) = host.split_once('\\') {
        if server.eq_ignore_ascii_case("(localdb)") {
            return Endpoint::LocalDb(instance.to_string());
        }
        return Endpoint::Tcp {
            host: server.to_string(),
            port,
            instance: Some(instance.to_string()),
        };
    }
    if let Some((server, port_text)) = host.split_once(',')
        && let Ok(parsed_port) = port_text.trim().parse()
    {
        return Endpoint::Tcp {
            host: server.to_string(),
            port: Some(parsed_port),
            instance: None,
        };
    }
    Endpoint::Tcp {
        host: host.to_string(),
        port,
        instance: None,
    }
}

impl SqlServerClient {
    pub async fn connect(
        profile: &ConnectionProfile,
        database: Option<&str>,
        password: Option<&str>,
    ) -> Result<Self> {
        let mut config = Config::new();
        config.application_name("Zed");
        // Tiberius's own command timeout leaves the session out of sync when it fires. Timeouts
        // are applied by `Execution` instead, which cancels the statement and keeps the session.
        config.command_timeout(None);
        if let Some(database) = database {
            config.database(database);
        }
        match profile.authentication {
            Authentication::Integrated => {
                #[cfg(windows)]
                config.authentication(AuthMethod::Integrated);
                #[cfg(not(windows))]
                anyhow::bail!("Integrated authentication is only supported on Windows");
            }
            Authentication::Password => {
                let username = profile
                    .username
                    .as_deref()
                    .context("a user name is required for password authentication")?;
                config.authentication(AuthMethod::sql_server(username, password.unwrap_or("")));
            }
        }
        if profile.trust_server_certificate {
            config.trust_cert();
        }

        let endpoint = parse_endpoint(&profile.host, profile.port);
        let stream: Box<dyn TdsStream> = match endpoint {
            Endpoint::Tcp {
                host,
                port,
                instance,
            } => {
                config.host(&host);
                config.encryption(if profile.encrypt {
                    EncryptionLevel::Required
                } else {
                    EncryptionLevel::Off
                });
                let tcp = match instance {
                    Some(instance) => {
                        config.instance_name(instance);
                        if let Some(port) = port {
                            config.port(port);
                        }
                        tokio::net::TcpStream::connect_named(&config)
                            .await
                            .with_context(|| format!("connecting to {}", profile.host))?
                    }
                    None => {
                        config.port(port.unwrap_or(1433));
                        tokio::net::TcpStream::connect(config.get_addr())
                            .await
                            .with_context(|| format!("connecting to {}", config.get_addr()))?
                    }
                };
                tcp.set_nodelay(true)?;
                Box::new(tcp.compat_write())
            }
            Endpoint::NamedPipe(path) => {
                // Local pipes don't negotiate TLS; LocalDB and pipe-only instances reject it.
                config.encryption(EncryptionLevel::NotSupported);
                open_named_pipe(&path).await?
            }
            Endpoint::LocalDb(instance) => {
                config.encryption(EncryptionLevel::NotSupported);
                let path = local_db_pipe(&instance).await?;
                open_named_pipe(&path).await?
            }
        };

        // Below TLS the filter would see ciphertext, so encrypted sessions go without it.
        let is_encrypted = matches!(
            parse_endpoint(&profile.host, profile.port),
            Endpoint::Tcp { .. }
        ) && profile.encrypt;
        let filter = (!is_encrypted).then(|| Arc::new(AtomicBool::new(false)));
        let stream: Box<dyn TdsStream> = match &filter {
            Some(enabled) => Box::new(AttentionAckFilter::new(stream, enabled.clone())),
            None => stream,
        };
        let client = tiberius::Client::connect(config, stream)
            .await
            .with_context(|| format!("logging in to {}", profile.host))?;
        // Login is done and fully read, so the stream is at a packet boundary from here on.
        if let Some(enabled) = &filter {
            enabled.store(true, Ordering::Release);
        }
        Ok(Self {
            client,
            filters_attention_acks: filter.is_some(),
        })
    }

    /// Runs `sql` batch by batch on this session, split on `GO` lines the way SSMS and sqlcmd
    /// do; `GO n` runs the batch before it `n` times. It stops at the first batch that fails or
    /// is stopped, and what earlier batches returned has already been sent by then. The run's
    /// timeout covers all of its batches together.
    pub async fn execute(&mut self, sql: &str, execution: &mut Execution) -> Result<ExecutionEnd> {
        let batches = sql::split_batches(sql);
        for (index, batch) in batches.iter().enumerate() {
            let text = &sql[batch.range.clone()];
            for run in 1..=batch.repeat {
                // Between batches nothing is in flight, so a stop needs no Attention.
                if let Some(interrupt) = execution.interrupted() {
                    return Ok(interrupt.into());
                }
                let result = self.execute_batch(text, execution).await;
                let end = result.map_err(|error| match batch_label(sql, &batches, index, run) {
                    Some(label) => error.context(label),
                    None => error,
                })?;
                if end != ExecutionEnd::Completed {
                    return Ok(end);
                }
            }
        }
        Ok(ExecutionEnd::Completed)
    }

    /// Runs `sql` as a SQL batch. `sp_executesql` (tiberius's `Client::execute`) would scope
    /// temp tables, `USE`, `SET` options and transactions to the call, so even statements that
    /// return no rows don't go through it.
    async fn execute_batch(
        &mut self,
        sql: &str,
        execution: &mut Execution,
    ) -> Result<ExecutionEnd> {
        let stopped = match execution.guard(self.client.simple_query(sql)).await {
            Err(interrupt) => Stopped::Interrupted(interrupt),
            Ok(stream) => {
                let mut items = stream?.map(|item| {
                    Ok(match item? {
                        QueryItem::Metadata(metadata) => Item::Columns(
                            metadata
                                .columns()
                                .iter()
                                .map(|column| ColumnInfo {
                                    name: column.name().to_string(),
                                    type_name: type_name(column.column_type()).to_string(),
                                })
                                .collect(),
                        ),
                        QueryItem::Row(row) => {
                            Item::Row(row.into_iter().map(format_value).collect())
                        }
                    })
                });
                match execution.read(&mut items).await? {
                    ReadEnd::Exhausted => Stopped::Finished,
                    // Dropping the stream mid-token would leave the Attention's acknowledgement
                    // unreadable, so stop between tokens first. An Attention sent after the
                    // whole response arrived is acknowledged in a message of its own, which
                    // tiberius doesn't wait for, so a drained result must not get one.
                    ReadEnd::Interrupted(interrupt) => {
                        match settle(&mut items, DRAIN_BEFORE_CANCEL, is_connection_error).await? {
                            Settled::Ended => Stopped::Drained(interrupt),
                            Settled::Open => Stopped::Interrupted(interrupt),
                        }
                    }
                }
            }
        };
        match stopped {
            Stopped::Finished => {
                if reports_row_count(sql)
                    && let Some(count) = self.last_row_count().await
                {
                    execution.emit(ExecutionEvent::RowsAffected(count));
                }
                Ok(ExecutionEnd::Completed)
            }
            Stopped::Drained(interrupt) => Ok(interrupt.into()),
            Stopped::Interrupted(interrupt) => {
                self.cancel_request().await?;
                Ok(interrupt.into())
            }
        }
    }

    /// The number of rows the previous batch's last statement affected. Tiberius drops the
    /// per-statement counts in a batch's `DONE` tokens (it only reports them for
    /// `sp_executesql`), so this asks the server in a batch of its own. The statement already
    /// ran, so a failure here is logged rather than reported as the statement's.
    async fn last_row_count(&mut self) -> Option<u64> {
        let row = match self.client.simple_query("SELECT ROWCOUNT_BIG()").await {
            Ok(stream) => stream.into_row().await,
            Err(error) => Err(error),
        };
        match row {
            Ok(row) => row
                .and_then(|row| row.get::<i64, _>(0))
                .and_then(|count| u64::try_from(count).ok()),
            Err(error) => {
                log::error!("couldn't read the affected-row count: {error}");
                None
            }
        }
    }

    /// Sends an Attention for the request in flight and waits for the server to acknowledge
    /// it. If the session can't be brought back in sync it fails with [`SessionReset`], which
    /// makes the pool reconnect.
    async fn cancel_request(&mut self) -> Result<()> {
        match tokio::time::timeout(CANCEL_TIMEOUT, self.client.cancel_query()).await {
            Ok(Ok(())) => Ok(()),
            // SQL Server ends the cancelled request's response, then acknowledges the Attention
            // in a message of its own, and tiberius gives up at the end of the first message.
            // The filter drops the acknowledgement when it arrives.
            Ok(Err(tiberius::error::Error::Protocol(message)))
                if self.filters_attention_acks && message.contains("Never got a DONE token") =>
            {
                Ok(())
            }
            Ok(Err(error)) => Err(SessionReset(format!("{error}")).into()),
            Err(_) => Err(SessionReset(format!(
                "SQL Server didn't acknowledge the cancel within {} s",
                CANCEL_TIMEOUT.as_secs()
            ))
            .into()),
        }
    }

    async fn query_rows(&mut self, sql: &str, params: &[&str]) -> Result<Vec<Row>> {
        let mut query = Query::new(sql);
        for param in params {
            query.bind(*param);
        }
        Ok(query
            .query(&mut self.client)
            .await?
            .into_first_result()
            .await?)
    }

    pub async fn list_databases(&mut self) -> Result<Vec<String>> {
        let edition = self
            .query_rows("SELECT CAST(SERVERPROPERTY('EngineEdition') AS int)", &[])
            .await?;
        let is_azure_sql_database = edition
            .first()
            .and_then(|row| row.get::<i32, _>(0))
            .is_some_and(|edition| edition == AZURE_SQL_DATABASE_EDITION);
        // Azure SQL Database doesn't support HAS_DBACCESS (it would hide every user database), and
        // from `master` its sys.databases already lists only what the login may see.
        let sql = if is_azure_sql_database {
            "SELECT name FROM sys.databases WHERE state = 0 ORDER BY name"
        } else {
            "SELECT name FROM sys.databases WHERE state = 0 AND HAS_DBACCESS(name) = 1 ORDER BY name"
        };
        let rows = self.query_rows(sql, &[]).await?;
        Ok(rows.iter().filter_map(|row| string(row, 0)).collect())
    }

    pub async fn list_schemas(&mut self) -> Result<Vec<String>> {
        let rows = self
            .query_rows(
                "SELECT DISTINCT SCHEMA_NAME(schema_id) FROM sys.objects \
                 WHERE type IN ('U', 'V') AND is_ms_shipped = 0 \
                 UNION SELECT 'dbo' ORDER BY 1",
                &[],
            )
            .await?;
        Ok(rows.iter().filter_map(|row| string(row, 0)).collect())
    }

    pub async fn list_objects(&mut self, schema: &str) -> Result<Vec<DatabaseObject>> {
        let rows = self
            .query_rows(
                "SELECT name, RTRIM(type) FROM sys.objects \
                 WHERE schema_id = SCHEMA_ID(@P1) AND type IN ('U', 'V') AND is_ms_shipped = 0 \
                 ORDER BY name",
                &[schema],
            )
            .await?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                Some(DatabaseObject {
                    name: string(row, 0)?,
                    kind: if string(row, 1)? == "V" {
                        ObjectKind::View
                    } else {
                        ObjectKind::Table
                    },
                })
            })
            .collect())
    }

    pub async fn list_columns(&mut self, schema: &str, table: &str) -> Result<Vec<ColumnSummary>> {
        let qualified = sql::Dialect::SqlServer.qualified_name(&TableRef {
            schema: Some(schema.to_string()),
            name: table.to_string(),
        });
        let rows = self
            .query_rows(
                "SELECT c.name, TYPE_NAME(c.user_type_id), c.is_nullable, \
                   CAST(CASE WHEN EXISTS ( \
                     SELECT 1 FROM sys.indexes i \
                     JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
                     WHERE i.object_id = c.object_id AND i.is_primary_key = 1 AND ic.column_id = c.column_id \
                   ) THEN 1 ELSE 0 END AS bit) \
                 FROM sys.columns c WHERE c.object_id = OBJECT_ID(@P1) ORDER BY c.column_id",
                &[&qualified],
            )
            .await?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                Some(ColumnSummary {
                    name: string(row, 0)?,
                    type_name: string(row, 1).unwrap_or_default(),
                    nullable: row.get::<bool, _>(2).unwrap_or(true),
                    primary_key: row.get::<bool, _>(3).unwrap_or(false),
                })
            })
            .collect())
    }

    pub async fn catalog(&mut self) -> Result<Catalog> {
        let default_schema = self
            .query_rows("SELECT SCHEMA_NAME()", &[])
            .await?
            .first()
            .and_then(|row| string(row, 0));
        let schemas = self.list_schemas().await?;
        let column_rows = self
            .query_rows(
                "SELECT SCHEMA_NAME(o.schema_id), o.name, RTRIM(o.type), c.name, \
                   t.name + CASE \
                     WHEN t.name IN ('varchar', 'char', 'varbinary', 'binary') THEN '(' + \
                       CASE WHEN c.max_length = -1 THEN 'max' ELSE CAST(c.max_length AS varchar(10)) END + ')' \
                     WHEN t.name IN ('nvarchar', 'nchar') THEN '(' + \
                       CASE WHEN c.max_length = -1 THEN 'max' ELSE CAST(c.max_length / 2 AS varchar(10)) END + ')' \
                     WHEN t.name IN ('decimal', 'numeric') THEN \
                       '(' + CAST(c.precision AS varchar(10)) + ',' + CAST(c.scale AS varchar(10)) + ')' \
                     ELSE '' END, \
                   c.is_nullable, CAST(CASE WHEN pk.column_id IS NULL THEN 0 ELSE 1 END AS bit) \
                 FROM sys.objects o \
                 JOIN sys.columns c ON c.object_id = o.object_id \
                 JOIN sys.types t ON t.user_type_id = c.user_type_id \
                 LEFT JOIN ( \
                   SELECT ic.object_id, ic.column_id FROM sys.indexes i \
                   JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
                   WHERE i.is_primary_key = 1 \
                 ) pk ON pk.object_id = c.object_id AND pk.column_id = c.column_id \
                 WHERE o.type IN ('U', 'V') AND o.is_ms_shipped = 0 \
                 ORDER BY o.schema_id, o.name, c.column_id",
                &[],
            )
            .await?;
        let objects = crate::catalog::group_columns(column_rows.iter().filter_map(|row| {
            let name = ObjectName {
                schema: string(row, 0)?,
                name: string(row, 1)?,
            };
            let kind = if string(row, 2)? == "V" {
                ObjectKind::View
            } else {
                ObjectKind::Table
            };
            let column = ColumnSummary {
                name: string(row, 3)?,
                type_name: string(row, 4).unwrap_or_default(),
                nullable: row.get::<bool, _>(5).unwrap_or(true),
                primary_key: row.get::<bool, _>(6).unwrap_or(false),
            };
            Some((name, kind, column))
        }));
        let key_rows = self
            .query_rows(
                "SELECT fk.name, SCHEMA_NAME(pt.schema_id), pt.name, pc.name, \
                   SCHEMA_NAME(rt.schema_id), rt.name, rc.name \
                 FROM sys.foreign_keys fk \
                 JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
                 JOIN sys.objects pt ON pt.object_id = fkc.parent_object_id \
                 JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
                 JOIN sys.objects rt ON rt.object_id = fkc.referenced_object_id \
                 JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
                 ORDER BY fk.object_id, fkc.constraint_column_id",
                &[],
            )
            .await?;
        let foreign_keys = crate::catalog::group_foreign_keys(key_rows.iter().filter_map(|row| {
            Some((
                string(row, 0)?,
                ObjectName {
                    schema: string(row, 1)?,
                    name: string(row, 2)?,
                },
                string(row, 3)?,
                ObjectName {
                    schema: string(row, 4)?,
                    name: string(row, 5)?,
                },
                string(row, 6)?,
            ))
        }));
        Ok(Catalog {
            default_schema,
            schemas,
            objects,
            foreign_keys,
        })
    }

    pub async fn editability(&mut self, sql: &str) -> Result<Editability> {
        // SQL Server can't describe `GO` lines, and `GO n` returns the result several times.
        let sql = match sql::split_batches(sql).as_slice() {
            [batch] if batch.repeat == 1 => &sql[batch.range.clone()],
            _ => {
                return Ok(Err(
                    "Results can only be edited when a single statement was run.".into(),
                ));
            }
        };
        let rows = match self
            .query_rows(
                "EXEC sp_describe_first_result_set @tsql = @P1, @params = NULL, @browse_information_mode = 1",
                &[sql],
            )
            .await
        {
            Ok(rows) => rows,
            Err(error) => {
                return Ok(Err(format!(
                    "SQL Server couldn't describe the query's source table: {error:#}"
                )));
            }
        };

        struct Described {
            hidden: bool,
            source_database: Option<String>,
            source_schema: Option<String>,
            source_table: Option<String>,
            source_column: Option<String>,
            read_only: bool,
        }
        let described = rows
            .iter()
            .map(|row| Described {
                hidden: row.get::<bool, _>("is_hidden").unwrap_or(false),
                source_database: row.get::<&str, _>("source_database").map(ToOwned::to_owned),
                source_schema: row.get::<&str, _>("source_schema").map(ToOwned::to_owned),
                source_table: row.get::<&str, _>("source_table").map(ToOwned::to_owned),
                source_column: row.get::<&str, _>("source_column").map(ToOwned::to_owned),
                read_only: row.get::<bool, _>("is_identity_column").unwrap_or(false)
                    || row.get::<bool, _>("is_computed_column").unwrap_or(false)
                    || !row.get::<bool, _>("is_updateable").unwrap_or(false),
            })
            .collect::<Vec<_>>();
        let visible = described
            .iter()
            .filter(|column| !column.hidden)
            .collect::<Vec<_>>();
        let mut tables = Vec::<(Option<String>, String, String)>::new();
        for column in &visible {
            if let (Some(schema), Some(table)) = (&column.source_schema, &column.source_table) {
                let entry = (
                    column.source_database.clone(),
                    schema.clone(),
                    table.clone(),
                );
                if !tables.contains(&entry) {
                    tables.push(entry);
                }
            }
        }
        let (source_database, schema, table) = match tables.as_slice() {
            [] => return Ok(Err("The result doesn't come from a table.".into())),
            [single] => single.clone(),
            many => {
                let names = many
                    .iter()
                    .map(|(_, schema, table)| format!("{schema}.{table}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Ok(Err(format!(
                    "The result combines several tables ({names}); edit queries that read from one table."
                )));
            }
        };
        // The session's database, which a `USE` in the console may have changed.
        let current_database = self
            .query_rows("SELECT DB_NAME()", &[])
            .await?
            .first()
            .and_then(|row| string(row, 0));
        if let (Some(source), Some(current)) = (&source_database, &current_database)
            && !source.eq_ignore_ascii_case(current)
        {
            return Ok(Err(format!(
                "The table is in database {source}; open a console on that database to edit it."
            )));
        }

        let table_ref = TableRef {
            schema: Some(schema.clone()),
            name: table.clone(),
        };
        let qualified = sql::Dialect::SqlServer.qualified_name(&table_ref);
        let key_rows = self
            .query_rows(
                "SELECT c.name FROM sys.indexes i \
                 JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
                 JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
                 WHERE i.object_id = OBJECT_ID(@P1) AND i.is_primary_key = 1 ORDER BY ic.key_ordinal",
                &[&qualified],
            )
            .await?;
        let key_names = key_rows
            .iter()
            .filter_map(|row| string(row, 0))
            .collect::<Vec<_>>();
        if key_names.is_empty() {
            return Ok(Err(format!(
                "{schema}.{table} has no primary key, so rows can't be identified for updates."
            )));
        }

        let from_table = |column: &Described| {
            column.source_schema.as_deref() == Some(schema.as_str())
                && column.source_table.as_deref() == Some(table.as_str())
        };
        let mut key_columns = Vec::new();
        for key_name in &key_names {
            let position = visible.iter().position(|column| {
                from_table(column)
                    && column
                        .source_column
                        .as_deref()
                        .is_some_and(|source| source.eq_ignore_ascii_case(key_name))
            });
            match position {
                Some(position) => key_columns.push(position),
                None => {
                    return Ok(Err(format!(
                        "Primary key column {key_name} isn't in the result; select it to enable editing."
                    )));
                }
            }
        }

        let columns = visible
            .iter()
            .map(|column| {
                let source = column.source_column.as_ref()?;
                from_table(column).then(|| EditColumn {
                    name: source.clone(),
                    type_name: None,
                    type_schema: None,
                })
            })
            .collect::<Vec<_>>();
        let writable = visible
            .iter()
            .map(|column| from_table(column) && column.source_column.is_some() && !column.read_only)
            .collect();

        Ok(Ok(EditTarget {
            table: table_ref,
            columns,
            writable,
            key_columns,
        }))
    }

    pub async fn apply(&mut self, statements: &[ParameterizedStatement]) -> Result<Vec<u64>> {
        self.client
            .simple_query("BEGIN TRANSACTION")
            .await?
            .into_results()
            .await?;
        let result = self.apply_in_transaction(statements).await;
        match &result {
            Ok(_) => {
                self.client
                    .simple_query("COMMIT TRANSACTION")
                    .await?
                    .into_results()
                    .await?;
            }
            Err(_) => {
                let rollback = self
                    .client
                    .simple_query("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION")
                    .await;
                match rollback {
                    Ok(stream) => {
                        stream.into_results().await.log_rollback_error();
                    }
                    Err(error) => log::error!("rollback failed: {error}"),
                }
            }
        }
        result
    }

    async fn apply_in_transaction(
        &mut self,
        statements: &[ParameterizedStatement],
    ) -> Result<Vec<u64>> {
        let mut counts = Vec::with_capacity(statements.len());
        for statement in statements {
            let mut query = Query::new(statement.sql.as_str());
            for param in &statement.params {
                query.bind(param.clone());
            }
            let result = query
                .execute(&mut self.client)
                .await
                .with_context(|| format!("running `{}`", statement.sql))?;
            let affected = result.total();
            check_affected(statement, affected)?;
            counts.push(affected);
        }
        Ok(counts)
    }
}

trait LogRollbackError {
    fn log_rollback_error(self);
}

impl<T> LogRollbackError for tiberius::Result<T> {
    fn log_rollback_error(self) {
        if let Err(error) = self {
            log::error!("rollback failed: {error}");
        }
    }
}

const PACKET_HEADER_LENGTH: usize = 8;
/// A packet header plus one `DONE` token (type, status, current command, row count).
const ATTENTION_ACK_LENGTH: usize = PACKET_HEADER_LENGTH + 13;
const TABULAR_RESULT_PACKET: u8 = 0x04;
const END_OF_MESSAGE: u8 = 0x01;
const DONE_TOKEN: u8 = 0xFD;
const DONE_ATTENTION: u16 = 0x20;

/// Whether `packet` is a whole message holding only the `DONE` that acknowledges an Attention.
/// Nothing else carries the attention bit, so dropping these can't hide a real reply.
fn is_attention_ack(packet: &[u8]) -> bool {
    let [
        kind,
        status,
        _,
        _,
        _,
        _,
        _,
        _,
        token,
        status_low,
        status_high,
        ..,
    ] = packet
    else {
        return false;
    };
    packet.len() == ATTENTION_ACK_LENGTH
        && *kind == TABULAR_RESULT_PACKET
        && status & END_OF_MESSAGE != 0
        && *token == DONE_TOKEN
        && u16::from_le_bytes([*status_low, *status_high]) & DONE_ATTENTION != 0
}

/// Drops the separate Attention acknowledgements that tiberius doesn't read (see
/// `SqlServerClient::cancel_request`); otherwise one would be taken as the reply to the next
/// request. It reads whole TDS packets, so it only works on an unencrypted stream, and it
/// passes bytes through untouched until `enabled` is set after login.
struct AttentionAckFilter<S> {
    inner: S,
    enabled: Arc<AtomicBool>,
    /// Bytes read from `inner` that don't yet form a whole packet.
    incoming: Vec<u8>,
    /// Whole packets waiting to be read, from `ready_start`.
    ready: Vec<u8>,
    ready_start: usize,
    at_end: bool,
}

impl<S> AttentionAckFilter<S> {
    fn new(inner: S, enabled: Arc<AtomicBool>) -> Self {
        Self {
            inner,
            enabled,
            incoming: Vec::new(),
            ready: Vec::new(),
            ready_start: 0,
            at_end: false,
        }
    }

    fn take_packets(&mut self) {
        loop {
            let [_, _, length_high, length_low, ..] = self.incoming[..] else {
                return;
            };
            let length = usize::from(u16::from_be_bytes([length_high, length_low]));
            if length < PACKET_HEADER_LENGTH {
                // Not a TDS packet header: stop interpreting and pass everything through.
                self.enabled.store(false, Ordering::Release);
                self.ready.append(&mut self.incoming);
                return;
            }
            if self.incoming.len() < length {
                return;
            }
            let packet = self.incoming.drain(..length).collect::<Vec<_>>();
            if is_attention_ack(&packet) {
                log::debug!("dropped a separate Attention acknowledgement");
            } else {
                self.ready.extend_from_slice(&packet);
            }
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for AttentionAckFilter<S> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        loop {
            if let Some(ready) = this.ready.get(this.ready_start..)
                && !ready.is_empty()
            {
                let count = ready.len().min(buf.len());
                if let (Some(target), Some(source)) = (buf.get_mut(..count), ready.get(..count)) {
                    target.copy_from_slice(source);
                }
                this.ready_start += count;
                if this.ready_start >= this.ready.len() {
                    this.ready.clear();
                    this.ready_start = 0;
                }
                return std::task::Poll::Ready(Ok(count));
            }
            if !this.enabled.load(Ordering::Acquire) && this.incoming.is_empty() {
                return std::pin::Pin::new(&mut this.inner).poll_read(cx, buf);
            }
            if this.at_end {
                if this.incoming.is_empty() {
                    return std::task::Poll::Ready(Ok(0));
                }
                this.ready.append(&mut this.incoming);
                continue;
            }
            let mut chunk = [0; 8192];
            let count =
                std::task::ready!(std::pin::Pin::new(&mut this.inner).poll_read(cx, &mut chunk))?;
            match chunk.get(..count) {
                Some(read) if !read.is_empty() => {
                    this.incoming.extend_from_slice(read);
                    this.take_packets();
                }
                _ => this.at_end = true,
            }
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for AttentionAckFilter<S> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_close(cx)
    }
}

#[cfg(windows)]
async fn open_named_pipe(path: &str) -> Result<Box<dyn TdsStream>> {
    use tokio::net::windows::named_pipe::ClientOptions;
    const ERROR_PIPE_BUSY: i32 = 231;
    let mut attempts = 0;
    loop {
        match ClientOptions::new().open(path) {
            Ok(pipe) => return Ok(Box::new(pipe.compat())),
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) && attempts < 20 => {
                attempts += 1;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("opening named pipe {path}"));
            }
        }
    }
}

#[cfg(not(windows))]
async fn open_named_pipe(path: &str) -> Result<Box<dyn TdsStream>> {
    anyhow::bail!("named pipe {path} can only be opened on Windows")
}

/// Starts a LocalDB instance (they stop when idle) and returns its named pipe path.
async fn local_db_pipe(instance: &str) -> Result<String> {
    let start = tokio::process::Command::new("sqllocaldb")
        .args(["start", instance])
        .output()
        .await
        .context("running sqllocaldb; is SQL Server Express LocalDB installed?")?;
    if !start.status.success() {
        anyhow::bail!(
            "sqllocaldb couldn't start instance {instance}: {}",
            String::from_utf8_lossy(&start.stdout).trim()
        );
    }
    let info = tokio::process::Command::new("sqllocaldb")
        .args(["info", instance])
        .output()
        .await
        .context("running sqllocaldb info")?;
    let output = String::from_utf8_lossy(&info.stdout);
    output
        .lines()
        .find_map(|line| {
            let start = line.find("np:")?;
            Some(line[start + 3..].trim().to_string())
        })
        .with_context(|| format!("LocalDB instance {instance} reported no pipe name"))
}

/// Whether `ROWCOUNT_BIG()` after the batch gives its count: the batch's last statement is its
/// only data change. With several, the count would read as the whole batch's.
fn reports_row_count(sql: &str) -> bool {
    let statements = sql::split_statements(sql, sql::Dialect::SqlServer);
    let is_data_change =
        |range: &std::ops::Range<usize>| sql.get(range.clone()).is_some_and(sql::is_data_change);
    statements.last().is_some_and(is_data_change)
        && statements
            .iter()
            .filter(|range| is_data_change(range))
            .count()
            == 1
}

/// Says which batch of a script failed, when there's more than one to choose from. Its line is
/// counted in the text that ran, and SQL Server's own line numbers count from there.
fn batch_label(sql: &str, batches: &[sql::Batch], index: usize, run: u32) -> Option<String> {
    let batch = batches.get(index)?;
    let repetition = (batch.repeat > 1).then(|| format!("repetition {run} of {}", batch.repeat));
    if batches.len() < 2 {
        return repetition.map(|repetition| format!("running {repetition} of the batch"));
    }
    let line = sql.get(..batch.range.start)?.matches('\n').count() + 1;
    let mut label = format!(
        "running batch {} of {} (from line {line})",
        index + 1,
        batches.len()
    );
    if let Some(repetition) = repetition {
        label.push_str(", ");
        label.push_str(&repetition);
    }
    Some(label)
}

fn string(row: &Row, index: usize) -> Option<String> {
    row.get::<&str, _>(index).map(ToOwned::to_owned)
}

fn format_value(data: ColumnData<'static>) -> Option<String> {
    match data {
        ColumnData::U8(value) => value.map(|value| value.to_string()),
        ColumnData::I16(value) => value.map(|value| value.to_string()),
        ColumnData::I32(value) => value.map(|value| value.to_string()),
        ColumnData::I64(value) => value.map(|value| value.to_string()),
        ColumnData::F32(value) => value.map(|value| value.to_string()),
        ColumnData::F64(value) => value.map(|value| value.to_string()),
        ColumnData::Bit(value) => value.map(|value| if value { "1" } else { "0" }.to_string()),
        ColumnData::String(value) => value.map(|value| value.into_owned()),
        ColumnData::Guid(value) => value.map(|value| value.to_string().to_uppercase()),
        ColumnData::Binary(value) => value.map(|bytes| {
            let mut text = String::with_capacity(2 + bytes.len() * 2);
            text.push_str("0x");
            for byte in bytes.iter() {
                text.push_str(&format!("{byte:02X}"));
            }
            text
        }),
        ColumnData::Numeric(value) => value.map(|value| value.to_string()),
        ColumnData::Xml(value) => value.map(|value| value.into_owned().into_string()),
        ColumnData::Date(_) => chrono::NaiveDate::from_sql(&data)
            .ok()
            .flatten()
            .map(|value| value.to_string()),
        ColumnData::Time(_) => chrono::NaiveTime::from_sql(&data)
            .ok()
            .flatten()
            .map(|value| value.to_string()),
        ColumnData::DateTimeOffset(_) => chrono::DateTime::<chrono::FixedOffset>::from_sql(&data)
            .ok()
            .flatten()
            .map(|value| value.format("%Y-%m-%d %H:%M:%S%.f %:z").to_string()),
        ColumnData::DateTime(_) | ColumnData::SmallDateTime(_) | ColumnData::DateTime2(_) => {
            chrono::NaiveDateTime::from_sql(&data)
                .ok()
                .flatten()
                .map(|value| value.format("%Y-%m-%d %H:%M:%S%.f").to_string())
        }
    }
}

fn type_name(column_type: ColumnType) -> &'static str {
    match column_type {
        ColumnType::Null => "null",
        ColumnType::Bit | ColumnType::Bitn => "bit",
        ColumnType::Int1 => "tinyint",
        ColumnType::Int2 => "smallint",
        ColumnType::Int4 => "int",
        ColumnType::Int8 => "bigint",
        ColumnType::Intn => "int",
        ColumnType::Datetime4 => "smalldatetime",
        ColumnType::Float4 => "real",
        ColumnType::Float8 | ColumnType::Floatn => "float",
        ColumnType::Money | ColumnType::Money4 => "money",
        ColumnType::Datetime | ColumnType::Datetimen => "datetime",
        ColumnType::Guid => "uniqueidentifier",
        ColumnType::Decimaln => "decimal",
        ColumnType::Numericn => "numeric",
        ColumnType::Daten => "date",
        ColumnType::Timen => "time",
        ColumnType::Datetime2 => "datetime2",
        ColumnType::DatetimeOffsetn => "datetimeoffset",
        ColumnType::BigVarBin => "varbinary",
        ColumnType::BigVarChar => "varchar",
        ColumnType::BigBinary => "binary",
        ColumnType::BigChar => "char",
        ColumnType::NVarchar => "nvarchar",
        ColumnType::NChar => "nchar",
        ColumnType::Xml => "xml",
        ColumnType::Udt => "udt",
        ColumnType::Text => "text",
        ColumnType::Image => "image",
        ColumnType::NText => "ntext",
        ColumnType::SSVariant => "sql_variant",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_only_separate_attention_acknowledgements() {
        use futures::AsyncReadExt as _;

        // As captured from LocalDB: a cancelled batch's response, then the acknowledgement.
        let response = [
            0x04, 0x01, 0x00, 0x15, 0x00, 0x55, 0x01, 0x00, 0xFD, 0x02, 0x00, 0xFD, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let mut acknowledgement = response;
        acknowledgement[9] = 0x20;
        let mut reply = response;
        reply[9] = 0x10;
        reply[13] = 0x07;
        let wire = [&response[..], &acknowledgement, &reply].concat();

        // A reader that hands the bytes over a few at a time, splitting packets.
        struct Trickle(Vec<u8>, usize);
        impl AsyncRead for Trickle {
            fn poll_read(
                mut self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                buf: &mut [u8],
            ) -> std::task::Poll<std::io::Result<usize>> {
                let start = self.1;
                let end = (start + 5).min(self.0.len()).min(start + buf.len());
                buf[..end - start].copy_from_slice(&self.0[start..end]);
                self.1 = end;
                std::task::Poll::Ready(Ok(end - start))
            }
        }

        let mut filtered = Vec::new();
        let mut filter =
            AttentionAckFilter::new(Trickle(wire.clone(), 0), Arc::new(AtomicBool::new(true)));
        futures::executor::block_on(filter.read_to_end(&mut filtered)).unwrap();
        assert_eq!(filtered, [&response[..], &reply].concat());

        let mut untouched = Vec::new();
        let mut disabled =
            AttentionAckFilter::new(Trickle(wire.clone(), 0), Arc::new(AtomicBool::new(false)));
        futures::executor::block_on(disabled.read_to_end(&mut untouched)).unwrap();
        assert_eq!(untouched, wire);
    }

    #[test]
    fn names_the_failing_batch() {
        let label = |sql: &str, index, run| {
            batch_label(sql, &sql::split_batches(sql), index, run).unwrap_or_default()
        };
        assert_eq!(label("SELECT 1", 0, 1), "");
        assert_eq!(
            label("SELECT 1\nGO 5", 0, 3),
            "running repetition 3 of 5 of the batch"
        );
        let script = "SELECT 1\nGO\n\nSELECT 2\nGO 4\nSELECT 3";
        assert_eq!(label(script, 0, 1), "running batch 1 of 3 (from line 1)");
        assert_eq!(
            label(script, 1, 2),
            "running batch 2 of 3 (from line 3), repetition 2 of 4"
        );
        assert_eq!(label(script, 2, 1), "running batch 3 of 3 (from line 6)");
    }

    #[test]
    fn parses_endpoints() {
        assert!(matches!(
            parse_endpoint("(localdb)\\MSSQLLocalDB", None),
            Endpoint::LocalDb(instance) if instance == "MSSQLLocalDB"
        ));
        assert!(matches!(
            parse_endpoint("db.example.com,14330", None),
            Endpoint::Tcp { host, port: Some(14330), instance: None } if host == "db.example.com"
        ));
        assert!(matches!(
            parse_endpoint("server\\SQLEXPRESS", None),
            Endpoint::Tcp { instance: Some(instance), .. } if instance == "SQLEXPRESS"
        ));
        assert!(matches!(
            parse_endpoint("np:\\\\.\\pipe\\sql\\query", None),
            Endpoint::NamedPipe(path) if path == "\\\\.\\pipe\\sql\\query"
        ));
    }
}

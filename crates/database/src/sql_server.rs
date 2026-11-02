use anyhow::{Context as _, Result};
use futures::{AsyncRead, AsyncWrite, TryStreamExt as _};
use tiberius::{
    AuthMethod, ColumnData, ColumnType, Config, EncryptionLevel, FromSql, Query, QueryItem, Row,
    SqlBrowser as _,
};
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

use crate::{
    Authentication, ColumnInfo, ColumnSummary, ConnectionProfile, DatabaseObject, EditTarget,
    Editability, ObjectKind, ResultSet, StatementOutcome, check_affected,
    sql::{self, EditColumn, ParameterizedStatement, TableRef},
};

trait TdsStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> TdsStream for T {}

type TdsClient = tiberius::Client<Box<dyn TdsStream>>;

pub struct SqlServerClient {
    client: TdsClient,
    database: Option<String>,
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

        let client = tiberius::Client::connect(config, stream)
            .await
            .with_context(|| format!("logging in to {}", profile.host))?;
        Ok(Self {
            client,
            database: database.map(ToOwned::to_owned),
        })
    }

    pub async fn run(&mut self, sql: &str) -> Result<Vec<StatementOutcome>> {
        if !sql::may_return_rows(sql) {
            let result = self.client.execute(sql, &[]).await?;
            let counts = result.rows_affected();
            if counts.is_empty() {
                return Ok(vec![StatementOutcome::Completed]);
            }
            return Ok(counts
                .iter()
                .map(|count| StatementOutcome::RowsAffected(*count))
                .collect());
        }

        let mut stream = self.client.simple_query(sql).await?;
        let mut outcomes = Vec::new();
        let mut current: Option<ResultSet> = None;
        while let Some(item) = stream.try_next().await? {
            match item {
                QueryItem::Metadata(metadata) => {
                    if let Some(result) = current.take() {
                        outcomes.push(StatementOutcome::Rows(result));
                    }
                    current = Some(ResultSet {
                        columns: metadata
                            .columns()
                            .iter()
                            .map(|column| ColumnInfo {
                                name: column.name().to_string(),
                                type_name: type_name(column.column_type()).to_string(),
                            })
                            .collect(),
                        rows: Vec::new(),
                    });
                }
                QueryItem::Row(row) => {
                    if let Some(result) = current.as_mut() {
                        result
                            .rows
                            .push(row.into_iter().map(format_value).collect());
                    }
                }
            }
        }
        if let Some(result) = current.take() {
            outcomes.push(StatementOutcome::Rows(result));
        }
        if outcomes.is_empty() {
            outcomes.push(StatementOutcome::Completed);
        }
        Ok(outcomes)
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
        let rows = self
            .query_rows(
                "SELECT name FROM sys.databases WHERE state = 0 AND HAS_DBACCESS(name) = 1 ORDER BY name",
                &[],
            )
            .await?;
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

    pub async fn editability(&mut self, sql: &str, result: &ResultSet) -> Result<Editability> {
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
        if visible.len() != result.columns.len() {
            return Ok(Err(
                "The result's columns don't match the statement's description.".into(),
            ));
        }

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
        if let (Some(source), Some(current)) = (&source_database, &self.database)
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

use anyhow::{Context as _, Result};
use tokio_postgres::{SimpleQueryMessage, config::SslMode, types::ToSql};

use crate::{
    ColumnInfo, ColumnSummary, ConnectionProfile, DatabaseObject, EditTarget, Editability,
    ObjectKind, ResultSet, StatementOutcome, check_affected,
    sql::{EditColumn, ParameterizedStatement, TableRef},
};

pub struct PostgresClient {
    client: tokio_postgres::Client,
}

impl PostgresClient {
    pub async fn connect(
        profile: &ConnectionProfile,
        database: Option<&str>,
        password: Option<&str>,
    ) -> Result<Self> {
        let mut config = tokio_postgres::Config::new();
        config
            .host(profile.host.trim())
            .port(profile.port.unwrap_or(5432))
            .dbname(database.unwrap_or("postgres"))
            .application_name("Zed")
            .ssl_mode(if profile.encrypt {
                SslMode::Prefer
            } else {
                SslMode::Disable
            });
        if let Some(username) = &profile.username {
            config.user(username);
        }
        if let Some(password) = password {
            config.password(password);
        }
        let connector = native_tls::TlsConnector::builder()
            .danger_accept_invalid_certs(profile.trust_server_certificate)
            .build()?;
        let (client, connection) = config
            .connect(postgres_native_tls::MakeTlsConnector::new(connector))
            .await
            .with_context(|| format!("connecting to {}", profile.host))?;
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                log::error!("postgres connection closed: {error}");
            }
        });
        Ok(Self { client })
    }

    pub async fn run(&mut self, sql: &str) -> Result<Vec<StatementOutcome>> {
        let messages = self.client.simple_query(sql).await?;
        let mut outcomes = Vec::new();
        let mut current: Option<ResultSet> = None;
        for message in messages {
            match message {
                SimpleQueryMessage::RowDescription(columns) => {
                    current = Some(ResultSet {
                        columns: columns
                            .iter()
                            .map(|column| ColumnInfo {
                                name: column.name().to_string(),
                                type_name: String::new(),
                            })
                            .collect(),
                        rows: Vec::new(),
                    });
                }
                SimpleQueryMessage::Row(row) => {
                    let result = current.get_or_insert_with(|| ResultSet {
                        columns: row
                            .columns()
                            .iter()
                            .map(|column| ColumnInfo {
                                name: column.name().to_string(),
                                type_name: String::new(),
                            })
                            .collect(),
                        rows: Vec::new(),
                    });
                    result.rows.push(
                        (0..row.len())
                            .map(|index| row.get(index).map(ToOwned::to_owned))
                            .collect(),
                    );
                }
                SimpleQueryMessage::CommandComplete(count) => match current.take() {
                    Some(result) => outcomes.push(StatementOutcome::Rows(result)),
                    None => outcomes.push(StatementOutcome::RowsAffected(count)),
                },
                _ => {}
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

    async fn strings(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<String>> {
        let rows = self.client.query(sql, params).await?;
        rows.iter()
            .map(|row| Ok(row.try_get::<_, String>(0)?))
            .collect()
    }

    pub async fn list_databases(&mut self) -> Result<Vec<String>> {
        self.strings(
            "SELECT datname FROM pg_database WHERE NOT datistemplate AND datallowconn ORDER BY datname",
            &[],
        )
        .await
    }

    pub async fn list_schemas(&mut self) -> Result<Vec<String>> {
        self.strings(
            "SELECT nspname::text FROM pg_namespace \
             WHERE nspname NOT LIKE 'pg\\_%' AND nspname <> 'information_schema' ORDER BY nspname",
            &[],
        )
        .await
    }

    pub async fn list_objects(&mut self, schema: &str) -> Result<Vec<DatabaseObject>> {
        let rows = self
            .client
            .query(
                "SELECT c.relname::text, c.relkind::text FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relkind IN ('r', 'p', 'v', 'm', 'f') ORDER BY c.relname",
                &[&schema],
            )
            .await?;
        rows.iter()
            .map(|row| {
                let kind: String = row.try_get(1)?;
                Ok(DatabaseObject {
                    name: row.try_get(0)?,
                    kind: if kind == "v" || kind == "m" {
                        ObjectKind::View
                    } else {
                        ObjectKind::Table
                    },
                })
            })
            .collect()
    }

    pub async fn list_columns(&mut self, schema: &str, table: &str) -> Result<Vec<ColumnSummary>> {
        let rows = self
            .client
            .query(
                "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), NOT a.attnotnull, \
                   COALESCE(a.attnum = ANY(i.indkey), false) \
                 FROM pg_attribute a \
                 JOIN pg_class c ON c.oid = a.attrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 LEFT JOIN pg_index i ON i.indrelid = c.oid AND i.indisprimary \
                 WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY a.attnum",
                &[&schema, &table],
            )
            .await?;
        rows.iter()
            .map(|row| {
                Ok(ColumnSummary {
                    name: row.try_get(0)?,
                    type_name: row.try_get(1)?,
                    nullable: row.try_get(2)?,
                    primary_key: row.try_get(3)?,
                })
            })
            .collect()
    }

    pub async fn editability(&mut self, sql: &str, result: &ResultSet) -> Result<Editability> {
        let statement = match self.client.prepare(sql).await {
            Ok(statement) => statement,
            Err(error) => return Ok(Err(format!("Couldn't describe the query: {error}"))),
        };
        let columns = statement.columns();
        if columns.len() != result.columns.len() {
            return Ok(Err(
                "The result's columns don't match the statement's description.".into(),
            ));
        }
        let mut table_oids = columns
            .iter()
            .filter_map(|column| column.table_oid())
            .collect::<Vec<_>>();
        table_oids.dedup();
        table_oids.sort_unstable();
        table_oids.dedup();
        let table_oid = match table_oids.as_slice() {
            [] => return Ok(Err("The result doesn't come from a table.".into())),
            [single] => *single,
            _ => {
                return Ok(Err(
                    "The result combines several tables; edit queries that read from one table."
                        .into(),
                ));
            }
        };

        let relation = self
            .client
            .query_one(
                "SELECT n.nspname::text, c.relname::text, c.relkind::text FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.oid = $1",
                &[&table_oid],
            )
            .await?;
        let schema: String = relation.try_get(0)?;
        let table: String = relation.try_get(1)?;
        let relkind: String = relation.try_get(2)?;
        if relkind != "r" && relkind != "p" {
            return Ok(Err(format!("{schema}.{table} is not a table.")));
        }

        let attributes = self
            .client
            .query(
                "SELECT a.attnum, a.attname::text, \
                   (a.attgenerated <> '' OR a.attidentity = 'a'), \
                   COALESCE(a.attnum = ANY(i.indkey), false) \
                 FROM pg_attribute a \
                 LEFT JOIN pg_index i ON i.indrelid = a.attrelid AND i.indisprimary \
                 WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped",
                &[&table_oid],
            )
            .await?;
        struct Attribute {
            number: i16,
            name: String,
            read_only: bool,
            primary_key: bool,
        }
        let attributes = attributes
            .iter()
            .map(|row| {
                Ok(Attribute {
                    number: row.try_get(0)?,
                    name: row.try_get(1)?,
                    read_only: row.try_get(2)?,
                    primary_key: row.try_get(3)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if !attributes.iter().any(|attribute| attribute.primary_key) {
            return Ok(Err(format!(
                "{schema}.{table} has no primary key, so rows can't be identified for updates."
            )));
        }

        let attribute_for = |index: usize| {
            let column = columns.get(index)?;
            if column.table_oid() != Some(table_oid) {
                return None;
            }
            let number = column.column_id()?;
            attributes
                .iter()
                .find(|attribute| attribute.number == number)
        };

        let mut key_columns = Vec::new();
        for attribute in attributes.iter().filter(|attribute| attribute.primary_key) {
            let position = (0..columns.len()).find(|index| {
                attribute_for(*index).is_some_and(|candidate| candidate.number == attribute.number)
            });
            match position {
                Some(position) => key_columns.push(position),
                None => {
                    return Ok(Err(format!(
                        "Primary key column {} isn't in the result; select it to enable editing.",
                        attribute.name
                    )));
                }
            }
        }

        let edit_columns = (0..columns.len())
            .map(|index| {
                let attribute = attribute_for(index)?;
                let column_type = columns[index].type_();
                Some(EditColumn {
                    name: attribute.name.clone(),
                    type_name: Some(column_type.name().to_string()),
                    type_schema: Some(column_type.schema().to_string()),
                })
            })
            .collect();
        let writable = (0..columns.len())
            .map(|index| attribute_for(index).is_some_and(|attribute| !attribute.read_only))
            .collect();

        Ok(Ok(EditTarget {
            table: TableRef {
                schema: Some(schema),
                name: table,
            },
            columns: edit_columns,
            writable,
            key_columns,
        }))
    }

    pub async fn apply(&mut self, statements: &[ParameterizedStatement]) -> Result<Vec<u64>> {
        let transaction = self.client.transaction().await?;
        let mut counts = Vec::with_capacity(statements.len());
        for statement in statements {
            let params = statement
                .params
                .iter()
                .map(|param| param as &(dyn ToSql + Sync))
                .collect::<Vec<_>>();
            let affected = transaction
                .execute(statement.sql.as_str(), &params)
                .await
                .with_context(|| format!("running `{}`", statement.sql))?;
            check_affected(statement, affected)?;
            counts.push(affected);
        }
        transaction.commit().await?;
        Ok(counts)
    }
}

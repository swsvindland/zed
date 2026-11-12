use std::{pin::pin, time::Duration};

use anyhow::{Context as _, Result};
use futures::{Stream, StreamExt as _, future};
use postgres_native_tls::MakeTlsConnector;
use tokio_postgres::{SimpleQueryMessage, config::SslMode, types::ToSql};

use crate::{
    ColumnInfo, ColumnSummary, ConnectionProfile, DatabaseObject, EditTarget, Editability,
    ObjectKind, SessionReset,
    catalog::{Catalog, ObjectName},
    check_affected,
    execution::{Execution, ExecutionEnd, Item, ReadEnd},
    sql::{EditColumn, ParameterizedStatement, TableRef},
};

/// How long the rest of a cancelled statement may take to arrive.
const CANCEL_TIMEOUT: Duration = Duration::from_secs(30);

pub struct PostgresClient {
    client: tokio_postgres::Client,
    /// Kept for cancel requests, which open their own connection.
    tls: MakeTlsConnector,
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
        let tls = MakeTlsConnector::new(connector);
        let (client, connection) = config
            .connect(tls.clone())
            .await
            .with_context(|| format!("connecting to {}", profile.host))?;
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                log::error!("postgres connection closed: {error}");
            }
        });
        Ok(Self { client, tls })
    }

    pub async fn execute(&mut self, sql: &str, execution: &mut Execution) -> Result<ExecutionEnd> {
        let stream = match execution.guard(self.client.simple_query_raw(sql)).await {
            Ok(stream) => stream?,
            Err(interrupt) => return Ok(interrupt.into()),
        };
        let mut items = pin!(items(stream));
        let interrupt = match execution.read(&mut items).await? {
            ReadEnd::Exhausted => return Ok(ExecutionEnd::Completed),
            ReadEnd::Interrupted(interrupt) => interrupt,
        };
        // The cancel goes over a connection of its own, so this session keeps its place in the
        // protocol; reading on to the end leaves it ready for the next statement.
        if let Err(error) = self
            .client
            .cancel_token()
            .cancel_query(self.tls.clone())
            .await
        {
            log::warn!("couldn't send a cancel request: {error}");
        }
        // The deadline is checked between rows as well, since a timeout only fires while its
        // future waits and rows that are already buffered arrive without waiting.
        let deadline = tokio::time::Instant::now() + CANCEL_TIMEOUT;
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout_at(deadline, items.next()).await {
                Ok(None) => return Ok(interrupt.into()),
                Ok(Some(Err(error)))
                    if error
                        .downcast_ref::<tokio_postgres::Error>()
                        .is_some_and(tokio_postgres::Error::is_closed) =>
                {
                    return Err(SessionReset(format!("{error:#}")).into());
                }
                // Rows still in flight, and the `query_canceled` error that ends the statement.
                Ok(Some(_)) => {}
                Err(_) => break,
            }
        }
        Err(SessionReset(format!(
            "PostgreSQL didn't stop the statement within {} s",
            CANCEL_TIMEOUT.as_secs()
        ))
        .into())
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

    pub async fn catalog(&mut self) -> Result<Catalog> {
        let default_schema = self
            .client
            .query_one("SELECT current_schema()::text", &[])
            .await?
            .try_get::<_, Option<String>>(0)?;
        let schemas = self.list_schemas().await?;
        let column_rows = self
            .client
            .query(
                "SELECT n.nspname::text, c.relname::text, c.relkind::text, a.attname::text, \
                   format_type(a.atttypid, a.atttypmod), NOT a.attnotnull, \
                   COALESCE(a.attnum = ANY(i.indkey), false) \
                 FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped \
                 LEFT JOIN pg_index i ON i.indrelid = c.oid AND i.indisprimary \
                 WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') \
                   AND n.nspname NOT LIKE 'pg\\_%' AND n.nspname <> 'information_schema' \
                 ORDER BY n.nspname, c.relname, a.attnum",
                &[],
            )
            .await?;
        let mut objects = Vec::with_capacity(column_rows.len());
        for row in &column_rows {
            let kind: String = row.try_get(2)?;
            objects.push((
                ObjectName {
                    schema: row.try_get(0)?,
                    name: row.try_get(1)?,
                },
                if kind == "v" || kind == "m" {
                    ObjectKind::View
                } else {
                    ObjectKind::Table
                },
                ColumnSummary {
                    name: row.try_get(3)?,
                    type_name: row.try_get(4)?,
                    nullable: row.try_get(5)?,
                    primary_key: row.try_get(6)?,
                },
            ));
        }
        let key_rows = self
            .client
            .query(
                "SELECT con.conname::text, pn.nspname::text, pc.relname::text, pa.attname::text, \
                   rn.nspname::text, rc.relname::text, ra.attname::text \
                 FROM pg_constraint con \
                 JOIN pg_class pc ON pc.oid = con.conrelid \
                 JOIN pg_namespace pn ON pn.oid = pc.relnamespace \
                 JOIN pg_class rc ON rc.oid = con.confrelid \
                 JOIN pg_namespace rn ON rn.oid = rc.relnamespace \
                 CROSS JOIN LATERAL unnest(con.conkey, con.confkey) WITH ORDINALITY AS k(attnum, refattnum, ordinal) \
                 JOIN pg_attribute pa ON pa.attrelid = con.conrelid AND pa.attnum = k.attnum \
                 JOIN pg_attribute ra ON ra.attrelid = con.confrelid AND ra.attnum = k.refattnum \
                 WHERE con.contype = 'f' \
                 ORDER BY con.oid, k.ordinal",
                &[],
            )
            .await?;
        let mut keys = Vec::with_capacity(key_rows.len());
        for row in &key_rows {
            keys.push((
                row.try_get(0)?,
                ObjectName {
                    schema: row.try_get(1)?,
                    name: row.try_get(2)?,
                },
                row.try_get(3)?,
                ObjectName {
                    schema: row.try_get(4)?,
                    name: row.try_get(5)?,
                },
                row.try_get(6)?,
            ));
        }
        Ok(Catalog {
            default_schema,
            schemas,
            objects: crate::catalog::group_columns(objects),
            foreign_keys: crate::catalog::group_foreign_keys(keys),
        })
    }

    pub async fn editability(&mut self, sql: &str) -> Result<Editability> {
        let statement = match self.client.prepare(sql).await {
            Ok(statement) => statement,
            Err(error) => return Ok(Err(format!("Couldn't describe the query: {error}"))),
        };
        let columns = statement.columns();
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

/// Maps the simple query protocol onto driver items. A command without a row description
/// (an `UPDATE`, say) reports its row count instead.
fn items(stream: tokio_postgres::SimpleQueryStream) -> impl Stream<Item = Result<Item>> {
    stream
        .scan(false, |in_result_set, message| {
            let item = match message {
                Err(error) => Some(Err(error.into())),
                Ok(SimpleQueryMessage::RowDescription(columns)) => {
                    *in_result_set = true;
                    Some(Ok(Item::Columns(
                        columns
                            .iter()
                            .map(|column| ColumnInfo {
                                name: column.name().to_string(),
                                type_name: String::new(),
                            })
                            .collect(),
                    )))
                }
                Ok(SimpleQueryMessage::Row(row)) => Some(Ok(Item::Row(
                    (0..row.len())
                        .map(|index| row.get(index).map(ToOwned::to_owned))
                        .collect(),
                ))),
                Ok(SimpleQueryMessage::CommandComplete(count)) => {
                    let had_rows = std::mem::replace(in_result_set, false);
                    (!had_rows).then_some(Ok(Item::RowsAffected(count)))
                }
                Ok(_) => None,
            };
            future::ready(Some(item))
        })
        .filter_map(future::ready)
}

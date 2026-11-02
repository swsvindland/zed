//! Database connections, drivers and query execution for Zed's database tools.
//!
//! Drivers are async and expect to run on a Tokio runtime (see `gpui_tokio`).

mod connections;
mod postgres;
pub mod sql;
mod sql_server;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::Result;

pub use connections::*;
use sql::{EditColumn, ParameterizedStatement, TableRef};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    /// Database type name for display, e.g. `nvarchar` or `int4`. Empty when unknown.
    pub type_name: String,
}

/// Rows of one result set. Values are already rendered as text; `None` is SQL `NULL`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResultSet {
    pub columns: Vec<ColumnInfo>,
    pub rows: Vec<Vec<Option<String>>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatementOutcome {
    Rows(ResultSet),
    RowsAffected(u64),
    /// The statement completed but the driver can't report a row count.
    Completed,
}

#[derive(Clone, Debug)]
pub struct QueryOutput {
    pub outcomes: Vec<StatementOutcome>,
    pub elapsed: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ObjectKind {
    Table,
    View,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseObject {
    pub name: String,
    pub kind: ObjectKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnSummary {
    pub name: String,
    pub type_name: String,
    pub nullable: bool,
    pub primary_key: bool,
}

/// Describes how a result set maps back onto a single table, which is what makes it editable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditTarget {
    pub table: TableRef,
    /// The table column behind each result column. `None` for expressions and other tables.
    pub columns: Vec<Option<EditColumn>>,
    /// Per result column: whether it may be changed. Identity, computed and expression columns
    /// are read-only, though an identity key still identifies rows.
    pub writable: Vec<bool>,
    /// Result column indices of the table's primary key.
    pub key_columns: Vec<usize>,
}

/// Either the result can be edited, or the reason it can't, phrased for the user.
pub type Editability = std::result::Result<EditTarget, String>;

pub enum Client {
    SqlServer(sql_server::SqlServerClient),
    Postgres(postgres::PostgresClient),
}

impl Client {
    pub async fn connect(
        profile: &ConnectionProfile,
        database: Option<&str>,
        password: Option<&str>,
    ) -> Result<Self> {
        let database = database
            .filter(|database| !database.is_empty())
            .or(profile.default_database.as_deref())
            .filter(|database| !database.is_empty());
        Ok(match profile.driver {
            DriverKind::SqlServer => Client::SqlServer(
                sql_server::SqlServerClient::connect(profile, database, password).await?,
            ),
            DriverKind::Postgres => Client::Postgres(
                postgres::PostgresClient::connect(profile, database, password).await?,
            ),
        })
    }

    pub async fn run(&mut self, sql: &str) -> Result<QueryOutput> {
        let start = Instant::now();
        let outcomes = match self {
            Client::SqlServer(client) => client.run(sql).await?,
            Client::Postgres(client) => client.run(sql).await?,
        };
        Ok(QueryOutput {
            outcomes,
            elapsed: start.elapsed(),
        })
    }

    pub async fn list_databases(&mut self) -> Result<Vec<String>> {
        match self {
            Client::SqlServer(client) => client.list_databases().await,
            Client::Postgres(client) => client.list_databases().await,
        }
    }

    pub async fn list_schemas(&mut self) -> Result<Vec<String>> {
        match self {
            Client::SqlServer(client) => client.list_schemas().await,
            Client::Postgres(client) => client.list_schemas().await,
        }
    }

    pub async fn list_objects(&mut self, schema: &str) -> Result<Vec<DatabaseObject>> {
        match self {
            Client::SqlServer(client) => client.list_objects(schema).await,
            Client::Postgres(client) => client.list_objects(schema).await,
        }
    }

    pub async fn list_columns(&mut self, schema: &str, table: &str) -> Result<Vec<ColumnSummary>> {
        match self {
            Client::SqlServer(client) => client.list_columns(schema, table).await,
            Client::Postgres(client) => client.list_columns(schema, table).await,
        }
    }

    /// Works out whether the result of `sql` can be edited in place. Only a single statement
    /// reading from one table that has a primary key, with every key column selected, qualifies.
    pub async fn editability(&mut self, sql: &str, result: &ResultSet) -> Result<Editability> {
        let statements = sql::split_statements(sql, self.dialect());
        if statements.len() != 1 {
            return Ok(Err(
                "Results can only be edited when a single statement was run.".into(),
            ));
        }
        match self {
            Client::SqlServer(client) => client.editability(sql, result).await,
            Client::Postgres(client) => client.editability(sql, result).await,
        }
    }

    /// Runs the statements in one transaction, rolling back if any fails or if an update or
    /// delete doesn't touch exactly one row. Returns the affected row count of each statement.
    pub async fn apply(&mut self, statements: &[ParameterizedStatement]) -> Result<Vec<u64>> {
        match self {
            Client::SqlServer(client) => client.apply(statements).await,
            Client::Postgres(client) => client.apply(statements).await,
        }
    }

    pub fn dialect(&self) -> sql::Dialect {
        match self {
            Client::SqlServer(_) => sql::Dialect::SqlServer,
            Client::Postgres(_) => sql::Dialect::Postgres,
        }
    }
}

fn check_affected(statement: &ParameterizedStatement, affected: u64) -> Result<()> {
    if statement.expect_one_row && affected != 1 {
        anyhow::bail!(
            "{affected} rows were affected by `{}` (expected exactly 1). The row may have been \
             changed or deleted since it was read. All changes were rolled back.",
            statement.sql
        );
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PoolKey {
    pub connection_id: String,
    /// Empty for the connection's default database.
    pub database: String,
    /// Separate sessions keep a console's transaction and temp tables to itself, and keep
    /// metadata browsing from waiting on a long-running query.
    pub session: String,
}

/// Errors after which a session can't be trusted, so the pool should reconnect.
pub fn is_connection_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        if let Some(error) = cause.downcast_ref::<tiberius::error::Error>() {
            return matches!(
                error,
                tiberius::error::Error::Io { .. }
                    | tiberius::error::Error::Tls(_)
                    | tiberius::error::Error::Protocol(_)
                    | tiberius::error::Error::Routing { .. }
            );
        }
        if let Some(error) = cause.downcast_ref::<tokio_postgres::Error>() {
            return error.is_closed();
        }
        cause.is::<std::io::Error>()
    })
}

/// Live clients keyed by connection, database and session. Each client is serialized behind an
/// async mutex because it is a single protocol session.
#[derive(Default)]
pub struct ConnectionPool {
    clients: Mutex<HashMap<PoolKey, Arc<tokio::sync::Mutex<Client>>>>,
}

impl ConnectionPool {
    pub fn get(&self, key: &PoolKey) -> Option<Arc<tokio::sync::Mutex<Client>>> {
        self.clients.lock().ok()?.get(key).cloned()
    }

    pub async fn get_or_connect(
        &self,
        profile: &ConnectionProfile,
        key: &PoolKey,
        password: Option<&str>,
    ) -> Result<Arc<tokio::sync::Mutex<Client>>> {
        if let Some(client) = self.get(key) {
            return Ok(client);
        }
        let key = key.clone();
        let client = Arc::new(tokio::sync::Mutex::new(
            Client::connect(profile, Some(&key.database), password).await?,
        ));
        if let Ok(mut clients) = self.clients.lock() {
            clients.insert(key, client.clone());
        }
        Ok(client)
    }

    /// Drops the client so the next use reconnects. Called after errors that may have left the
    /// session in an unknown state.
    pub fn discard(&self, key: &PoolKey) {
        if let Ok(mut clients) = self.clients.lock() {
            clients.remove(key);
        }
    }

    pub fn discard_connection(&self, connection_id: &str) {
        if let Ok(mut clients) = self.clients.lock() {
            clients.retain(|key, _| key.connection_id != connection_id);
        }
    }

    pub fn discard_session(&self, session: &str) {
        if let Ok(mut clients) = self.clients.lock() {
            clients.retain(|key, _| key.session != session);
        }
    }

    pub fn is_connected(&self, connection_id: &str) -> bool {
        self.clients
            .lock()
            .is_ok_and(|clients| clients.keys().any(|key| key.connection_id == connection_id))
    }
}

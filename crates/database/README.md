# database

Connections, drivers and query execution behind Zed's database tools (`database_ui`).
It has no UI; drivers are async and run on the Tokio runtime from `gpui_tokio`.

- **Profiles** (`ConnectionProfile`, `ConnectionsFile`) live in
  `<config dir>/database_connections.json`, grouped by a free-form `environment`
  (`dev`, `qa`, `prod`, …). A profile can limit which databases on the server it shows
  (`databases`) and which schemas in each (`schemas`); empty shows everything.
  Names starting with `prod` (or `production: true`) mark a production connection.
- **Passwords** never go in that file. `read_password` / `write_password` store them in the
  OS credential store through `zed_credentials_provider`, keyed `zed-database://<profile id>`.
  Development builds use Zed's development credentials file unless
  `ZED_DEVELOPMENT_USE_KEYCHAIN` is set.
- **SQL Server** uses `tiberius`: TCP (`host`, `host,port`), named instances (`host\instance`,
  via SQL Browser), named pipes (`np:\\.\pipe\…`) and LocalDB (`(localdb)\MSSQLLocalDB`,
  started with `sqllocaldb` and reached over its pipe). Windows integrated authentication or
  SQL logins. On Azure SQL Database every database the login can see in `master` is listed,
  since `HAS_DBACCESS` isn't available there.
- **PostgreSQL** uses `tokio-postgres` with `native-tls` (`sslmode=prefer`, or disabled when
  `encrypt` is off).
- **`Client`** runs scripts (multiple result sets, affected-row counts), lists databases /
  schemas / tables / views / columns, decides whether a result is editable
  (`editability`: one statement, one table, primary key present in the result; it describes
  the statement without running it) and applies edits in a single transaction (`apply`),
  rolling back if an update or delete doesn't hit exactly one row. On SQL Server, when the
  session already has a transaction open (a console's `BEGIN TRANSACTION`), `apply` takes a
  savepoint instead: a failed push rolls back only to it, a successful one is committed with
  that transaction (`Applied::in_open_transaction`), and an error that ended or doomed the
  transaction says so. On PostgreSQL `apply` still sends its own `BEGIN` and `COMMIT`, which
  commits or aborts an open transaction block; it's unchanged because there's no live
  PostgreSQL test to check a fix against. Values come back as text,
  with `None` for SQL `NULL`.
- **Running statements** (`execution`, `Client::execute`): rows are sent as
  `ExecutionEvent`s while they arrive, and each result set pauses at the profile's
  `fetch_limit` (default `DEFAULT_FETCH_LIMIT`, 500; `0` reads everything) with the server's
  result left open. The `ExecutionHandle` asks for the next page, for everything, or cancels;
  dropping it cancels too. While a result is open its session is busy.
  - On SQL Server every run is sent as a SQL batch, so temp tables, `USE`, `SET` options and
    transactions stay with the session between runs. (Tiberius's `Client::execute` goes
    through `sp_executesql`, which scopes all of those to the call; `BEGIN TRANSACTION` alone
    fails there with error 266.) Tiberius 0.13 drops the `DONE` tokens that carry a batch's
    per-statement row counts, so the count is read back afterwards with
    `SELECT ROWCOUNT_BIG()` in a batch of its own. That only knows the last statement's count,
    so a count is reported when a run's last statement is its only data change (`INSERT`,
    `UPDATE`, `DELETE` or `MERGE`, also after `WITH`), including under `SET NOCOUNT ON`.
    Other runs report completion without a count, and `@@ROWCOUNT` in the next run reads the
    probe's 1. A tiberius change that kept the `DONE` counts on `QueryStream` would give every
    statement its own count.
  - `GO` lines are client-side separators that SQL Server rejects, so a run is first split into
    batches (`sql::split_batches`) and they're sent one after another on the same session, as
    SSMS and sqlcmd do. `GO n` sends the batch before it `n` times. Each batch gets the row
    count check above on its own. The first batch that fails ends the run, with the batch
    named in the error (`running batch 3 of 4 (from line 5)`); what earlier batches returned
    has already been sent as events. A batch starts at the line after its `GO`, so SQL Server's
    line numbers count from there, as in SSMS. `editability` describes a script only when it is
    a single batch that runs once. PostgreSQL has no `GO` and runs the text as it is.
  - `query_timeout_seconds` cancels a statement whose server work runs longer. It covers a
    whole run, all of a script's batches together. Time spent paused at the fetch limit
    doesn't count. A stop or timeout that lands between two batches ends the run there, with
    nothing to cancel on the server.
  - Cancelling on SQL Server reads a small remainder to the end if it's already on the wire,
    and otherwise sends a TDS Attention and waits for the server to acknowledge it, keeping
    the session (temp tables, open transactions) on encrypted and unencrypted connections
    alike. SQL Server acknowledges an Attention in a message of its own after the cancelled
    response, which tiberius 0.13 doesn't read, so the workspace uses a patched copy
    (`tooling/patches/tiberius`, see its `PATCHES.md`). Only a cancel the server doesn't
    acknowledge within 30 s, or a broken connection, closes the session (`SessionReset`) and
    makes the pool reconnect.
  - Cancelling on PostgreSQL sends a `CancelToken` request over its own connection and reads
    the statement to its end, keeping the session.
  - Tiberius's own `command_timeout` is turned off: when it fires it leaves the session out of
    sync with the server.
  - `Client::run` is the uninterruptible form, collecting everything.
- **Completion** (`catalog`, `completion`): `Client::catalog` reads a database's schemas,
  tables and views with their columns and types, and foreign keys, in a few bulk queries.
  `completion::complete` works out what the cursor is in and offers candidates:
  - tables, views and schemas after `FROM`, `JOIN`, `UPDATE`, `INTO` or a comma in a `FROM`
    list;
  - a table's columns after `alias.`, `table.` or `schema.table.`, and a schema's objects after
    `schema.`;
  - after `JOIN`, whole joins from foreign keys (`Customers c ON c.Id = o.CustomerId`), and
    after `ON`, the conditions alone;
  - elsewhere, the columns of every table in the statement (qualified when ambiguous), its
    aliases and keywords.

  Names are quoted where the dialect needs it, including mixed-case names on PostgreSQL.
  `completion::may_change_schema` spots DDL, so callers know to read the catalog again.
- **`ConnectionPool`** keeps one session per connection, database and session name.
- **`sql`** splits scripts into statements (`;`, blank lines, `GO`, aware of strings, comments,
  brackets, `BEGIN … END` and dollar quotes) and SQL Server scripts into batches
  (`split_batches`: `GO` or `GO n` alone on a line, optionally with a trailing `--` comment,
  outside strings, quoted identifiers and nested block comments), and generates parameterized
  `UPDATE` / `INSERT` / `DELETE` statements. Values are always bound parameters, never
  concatenated into SQL text.

## Tests

`cargo test -p database` runs the unit tests, which include paging, cancelling and timeouts
against simulated result streams, and completion (alias resolution, join conditions, quoting).
To also run the end-to-end tests against a real SQL Server, point them at a server reachable
with integrated authentication. They create a `ZedDatabaseTest` database and check cancelling
on the server (including the session surviving it), paging and timeouts, that temp tables,
`USE`, `SET` options and open transactions carry over between runs with row counts reported,
and that `GO` scripts run batch by batch: `CREATE PROCEDURE` after a `GO`, `GO n`, an error in
a later batch, and a stop or timeout spanning batches.

```sh
ZED_DATABASE_TEST_SQL_SERVER='(localdb)\MSSQLLocalDB' cargo test -p database --test local_db
```

The PostgreSQL driver has no live test yet.

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
  rolling back if an update or delete doesn't hit exactly one row. Values come back as text,
  with `None` for SQL `NULL`.
- **Running statements** (`execution`, `Client::execute`): rows are sent as
  `ExecutionEvent`s while they arrive, and each result set pauses at the profile's
  `fetch_limit` (default `DEFAULT_FETCH_LIMIT`, 500; `0` reads everything) with the server's
  result left open. The `ExecutionHandle` asks for the next page, for everything, or cancels;
  dropping it cancels too. While a result is open its session is busy.
  - `query_timeout_seconds` cancels a statement whose server work runs longer. Time spent
    paused at the fetch limit doesn't count.
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
  brackets, `BEGIN … END` and dollar quotes), and generates parameterized
  `UPDATE` / `INSERT` / `DELETE` statements. Values are always bound parameters, never
  concatenated into SQL text.

## Tests

`cargo test -p database` runs the unit tests, which include paging, cancelling and timeouts
against simulated result streams, and completion (alias resolution, join conditions, quoting).
To also run the end-to-end tests against a real SQL Server, point them at a server reachable
with integrated authentication. They create a `ZedDatabaseTest` database and check cancelling
on the server (including the session surviving it), paging and timeouts.

```sh
ZED_DATABASE_TEST_SQL_SERVER='(localdb)\MSSQLLocalDB' cargo test -p database --test local_db
```

The PostgreSQL driver has no live test yet.

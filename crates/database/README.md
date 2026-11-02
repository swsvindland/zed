# database

Connections, drivers and query execution behind Zed's database tools (`database_ui`).
It has no UI; drivers are async and run on the Tokio runtime from `gpui_tokio`.

- **Profiles** (`ConnectionProfile`, `ConnectionsFile`) live in
  `<config dir>/database_connections.json`, grouped by a free-form `environment`
  (`dev`, `qa`, `prod`, …). A profile can list several databases on one server.
  Names starting with `prod` (or `production: true`) mark a production connection.
- **Passwords** never go in that file. `read_password` / `write_password` store them in the
  OS credential store through `zed_credentials_provider`, keyed `zed-database://<profile id>`.
  Development builds use Zed's development credentials file unless
  `ZED_DEVELOPMENT_USE_KEYCHAIN` is set.
- **SQL Server** uses `tiberius`: TCP (`host`, `host,port`), named instances (`host\instance`,
  via SQL Browser), named pipes (`np:\\.\pipe\…`) and LocalDB (`(localdb)\MSSQLLocalDB`,
  started with `sqllocaldb` and reached over its pipe). Windows integrated authentication or
  SQL logins.
- **PostgreSQL** uses `tokio-postgres` with `native-tls` (`sslmode=prefer`, or disabled when
  `encrypt` is off).
- **`Client`** runs scripts (multiple result sets, affected-row counts), lists databases /
  schemas / tables / views / columns, decides whether a result is editable
  (`editability`: one statement, one table, primary key present in the result) and applies
  edits in a single transaction (`apply`), rolling back if an update or delete doesn't hit
  exactly one row.
- **`ConnectionPool`** keeps one session per connection, database and session name.
- **`sql`** splits scripts into statements (`;`, blank lines, `GO`, aware of strings, comments,
  brackets, `BEGIN … END` and dollar quotes), and generates parameterized
  `UPDATE` / `INSERT` / `DELETE` statements. Values are always bound parameters, never
  concatenated into SQL text.

## Tests

`cargo test -p database` runs the unit tests. To also run the end-to-end test against a real
SQL Server (it creates a `ZedDatabaseTest` database), point it at a server reachable with
integrated authentication:

```sh
ZED_DATABASE_TEST_SQL_SERVER='(localdb)\MSSQLLocalDB' cargo test -p database --test local_db
```

The PostgreSQL driver has no live test yet.

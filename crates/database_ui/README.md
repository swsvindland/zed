# database_ui

DataGrip-style database tools for Zed, built on the `database` crate.

## Database panel

`database: toggle focus` opens a dock panel with a tree:
environment → connection → database → schema → tables / views → columns, loaded lazily.
Environments are colored (dev green, qa yellow, prod red) and connected servers get a green icon.

- **+** (`database: new connection`) opens a form: driver, environment, host, port,
  Windows or password authentication, databases to show (empty shows all), default database,
  fetch limit and query timeout, encryption and certificate trust, and a production override.
  **Test Connection** checks it.
  Passwords go to the OS credential store, never the JSON file.
- Expanded connections and databases show a badge such as **5 of 19**. Click it (or right-click
  and pick **Choose Databases…** / **Choose Schemas…**) for a checklist of what the server has;
  the choice is applied when the list closes and saved to the profile (`databases`, `schemas`).
- The gear opens `database_connections.json` for hand editing; the refresh button reloads it.
- Right-click a connection or database for **New Query Console**, **Use for Current Editor**,
  **Edit Connection…**, **Disconnect** and **Delete Connection**. Double-click a table (or use
  **Select Top 100 Rows**) to open its rows.

## Consoles and results

A query console is a `.sql` file under `<data dir>/database_consoles/<connection>/<database>.sql`,
so it persists and keeps its connection. Any other `.sql` file can be bound with
`database: select connection` or the toolbar button above SQL editors.

- `ctrl-enter` (`cmd-enter` on macOS), `database: run statement`, runs the selection or the
  statement under the cursor. `ctrl-shift-enter`, `database: run script`, runs the whole file.
- On SQL Server, `GO` lines split a script (or a selection) into batches, as in SSMS and
  sqlcmd. The batches run one after another on the console's session, and `GO 5` runs the
  batch before it five times. The first batch that fails ends the run: the results and row
  counts of the batches before it stay, and the error names the batch and the line it starts
  on, counted in what ran. SQL Server's own line numbers in the error count from that line.
  Stop and the query timeout apply to the whole run.
- Results open in a pane below the editor: a grid per result set (sortable and filterable,
  from `tabular_data_preview`), plus a **Messages** tab with row counts, timing and errors.
  Each console has its own session, so transactions, temp tables, `USE` and `SET` options carry
  over between runs. After a `USE`, completion still offers the console's own database.
  SQL Server reports a row count only when a run's last statement is its only `INSERT`,
  `UPDATE`, `DELETE` or `MERGE`; see the `database` README.
- Rows stream in a page at a time. Each result set stops at the connection's fetch limit
  (500 rows unless set in the connection form; 0 fetches everything) and its tab shows
  **500+**. **Next 500** (`database: fetch next page`) and **All** (`database: fetch all rows`)
  read more from the result, which stays open on the server until then. The console's session
  is busy meanwhile, so running another statement or pushing edits closes it first.
- **Stop** (`ctrl-f2` / `cmd-f2`, `database: stop statement`, or the red button in the console
  toolbar and the results header) cancels the running statement, or closes an open result.
  Rows already read stay in the grid. A connection's query timeout cancels the same way.
  The console's session survives a stop, so temp tables and open transactions are kept.
- `NULL` shows as a muted `<null>`, distinct from an empty string, in the grid and its filters.
- Zed has no SQL grammar of its own, so this crate registers one (`tree-sitter-sequel`) unless
  a SQL language is already registered.

## Completion

Consoles and `.sql` editors with a connection get completion from the database itself,
through the editor's completion-provider hook rather than a SQL language server. Results from
language servers and snippets still appear, after these.

- Metadata is read the first time a database's console asks, over the panel's metadata
  session, and cached per connection and database.
- It's read again after a console runs DDL (`CREATE`, `ALTER`, `DROP`, `sp_rename`,
  `SELECT … INTO`), when the database's node is refreshed in the panel, on
  `database: refresh completion metadata`, and when connections are reloaded or edited.
- See the `database` README for what is offered where. While metadata is loading or failed
  to load, only keywords are offered.

## Editing results

When the statement reads from one table with a primary key and every key column is selected,
the grid is editable:

- Double-click a cell, press Enter or F2, or start typing to edit; Enter commits, Escape
  cancels. Changed cells are highlighted until pushed. Opening a `NULL` cell and leaving it
  without typing keeps it `NULL`; **Set NULL** writes `NULL` back.
- A result paused at the fetch limit can be edited too; the rows not yet fetched aren't
  affected.
- **Add Row**, **Delete Selected Row** (toggle), **Set NULL** and **Revert** work on the
  pending changes.
- **Push** shows the generated `UPDATE … WHERE <key>`, `INSERT` and `DELETE` statements with
  their bound parameter values, then runs them in one transaction and re-runs the query.
  On SQL Server, a push into a transaction left open in the console joins it: a failed push
  undoes only its own changes, and nothing is saved until the console commits, which the
  notice after the push points out. (PostgreSQL still commits the open transaction; see the
  `database` README.) Production connections get a red header and need an explicit acknowledgement before pushing.

When a result can't be edited (joins, several statements, no primary key, key not selected,
views without a single base table), the grid says why. Identity and computed columns are
read-only.

## Tests

With `ZED_DATABASE_TEST_SQL_SERVER` set (see the `database` README), `cargo test -p database_ui`
also drives the panel, a console, the grid, paging, Stop, NULL editing, completion, a push and
a script with `GO` batches against that server, in a `ZedDatabaseUiTest` database.

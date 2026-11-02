# database_ui

DataGrip-style database tools for Zed, built on the `database` crate.

## Database panel

`database: toggle focus` opens a dock panel with a tree:
environment → connection → database → schema → tables / views → columns, loaded lazily.
Environments are colored (dev green, qa yellow, prod red) and connected servers get a green icon.

- **+** (`database: new connection`) opens a form: driver, environment, host, port,
  Windows or password authentication, databases to show (empty shows all), default database,
  encryption and certificate trust, and a production override. **Test Connection** checks it.
  Passwords go to the OS credential store, never the JSON file.
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
- Results open in a pane below the editor: a grid per result set (sortable and filterable,
  from `tabular_data_preview`), plus a **Messages** tab with row counts, timing and errors.
  Each console has its own session, so transactions and temp tables carry over between runs.
- Zed has no SQL grammar of its own, so this crate registers one (`tree-sitter-sequel`) unless
  a SQL language is already registered.

## Editing results

When the statement reads from one table with a primary key and every key column is selected,
the grid is editable:

- Double-click a cell, press Enter or F2, or start typing to edit; Enter commits, Escape
  cancels. Changed cells are highlighted until pushed.
- **Add Row**, **Delete Selected Row** (toggle), **Set NULL** and **Revert** work on the
  pending changes.
- **Push** shows the generated `UPDATE … WHERE <key>`, `INSERT` and `DELETE` statements with
  their bound parameter values, then runs them in one transaction and re-runs the query.
  Production connections get a red header and need an explicit acknowledgement before pushing.

When a result can't be edited (joins, several statements, no primary key, key not selected,
views without a single base table), the grid says why. Identity and computed columns are
read-only.

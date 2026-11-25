# Export query results: design

## Why

Save what a SQL console run returned to a file, mainly so Claude Code can read it. People can open the same file too, for example a CSV in Excel. This is the export half of FORK_ROADMAP.md 2.4.

## Goals

- An **Export…** button in the results header, plus a `database: export results` action, opens a save dialog.
- The file extension picks the format: `.md` (the default), `.json`, `.csv` or `.tsv`.
- The data is complete. If the run is paused at the fetch limit, export fetches the remaining rows before writing.
- Every format keeps SQL `NULL` separate from an empty string.

## Non-goals

- Exporting or copying a range selection (the other half of 2.4)
- SQL `INSERT` output
- Query history
- A command-line runner

## Anti-goals

- A file that looks complete but isn't
- Freezing the UI while a large result is written

## Approaches

- **A. Export… with a save dialog (picked).** You choose the path and format each time. It works for Claude Code and for people, and the dialog opens in the project folder, so the Claude Code case takes two clicks.
- **B. One-click "Save for Claude".** It writes to `query-results/` in the project with no dialog. It's faster, but useful only for that one case.
- **C. A command-line runner on the `database` crate.** Claude Code would run scripts itself. Deferred for now; it can reuse this formatting code later.

## Design

**Button.** An icon button in the results header, next to the result tabs. It's enabled when the run has at least one result set and isn't Running or Stopping.

**Dialog.**
- It calls `cx.prompt_for_new_path` and opens in the first worktree root of the workspace. Without one, it opens in the home directory.
- The suggested name is `<database>-<yyyymmdd-hhmmss>.md`, so exports don't overwrite each other. *(my call)*
- An unknown extension is written as Markdown. *(my call)*

**What gets written.**
- `.md` and `.json` write every result set of the run, plus the rows-affected counts and messages.
- `.csv` and `.tsv` can hold only one table, so they write the result tab that's open. If the Messages tab is open, they write Result 1. *(my call)*
- Rows are written as the database returned them, in query order. Unsaved grid edits, grid sorting and grid filters are not applied. *(my call: the file is the query's result, and `ORDER BY` is how you control order)*

**Formats.**
- **Markdown.**
  - A header lists the connection, environment, database, start time, elapsed time and run status, followed by the SQL in a ` ```sql ` block.
  - Each result set gets a `## Result N (R rows)` heading and a pipe table.
  - `NULL` is written as `NULL`, and an empty string as an empty cell.
  - `|` is escaped as `\|`, and newlines inside a value become `<br>`.
  - A `## Messages` section ends the file.
- **JSON.**
  - The top-level fields are `connection`, `environment`, `database`, `started_at`, `elapsed_ms`, `status`, `sql`, `result_sets` and `messages`.
  - Each result set is `{ "columns": [{ "name", "type" }], "rows": [[…]] }`.
  - Each row is an array, and NULL is `null`.
- **CSV and TSV.**
  - The first row holds the column names.
  - Values are quoted as RFC 4180 describes, with a comma or a tab as the separator.
  - `NULL` is an empty field with no quotes, and an empty string is `""`.

**Paused runs.** Export asks for the path first. It then fetches every remaining row, the same as clicking **All**, which also runs the rest of the script. The file is written when the run finishes. If the run is stopped or fails before then, nothing is written and the notice says why.

**Stopped or failed runs with partial results.** Export writes what's there. The Markdown and JSON status says the run was stopped or failed, so the rows may be incomplete.

**Writing.** The file is written on a background thread. A workspace toast then shows "Exported N rows to <path>", or the error.

**Code.**
- Formatting lives in `crates/database_ui/src/export.rs` as pure functions with unit tests for each format: NULL versus empty string, escaping, and several result sets.
- A results-view test covers a paused run: export fetches every row, and the file contains all of them.

## Left to the builder

- The icon
- The toast wording
- How JSON whitespace is laid out
- Whether the action gets a default keybinding

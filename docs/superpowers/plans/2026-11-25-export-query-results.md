# Export Query Results Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an **Export…** button and a `database: export results` action to the SQL results pane. They save a run's results to a Markdown, JSON, CSV or TSV file chosen in a save dialog.

**Architecture:**
- A new `crates/database_ui/src/export.rs` holds pure functions that turn a snapshot of a run (`ExportData`) into file text.
- `results_view.rs` takes the snapshot and shows the save dialog. On a paused run it fetches every remaining row first. It writes the file on a background thread and reports the outcome in a workspace toast.

**Tech Stack:** Rust, GPUI (`cx.prompt_for_new_path`, `workspace::Toast`), `serde_json`, `chrono`, and `tempfile` for tests.

**Spec:** `docs/superpowers/specs/2026-11-25-export-query-results-design.md`

## Global Constraints

- **Formats:** the extension picks the format. `.md` and `.markdown` give Markdown, `.json` gives JSON, `.csv` gives CSV and `.tsv` gives TSV, matched case-insensitively. Any other extension, or none, gives Markdown.
- **Suggested file name:** `<database>-<yyyymmdd-hhmmss>.md`. The dialog opens in the first visible worktree root, or in the home directory if there is none.
- **Which result sets:**
  - Markdown and JSON write every result set, plus the messages.
  - CSV and TSV write the open result tab, or Result 1 when the Messages tab is open.
- **Rows:** in query order. Unsaved grid edits, grid sorting and grid filters are ignored.
- **NULL versus empty string:**

  | Format | NULL | Empty string |
  | --- | --- | --- |
  | Markdown | `NULL` | empty cell |
  | JSON | `null` | `""` |
  | CSV / TSV | empty unquoted field | `""` |

- **Paused run:** fetch every remaining row (`Fetch::All`), then write when the run finishes. If it stops or fails first, write nothing and show why in a toast.
- **Stopped or failed run:** write what's there. The Markdown and JSON status says the rows may be incomplete.
- **Writing:** the file is written on a background thread. The toast reads `Exported N rows to <path>`, or `Export failed: <error>`.
- **Project rules (`.rules`):**
  - No `unwrap()` outside tests, and no panicking indexing.
  - No `let _ =` on fallible calls.
  - No comments that only summarize the code.
  - Use full-word names.
  - Run `./script/clippy`, not `cargo clippy`.

## Review Focus

1. **SQL that contains ```` ``` ````**, for example in a string or comment, must not break the Markdown code block. The fence must be longer than the longest backtick run in the SQL. This is tested in Task 1.
2. **Database names Windows won't accept in a file name** (`<>:"/\|?*` or control characters), or an empty default database, must still give a valid suggested name. Bad characters become `_`, and an empty name becomes `results`. This is tested in Task 1.
3. **Values containing the delimiter, `"`, CR, LF or a tab** must round-trip through CSV and TSV with RFC 4180 quoting. In Markdown, `|` is escaped and line breaks become `<br>`. This is tested in Task 1.
4. **Non-ASCII data opened in Excel.** CSV and TSV start with a UTF-8 BOM (`\u{feff}`) so Excel doesn't read them in the ANSI code page. *(Builder's call, since the spec says nothing about it.)* This is tested in Task 1.
5. **Starting a new run while a paused export waits for rows** must drop that export. No file from the old run may appear later. This is tested in Task 2.

---

### Task 1: Export formatting

**Files:**
- Create: `crates/database_ui/src/export.rs`
- Modify: `crates/database_ui/src/database_ui.rs`: add `mod export;` to the module list.
- Modify: `crates/database_ui/src/results_view.rs`: make `row_count_text` and `format_duration` `pub(crate)` so `export.rs` can reuse them.
- Modify: `crates/database_ui/Cargo.toml`: add `chrono = { workspace = true, features = ["clock"] }` and `serde_json.workspace = true` to `[dependencies]`.

**Interfaces:**
- Produces:
  - `pub(crate) enum ExportFormat { Markdown, Json, Csv, Tsv }` with `pub(crate) fn from_path(path: &Path) -> Self` and `pub(crate) fn holds_one_table(self) -> bool`. The latter is true for CSV and TSV.
  - `pub(crate) enum RunStatus { Completed, Stopped(String), Failed(String) }`.
  - `pub(crate) struct ExportData` with these fields:
    - `connection: String`
    - `environment: String`
    - `database: String`
    - `started_at: Option<chrono::DateTime<chrono::Local>>`
    - `elapsed: Option<Duration>`
    - `status: RunStatus`
    - `sql: String`
    - `result_sets: Vec<database::ResultSet>`
    - `messages: Vec<String>`
  - `pub(crate) fn render(format: ExportFormat, data: &ExportData, table: usize) -> String`. `table` picks the result set for CSV and TSV. If it's out of range, the output is empty except for the BOM.
  - `pub(crate) fn exported_row_count(format: ExportFormat, data: &ExportData, table: usize) -> usize`.
  - `pub(crate) fn suggested_file_name(database: &str, at: chrono::DateTime<chrono::Local>) -> String`.

- [ ] **Step 1: Write the failing tests** in a `#[cfg(test)] mod tests` at the bottom of `export.rs`.

The shared fixture `sample()` returns this `ExportData`:
- connection `QA`, environment `qa`, database `Shop`
- `started_at`: 2026-11-25 14:03:12 local time (`Local.with_ymd_and_hms`)
- elapsed 1250 ms, `RunStatus::Completed`
- `sql`: `"SELECT Id, Name FROM Customers;\nSELECT 1 AS One"`
- Result set 1 has columns `Id int` and `Name nvarchar`, with rows:
  - `["1", "Ada"]`
  - `["2", NULL]`
  - `["3", ""]`
  - `["4", "a|b\r\nc \"q\"\tt"]`
- Result set 2 has the column `One int` and the row `["1"]`.
- `messages`: `["Result 1: 4 rows", "Result 2: 1 row", "Finished in 1.25 s"]`.

```rust
#[test]
fn test_format_from_path() {
    for (path, format) in [
        ("a.md", ExportFormat::Markdown), ("a.MARKDOWN", ExportFormat::Markdown),
        ("a.json", ExportFormat::Json), ("a.CSV", ExportFormat::Csv),
        ("a.tsv", ExportFormat::Tsv), ("a.txt", ExportFormat::Markdown), ("a", ExportFormat::Markdown),
    ] {
        assert_eq!(ExportFormat::from_path(Path::new(path)), format, "{path}");
    }
}

#[test]
fn test_suggested_file_name() {
    let at = Local.with_ymd_and_hms(2026, 11, 25, 14, 3, 12).unwrap();
    assert_eq!(suggested_file_name("Shop", at), "Shop-20261125-140312.md");
    assert_eq!(suggested_file_name("", at), "results-20261125-140312.md");
    assert_eq!(suggested_file_name("my:db/x*", at), "my_db_x_-20261125-140312.md");
}

#[test]
fn test_markdown() {
    let text = render(ExportFormat::Markdown, &sample(), 0);
    for expected in [
        "# Query results\n",
        "- Connection: QA\n- Environment: qa\n- Database: Shop\n- Started: 2026-11-25 14:03:12\n- Elapsed: 1.25 s\n- Status: Completed\n",
        "```sql\nSELECT Id, Name FROM Customers;\nSELECT 1 AS One\n```\n",
        "## Result 1 (4 rows)\n\n| Id | Name |\n| --- | --- |\n| 1 | Ada |\n| 2 | NULL |\n| 3 |  |\n| 4 | a\\|b<br>c \"q\"\tt |\n",
        "## Result 2 (1 row)\n\n| One |\n| --- |\n| 1 |\n",
        "## Messages\n\n- Result 1: 4 rows\n- Result 2: 1 row\n- Finished in 1.25 s\n",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in\n{text}");
    }
}

#[test]
fn test_markdown_partial_status_and_fence() {
    let mut data = sample();
    data.status = RunStatus::Stopped("Stopped".into());
    data.sql = "SELECT '```' AS x".into();
    let text = render(ExportFormat::Markdown, &data, 0);
    assert!(text.contains("- Status: Stopped (Stopped). Rows may be incomplete.\n"), "{text}");
    assert!(text.contains("````sql\nSELECT '```' AS x\n````\n"), "{text}");
    data.status = RunStatus::Failed("Invalid object name 'x'.".into());
    let text = render(ExportFormat::Markdown, &data, 0);
    assert!(text.contains("- Status: Failed (Invalid object name 'x'.). Rows may be incomplete.\n"), "{text}");
}

#[test]
fn test_json() {
    let value: serde_json::Value = serde_json::from_str(&render(ExportFormat::Json, &sample(), 0)).unwrap();
    assert_eq!(value["connection"], "QA");
    assert_eq!(value["environment"], "qa");
    assert_eq!(value["database"], "Shop");
    assert!(value["started_at"].as_str().unwrap().starts_with("2026-11-25T14:03:12"));
    assert_eq!(value["elapsed_ms"], 1250);
    assert_eq!(value["status"], "completed");
    assert_eq!(value["status_detail"], serde_json::Value::Null);
    assert_eq!(value["sql"], "SELECT Id, Name FROM Customers;\nSELECT 1 AS One");
    assert_eq!(value["result_sets"].as_array().unwrap().len(), 2);
    assert_eq!(value["result_sets"][0]["columns"][1], json!({ "name": "Name", "type": "nvarchar" }));
    assert_eq!(value["result_sets"][0]["rows"][1], json!(["2", null]));
    assert_eq!(value["result_sets"][0]["rows"][2], json!(["3", ""]));
    assert_eq!(value["messages"][2], "Finished in 1.25 s");
}

#[test]
fn test_csv_and_tsv() {
    let data = sample();
    assert_eq!(
        render(ExportFormat::Csv, &data, 0),
        "\u{feff}Id,Name\r\n1,Ada\r\n2,\r\n3,\"\"\r\n4,\"a|b\r\nc \"\"q\"\"\tt\"\r\n"
    );
    assert_eq!(
        render(ExportFormat::Tsv, &data, 0),
        "\u{feff}Id\tName\r\n1\tAda\r\n2\t\r\n3\t\"\"\r\n4\t\"a|b\r\nc \"\"q\"\"\tt\"\r\n"
    );
    assert_eq!(render(ExportFormat::Csv, &data, 1), "\u{feff}One\r\n1\r\n");
    assert_eq!(exported_row_count(ExportFormat::Csv, &data, 1), 1);
    assert_eq!(exported_row_count(ExportFormat::Markdown, &data, 1), 5);
}
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test -p database_ui export::tests`
Expected: compile errors because `ExportFormat`, `render` and the other names don't exist yet.

- [ ] **Step 3: Implement `export.rs` with the interfaces above**

How each part works:
- **Markdown fence:** three backticks, or one more than the longest backtick run in the SQL.
- **Markdown cells:** replace `\r\n`, `\n` and `\r` with `<br>`, and `|` with `\|`.
- **CSV and TSV:**
  - Quote a field when it's an empty string or contains the delimiter, `"`, `\r` or `\n`. Double any `"` inside it.
  - End every line with `\r\n`.
  - Start the text with a BOM.
- **JSON:** build it with `serde_json::json!` and write it with `to_string_pretty`.
  - `status` is `"completed"`, `"stopped"` or `"failed"`.
  - `status_detail` holds the reason or error, or `null`.
  - `started_at` is `to_rfc3339()`, and `elapsed_ms` is an integer.
- **Markdown `Started` line:** format it as `%Y-%m-%d %H:%M:%S`.
- **Elapsed:** reuse `format_duration`.
- **Row counts in headings:** reuse `row_count_text`.

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test -p database_ui export::tests`
Expected: all 6 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/database_ui docs/superpowers
git commit -m "database_ui: Format query results as Markdown, JSON, CSV and TSV"
```

### Task 2: Export from the results pane

**Files:**
- Modify: `crates/database_ui/src/results_view.rs`
- Modify: `crates/database_ui/Cargo.toml`: add `tempfile.workspace = true` to `[dev-dependencies]`.
- Modify: `crates/database_ui/src/database_ui_tests.rs`: add `test_export_results`.
- Modify: `FORK_ROADMAP.md`:
  - Line 33: drop "no export" from the Databases gaps.
  - Section 2.4: mark result-set export as done. Range-selection copy is still open.

**Interfaces:**
- Consumes: from Task 1, `ExportFormat::from_path`, `ExportFormat::holds_one_table`, `ExportData`, `RunStatus`, `render`, `exported_row_count` and `suggested_file_name`.
- Produces:
  - The action `database::ExportResults`, documented as `/// Saves the results to a Markdown, JSON, CSV or TSV file, picked by its extension.`
  - Two test helpers on `QueryResultsView`: `pub(crate) fn export_to_for_test(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>)` and `pub(crate) fn show_result_tab_for_test(&mut self, index: usize, cx: &mut Context<Self>)`.

- [ ] **Step 1: Write the failing UI test** `test_export_results` in `database_ui_tests.rs`. Model it on `test_paging_stop_null_and_completion`.

Setup:
- Use `profile("ui-test-export", "dev")` with `fetch_limit = Some(2)`.
- Create `dbo.ExportCustomers (Id int PRIMARY KEY, Name nvarchar(50) NULL)` with the rows `(1,'Ada'), (2,NULL), (3,''), (4,'Grace'), (5,'Linus')`.
- The console text is `"SELECT Id, Name FROM dbo.ExportCustomers ORDER BY Id;\nSELECT COUNT(*) AS Total FROM dbo.ExportCustomers"`, run with `RunScript`.
- Use `let directory = tempfile::tempdir().unwrap();`.

Assertions:
1. **Paused run:** wait until `is_paused_for_test()`, then call `export_to_for_test(directory.path().join("out.md"))`. Wait until the run is not busy and the file exists.
   - The file contains `"## Result 1 (5 rows)"`, `"| 2 | NULL |\n"`, `"| 3 |  |\n"`, `"## Result 2 (1 row)"` and `"| Total |\n| --- |\n| 5 |\n"`.
2. **CSV of the open tab:** call `show_result_tab_for_test(1)`, then `export_to_for_test(directory.path().join("out.csv"))`. Wait until the file exists.
   - Its contents equal `"\u{feff}Total\r\n5\r\n"`.
3. **New run during a paused export (Review Focus 5):**
   - Run the script again and wait until paused.
   - Call `export_to_for_test(directory.path().join("dropped.json"))`.
   - Dispatch `RunScript` again and wait until paused.
   - Call `results.update(cx, |r, cx| r.stop_if_running(cx))` and wait until not busy.
   - Assert that `dropped.json` does not exist.

- [ ] **Step 2: Run the test and confirm it fails**

Run: `ZED_DATABASE_TEST_SQL_SERVER='(localdb)\MSSQLLocalDB' cargo test -p database_ui test_export_results`
Expected: compile errors because `export_to_for_test` and `show_result_tab_for_test` don't exist yet.

- [ ] **Step 3: Implement export in `QueryResultsView`**

**New fields:**
- `started_at: Option<chrono::DateTime<chrono::Local>>`, set to `Local::now()` in `run()`.
- `pending_export: Option<PendingExport>`, cleared in `run()`. `struct PendingExport { path: PathBuf, table: usize }`.
- `export_task: Option<Task<()>>`.

**`fn can_export(&self) -> bool`:** there's at least one result set, the state is `Finished`, `Paused`, `Stopped(_)` or `Failed`, and `pending_export` is `None`.

**`fn export(&mut self, window: &mut Window, cx: &mut Context<Self>)`:**
- Does nothing unless `can_export()`.
- The directory is the first visible worktree's `abs_path`, or `paths::home_dir()` if there is none.
- It calls `cx.prompt_for_new_path(&directory, Some(&suggested_file_name(&self.target.database, started_at or now)))`.
- It runs in `cx.spawn_in`. On `Ok(Ok(Some(path)))` it calls `export_to(path, window, cx)`. A cancelled dialog does nothing. A dialog error gets the `Export failed` toast.

**`fn export_to(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>)`:**
- The table is the open `Tab::Result(i)`, or 0 for Messages.
- `Paused`: store a `PendingExport`, then call `self.fetch(Fetch::All, window, cx)`.
- `Running`: store a `PendingExport`.
- `Finished`, `Stopped` or `Failed`: call `write_export(path, table, cx)`.
- `Idle` or `Stopping`: do nothing.

**`finish_run`:** after the state is set, `take()` any pending export.
- If the run finished, call `write_export`.
- Otherwise show the toast `Export cancelled: the run stopped before every row arrived.`, or `…failed before…` for a failed run.

**`fn export_data(&self, cx: &App) -> ExportData`:**
- The connection name and environment come from `profile(cx)`, or empty strings.
- The database is `self.target.database`.
- `RunStatus` maps from `self.state` and `self.error`.
- The messages are `self.messages`.
- The result sets are clones of `set.result`.

**`fn write_export(&mut self, path: PathBuf, table: usize, cx: &mut Context<Self>)`:**
- `export_task = cx.spawn(...)`.
- In the background, it runs `render` and `std::fs::write`, returning `anyhow::Result<usize>` (the row count from `exported_row_count`).
- In the foreground, it calls `show_export_toast`.

**`fn show_export_toast(&self, message: String, cx: &mut App)`:**
- It goes through `self.workspace.update(...)` with `workspace::Toast::new(NotificationId::unique::<ExportToast>(), message)`.
- Success toasts are `.autohide()`.
- The workspace update failing is handled with `.log_err()`.

**Header:**
- Add `IconButton::new("database-export", IconName::Download)` at the end of the tabs row, with `.icon_size(IconSize::Small)` and `.disabled(!self.can_export())`.
- The tooltip uses `Tooltip::for_action_title(..., &ExportResults)`. It's `"Fetch Every Row and Export…"` when paused and `"Export Results…"` otherwise.
- It calls `export` on click.

**Render:**
- Add `.on_action(cx.listener(|this, _: &ExportResults, window, cx| this.export(window, cx)))`.
- Add `ExportResults` to the `gpui::actions!` list.

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `ZED_DATABASE_TEST_SQL_SERVER='(localdb)\MSSQLLocalDB' cargo test -p database_ui`
Expected: `test_export_results`, the existing UI tests and Task 1's tests all pass.

Run: `./script/clippy -p database_ui`
Expected: no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/database_ui FORK_ROADMAP.md
git commit -m "database_ui: Export query results to a file"
```

### Task 3: Check it in the running app

- [ ] **Step 1:** Build and run Zed, using `.claude/skills` or the `run` skill. Open a console on the QA connection and run a two-statement script.
- [ ] **Step 2:** Click **Export**.
  - The dialog opens in the project folder with `<database>-<timestamp>.md` filled in.
  - Saving shows the `Exported N rows to <path>` toast.
  - The file matches the grid.
- [ ] **Step 3:** Run a `SELECT` with more than 500 rows. While it's paused, export to `.csv`.
  - The file has every row.
  - Excel shows non-ASCII text correctly.

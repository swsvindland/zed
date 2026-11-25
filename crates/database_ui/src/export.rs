//! Turns a console run's results into the text of an exported file.

use std::{path::Path, time::Duration};

use chrono::{DateTime, Local};
use database::ResultSet;
use serde_json::json;

use crate::results_view::{format_duration, row_count_text};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExportFormat {
    Markdown,
    Json,
    Csv,
    Tsv,
}

impl ExportFormat {
    /// Picks the format from the file extension. Anything unrecognized is written as Markdown.
    pub(crate) fn from_path(path: &Path) -> Self {
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase);
        match extension.as_deref() {
            Some("json") => Self::Json,
            Some("csv") => Self::Csv,
            Some("tsv") => Self::Tsv,
            _ => Self::Markdown,
        }
    }

    /// CSV and TSV hold a single table, so they export one result set.
    pub(crate) fn holds_one_table(self) -> bool {
        matches!(self, Self::Csv | Self::Tsv)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RunStatus {
    Completed,
    Stopped(String),
    Failed(String),
}

/// A snapshot of a run, taken so the file can be written off the main thread.
pub(crate) struct ExportData {
    pub connection: String,
    pub environment: String,
    pub database: String,
    pub started_at: Option<DateTime<Local>>,
    pub elapsed: Option<Duration>,
    pub status: RunStatus,
    pub sql: String,
    pub result_sets: Vec<ResultSet>,
    pub messages: Vec<String>,
}

/// `table` picks the result set for formats that hold one table.
pub(crate) fn render(format: ExportFormat, data: &ExportData, table: usize) -> String {
    match format {
        ExportFormat::Markdown => markdown(data),
        ExportFormat::Json => json_text(data),
        ExportFormat::Csv => delimited(data.result_sets.get(table), ','),
        ExportFormat::Tsv => delimited(data.result_sets.get(table), '\t'),
    }
}

pub(crate) fn exported_row_count(format: ExportFormat, data: &ExportData, table: usize) -> usize {
    if format.holds_one_table() {
        data.result_sets.get(table).map_or(0, |set| set.rows.len())
    } else {
        data.result_sets.iter().map(|set| set.rows.len()).sum()
    }
}

/// The notice shown once the file is written, and whether it may hide itself. A CSV can't say
/// that its rows are incomplete, so the notice does, and stays until dismissed.
pub(crate) fn exported_message(rows: usize, path: &Path, status: &RunStatus) -> (String, bool) {
    let exported = format!("Exported {} to {}", row_count_text(rows), path.display());
    match status {
        RunStatus::Completed => (exported, true),
        RunStatus::Stopped(_) => (
            format!("{exported}. The run was stopped, so rows may be incomplete."),
            false,
        ),
        RunStatus::Failed(_) => (
            format!("{exported}. The run failed, so rows may be incomplete."),
            false,
        ),
    }
}

pub(crate) fn suggested_file_name(database: &str, at: DateTime<Local>) -> String {
    let name = database
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
            {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    let name = if name.trim().is_empty() {
        "results"
    } else {
        name.as_str()
    };
    format!("{name}-{}.md", at.format("%Y%m%d-%H%M%S"))
}

fn markdown(data: &ExportData) -> String {
    let mut text = String::from("# Query results\n\n");
    text.push_str(&format!("- Connection: {}\n", data.connection));
    text.push_str(&format!("- Environment: {}\n", data.environment));
    text.push_str(&format!("- Database: {}\n", data.database));
    if let Some(started_at) = data.started_at {
        text.push_str(&format!(
            "- Started: {}\n",
            started_at.format("%Y-%m-%d %H:%M:%S")
        ));
    }
    if let Some(elapsed) = data.elapsed {
        text.push_str(&format!("- Elapsed: {}\n", format_duration(elapsed)));
    }
    let status = match &data.status {
        RunStatus::Completed => "Completed".to_string(),
        RunStatus::Stopped(reason) => format!("Stopped ({reason}). Rows may be incomplete."),
        RunStatus::Failed(error) => format!("Failed ({error}). Rows may be incomplete."),
    };
    text.push_str(&format!("- Status: {status}\n"));

    // A fence must be longer than any run of backticks inside it, or the SQL would end it.
    let longest_backticks = data
        .sql
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest_backticks.max(2) + 1);
    text.push_str(&format!("\n{fence}sql\n{}\n{fence}\n", data.sql));

    for (index, set) in data.result_sets.iter().enumerate() {
        text.push_str(&format!(
            "\n## Result {} ({})\n\n",
            index + 1,
            row_count_text(set.rows.len())
        ));
        if set.columns.is_empty() {
            continue;
        }
        push_markdown_row(
            &mut text,
            set.columns.iter().map(|column| column.name.as_str()),
        );
        push_markdown_row(&mut text, set.columns.iter().map(|_| "---"));
        for row in &set.rows {
            let cells = row
                .iter()
                .map(|value| match value {
                    Some(value) => markdown_cell(value),
                    None => "NULL".to_string(),
                })
                .collect::<Vec<_>>();
            push_markdown_row(&mut text, cells.iter().map(String::as_str));
        }
    }

    if !data.messages.is_empty() {
        text.push_str("\n## Messages\n\n");
        for message in &data.messages {
            text.push_str(&format!("- {message}\n"));
        }
    }
    text
}

fn push_markdown_row<'a>(text: &mut String, cells: impl Iterator<Item = &'a str>) {
    text.push('|');
    for cell in cells {
        text.push(' ');
        text.push_str(cell);
        text.push_str(" |");
    }
    text.push('\n');
}

fn markdown_cell(value: &str) -> String {
    value
        .replace("\r\n", "<br>")
        .replace(['\r', '\n'], "<br>")
        .replace('|', "\\|")
}

fn json_text(data: &ExportData) -> String {
    let (status, status_detail) = match &data.status {
        RunStatus::Completed => ("completed", None),
        RunStatus::Stopped(reason) => ("stopped", Some(reason)),
        RunStatus::Failed(error) => ("failed", Some(error)),
    };
    let result_sets = data
        .result_sets
        .iter()
        .map(|set| {
            let columns = set
                .columns
                .iter()
                .map(|column| json!({ "name": column.name, "type": column.type_name }))
                .collect::<Vec<_>>();
            json!({ "columns": columns, "rows": set.rows })
        })
        .collect::<Vec<_>>();
    let value = json!({
        "connection": data.connection,
        "environment": data.environment,
        "database": data.database,
        "started_at": data.started_at.map(|started_at| started_at.to_rfc3339()),
        "elapsed_ms": data
            .elapsed
            .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)),
        "status": status,
        "status_detail": status_detail,
        "sql": data.sql,
        "result_sets": result_sets,
        "messages": data.messages,
    });
    // `{:#}` pretty-prints a `Value`, which unlike `to_string_pretty` can't fail.
    format!("{value:#}\n")
}

/// RFC 4180 text with a byte order mark, which Excel needs to read the file as UTF-8. NULL is an
/// empty field and the empty string is `""`, so the two stay distinct.
fn delimited(set: Option<&ResultSet>, delimiter: char) -> String {
    let mut text = String::from("\u{feff}");
    let Some(set) = set else {
        return text;
    };
    push_record(
        &mut text,
        set.columns.iter().map(|column| Some(column.name.as_str())),
        delimiter,
    );
    for row in &set.rows {
        push_record(&mut text, row.iter().map(Option::as_deref), delimiter);
    }
    text
}

fn push_record<'a>(
    text: &mut String,
    values: impl Iterator<Item = Option<&'a str>>,
    delimiter: char,
) {
    for (index, value) in values.enumerate() {
        if index > 0 {
            text.push(delimiter);
        }
        match value {
            None => {}
            Some(value) if value.is_empty() || value.contains([delimiter, '"', '\r', '\n']) => {
                text.push('"');
                text.push_str(&value.replace('"', "\"\""));
                text.push('"');
            }
            Some(value) => text.push_str(value),
        }
    }
    text.push_str("\r\n");
}

#[cfg(test)]
mod tests {
    use std::{path::Path, time::Duration};

    use chrono::{Local, TimeZone as _};
    use database::{ColumnInfo, ResultSet};
    use serde_json::json;

    use super::*;

    fn column(name: &str, type_name: &str) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            type_name: type_name.into(),
        }
    }

    fn sample() -> ExportData {
        ExportData {
            connection: "QA".into(),
            environment: "qa".into(),
            database: "Shop".into(),
            started_at: Some(Local.with_ymd_and_hms(2026, 11, 25, 14, 3, 12).unwrap()),
            elapsed: Some(Duration::from_millis(1250)),
            status: RunStatus::Completed,
            sql: "SELECT Id, Name FROM Customers;\nSELECT 1 AS One".into(),
            result_sets: vec![
                ResultSet {
                    columns: vec![column("Id", "int"), column("Name", "nvarchar")],
                    rows: vec![
                        vec![Some("1".into()), Some("Ada".into())],
                        vec![Some("2".into()), None],
                        vec![Some("3".into()), Some(String::new())],
                        vec![Some("4".into()), Some("a|b\r\nc \"q\"\tt".into())],
                    ],
                },
                ResultSet {
                    columns: vec![column("One", "int")],
                    rows: vec![vec![Some("1".into())]],
                },
            ],
            messages: vec![
                "Result 1: 4 rows".into(),
                "Result 2: 1 row".into(),
                "Finished in 1.25 s".into(),
            ],
        }
    }

    #[test]
    fn test_format_from_path() {
        for (path, format) in [
            ("a.md", ExportFormat::Markdown),
            ("a.MARKDOWN", ExportFormat::Markdown),
            ("a.json", ExportFormat::Json),
            ("a.CSV", ExportFormat::Csv),
            ("a.tsv", ExportFormat::Tsv),
            ("a.txt", ExportFormat::Markdown),
            ("a", ExportFormat::Markdown),
        ] {
            assert_eq!(ExportFormat::from_path(Path::new(path)), format, "{path}");
        }
        assert!(ExportFormat::Csv.holds_one_table());
        assert!(ExportFormat::Tsv.holds_one_table());
        assert!(!ExportFormat::Markdown.holds_one_table());
        assert!(!ExportFormat::Json.holds_one_table());
    }

    #[test]
    fn test_suggested_file_name() {
        let at = Local.with_ymd_and_hms(2026, 11, 25, 14, 3, 12).unwrap();
        assert_eq!(suggested_file_name("Shop", at), "Shop-20261125-140312.md");
        assert_eq!(suggested_file_name("", at), "results-20261125-140312.md");
        assert_eq!(
            suggested_file_name("my:db/x*", at),
            "my_db_x_-20261125-140312.md"
        );
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
        assert!(
            text.contains("- Status: Stopped (Stopped). Rows may be incomplete.\n"),
            "{text}"
        );
        assert!(
            text.contains("````sql\nSELECT '```' AS x\n````\n"),
            "{text}"
        );
        data.status = RunStatus::Failed("Invalid object name 'x'.".into());
        let text = render(ExportFormat::Markdown, &data, 0);
        assert!(
            text.contains("- Status: Failed (Invalid object name 'x'.). Rows may be incomplete.\n"),
            "{text}"
        );
    }

    #[test]
    fn test_json() {
        let value: serde_json::Value =
            serde_json::from_str(&render(ExportFormat::Json, &sample(), 0)).unwrap();
        assert_eq!(value["connection"], "QA");
        assert_eq!(value["environment"], "qa");
        assert_eq!(value["database"], "Shop");
        assert!(
            value["started_at"]
                .as_str()
                .unwrap()
                .starts_with("2026-11-25T14:03:12")
        );
        assert_eq!(value["elapsed_ms"], 1250);
        assert_eq!(value["status"], "completed");
        assert_eq!(value["status_detail"], serde_json::Value::Null);
        assert_eq!(
            value["sql"],
            "SELECT Id, Name FROM Customers;\nSELECT 1 AS One"
        );
        assert_eq!(value["result_sets"].as_array().unwrap().len(), 2);
        assert_eq!(
            value["result_sets"][0]["columns"][1],
            json!({ "name": "Name", "type": "nvarchar" })
        );
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
        assert_eq!(render(ExportFormat::Csv, &data, 7), "\u{feff}");
        assert_eq!(exported_row_count(ExportFormat::Csv, &data, 1), 1);
        assert_eq!(exported_row_count(ExportFormat::Markdown, &data, 1), 5);
    }

    #[test]
    fn test_exported_message_flags_incomplete_runs() {
        let path = Path::new("out.csv");
        assert_eq!(
            exported_message(5, path, &RunStatus::Completed),
            ("Exported 5 rows to out.csv".to_string(), true)
        );
        assert_eq!(
            exported_message(500, path, &RunStatus::Stopped("Stopped".into())),
            (
                "Exported 500 rows to out.csv. The run was stopped, so rows may be incomplete."
                    .to_string(),
                false
            )
        );
        assert_eq!(
            exported_message(1, path, &RunStatus::Failed("Timeout".into())),
            (
                "Exported 1 row to out.csv. The run failed, so rows may be incomplete.".to_string(),
                false
            )
        );
    }
}

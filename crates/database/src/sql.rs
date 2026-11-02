//! Dialect-aware SQL text helpers: splitting a script into statements and generating the
//! parameterized DML that pushes grid edits back to a table.

use std::{fmt::Write as _, ops::Range};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    SqlServer,
    Postgres,
}

impl Dialect {
    pub fn quote_identifier(self, identifier: &str) -> String {
        match self {
            Dialect::SqlServer => format!("[{}]", identifier.replace(']', "]]")),
            Dialect::Postgres => format!("\"{}\"", identifier.replace('"', "\"\"")),
        }
    }

    pub fn qualified_name(self, table: &TableRef) -> String {
        match &table.schema {
            Some(schema) => format!(
                "{}.{}",
                self.quote_identifier(schema),
                self.quote_identifier(&table.name)
            ),
            None => self.quote_identifier(&table.name),
        }
    }

    pub fn select_top(self, table: &TableRef, limit: usize) -> String {
        let name = self.qualified_name(table);
        match self {
            Dialect::SqlServer => format!("SELECT TOP {limit} * FROM {name}"),
            Dialect::Postgres => format!("SELECT * FROM {name} LIMIT {limit}"),
        }
    }
}

/// Splits a script into statement ranges (byte offsets, trimmed of surrounding whitespace).
///
/// Statements end at a `;`, at a blank line, or (SQL Server) at a `GO` line. Delimiters inside
/// strings, quoted identifiers, comments, parentheses and `BEGIN … END` / `CASE … END` blocks
/// are ignored, so procedure bodies and multi-line subqueries stay in one piece. Blank lines are
/// treated as separators because T-SQL scripts frequently omit semicolons.
pub fn split_statements(text: &str, dialect: Dialect) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut paren_depth = 0usize;
    let mut block_depth = 0usize;
    let mut index = 0;

    let push = |ranges: &mut Vec<Range<usize>>, range: Range<usize>| {
        let trimmed = trim_range(text, range);
        if !trimmed.is_empty() && !is_only_comments(&text[trimmed.clone()]) {
            ranges.push(trimmed);
        }
    };

    while index < bytes.len() {
        let byte = bytes[index];
        match byte {
            b'\'' => index = skip_quoted(bytes, index, b'\''),
            b'"' => index = skip_quoted(bytes, index, b'"'),
            b'[' if dialect == Dialect::SqlServer => index = skip_quoted(bytes, index, b']'),
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                index = find_byte(bytes, index, b'\n').unwrap_or(bytes.len());
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = find_subslice(bytes, index + 2, b"*/").map_or(bytes.len(), |end| end + 2);
            }
            b'$' if dialect == Dialect::Postgres => {
                index = skip_dollar_quoted(bytes, index).unwrap_or(index + 1);
            }
            b'(' => {
                paren_depth += 1;
                index += 1;
            }
            b')' => {
                paren_depth = paren_depth.saturating_sub(1);
                index += 1;
            }
            b';' => {
                if paren_depth == 0 && block_depth == 0 {
                    push(&mut ranges, start..index);
                    start = index + 1;
                }
                index += 1;
            }
            b'\n' => {
                let next_line_start = index + 1;
                let next_line_end = find_byte(bytes, next_line_start, b'\n').unwrap_or(bytes.len());
                let next_line = text[next_line_start..next_line_end].trim();
                if dialect == Dialect::SqlServer && is_go_line(next_line) {
                    push(&mut ranges, start..index);
                    start = next_line_end;
                    index = next_line_end;
                    paren_depth = 0;
                    block_depth = 0;
                    continue;
                }
                if next_line.is_empty() && paren_depth == 0 && block_depth == 0 {
                    push(&mut ranges, start..index);
                    start = next_line_start;
                }
                index += 1;
            }
            byte if is_word_start(byte) => {
                let word_end = word_end(bytes, index);
                let word = &text[index..word_end];
                if word.eq_ignore_ascii_case("CASE") {
                    block_depth += 1;
                } else if word.eq_ignore_ascii_case("END") {
                    block_depth = block_depth.saturating_sub(1);
                } else if word.eq_ignore_ascii_case("BEGIN") && begins_block(text, word_end) {
                    block_depth += 1;
                }
                index = word_end;
            }
            _ => index += 1,
        }
    }

    // A script starting with `GO` on its first line has no preceding newline to trigger the check.
    push(&mut ranges, start..bytes.len());
    if dialect == Dialect::SqlServer {
        ranges.retain(|range| !is_go_line(text[range.clone()].trim()));
    }
    ranges
}

/// Returns the statement containing `offset`, or the closest statement before it when the
/// cursor sits in whitespace between statements.
pub fn statement_at(text: &str, offset: usize, dialect: Dialect) -> Option<Range<usize>> {
    let ranges = split_statements(text, dialect);
    ranges
        .iter()
        .rev()
        .find(|range| range.start <= offset)
        .or_else(|| ranges.first())
        .cloned()
}

/// Whether a statement may return rows. SQL Server's batch API only reports affected-row counts
/// for statements executed without reading results, so the driver uses this to pick a mode.
pub fn may_return_rows(text: &str) -> bool {
    const ROW_KEYWORDS: &[&str] = &[
        "SELECT",
        "EXEC",
        "EXECUTE",
        "OUTPUT",
        "RETURNING",
        "FETCH",
        "RESTORE",
        "DBCC",
    ];
    // Anything else first (such as a bare procedure name) is assumed to return rows.
    const NON_ROW_STARTS: &[&str] = &[
        "INSERT",
        "UPDATE",
        "DELETE",
        "MERGE",
        "CREATE",
        "ALTER",
        "DROP",
        "TRUNCATE",
        "GRANT",
        "REVOKE",
        "DENY",
        "SET",
        "USE",
        "DECLARE",
        "BEGIN",
        "COMMIT",
        "ROLLBACK",
        "SAVE",
        "PRINT",
        "RAISERROR",
        "THROW",
        "IF",
        "WHILE",
        "WITH",
    ];
    let mut first_word = true;
    let mut found = false;
    for_each_word(text, |word| {
        let is_keyword = |keyword: &&str| word.eq_ignore_ascii_case(keyword);
        if first_word && !NON_ROW_STARTS.iter().any(is_keyword) {
            found = true;
        }
        first_word = false;
        if ROW_KEYWORDS.iter().any(is_keyword) {
            found = true;
        }
    });
    found
}

fn begins_block(text: &str, after_begin: usize) -> bool {
    let rest = text[after_begin..].trim_start();
    if rest.is_empty() || rest.starts_with(';') {
        return false;
    }
    let next_word_end = rest
        .bytes()
        .position(|byte| !is_word_byte(byte))
        .unwrap_or(rest.len());
    let next_word = &rest[..next_word_end];
    ![
        "TRAN",
        "TRANSACTION",
        "DISTRIBUTED",
        "WORK",
        "ISOLATION",
        "DIALOG",
        "CONVERSATION",
    ]
    .iter()
    .any(|keyword| next_word.eq_ignore_ascii_case(keyword))
}

fn for_each_word(text: &str, mut f: impl FnMut(&str)) {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        match byte {
            b'\'' => index = skip_quoted(bytes, index, b'\''),
            b'"' => index = skip_quoted(bytes, index, b'"'),
            b'[' => index = skip_quoted(bytes, index, b']'),
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                index = find_byte(bytes, index, b'\n').unwrap_or(bytes.len());
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = find_subslice(bytes, index + 2, b"*/").map_or(bytes.len(), |end| end + 2);
            }
            byte if is_word_start(byte) => {
                let end = word_end(bytes, index);
                f(&text[index..end]);
                index = end;
            }
            _ => index += 1,
        }
    }
}

fn is_go_line(line: &str) -> bool {
    let mut parts = line.split_whitespace();
    let Some(first) = parts.next() else {
        return false;
    };
    first.eq_ignore_ascii_case("GO")
        && parts
            .next()
            .is_none_or(|count| count.parse::<u32>().is_ok())
        && parts.next().is_none()
}

fn is_word_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte == b'@' || byte == b'#'
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'@' | b'#' | b'$') || byte >= 0x80
}

fn word_end(bytes: &[u8], start: usize) -> usize {
    let mut end = start;
    while end < bytes.len() && is_word_byte(bytes[end]) {
        end += 1;
    }
    end
}

fn find_byte(bytes: &[u8], from: usize, needle: u8) -> Option<usize> {
    bytes
        .get(from..)?
        .iter()
        .position(|byte| *byte == needle)
        .map(|position| from + position)
}

fn find_subslice(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|position| from + position)
}

/// Skips a quoted region starting at `start` (the opening quote). A doubled closing quote is an
/// escaped quote. Returns the index just past the closing quote.
fn skip_quoted(bytes: &[u8], start: usize, close: u8) -> usize {
    let mut index = start + 1;
    while index < bytes.len() {
        if bytes[index] == close {
            if bytes.get(index + 1) == Some(&close) {
                index += 2;
                continue;
            }
            return index + 1;
        }
        index += 1;
    }
    bytes.len()
}

fn skip_dollar_quoted(bytes: &[u8], start: usize) -> Option<usize> {
    if start > 0 && is_word_byte(bytes[start - 1]) {
        return None;
    }
    let mut tag_end = start + 1;
    while tag_end < bytes.len()
        && (bytes[tag_end].is_ascii_alphanumeric() || bytes[tag_end] == b'_')
    {
        tag_end += 1;
    }
    if bytes.get(tag_end) != Some(&b'$') || bytes.get(start + 1).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    let tag = &bytes[start..=tag_end];
    let close = find_subslice(bytes, tag_end + 1, tag)?;
    Some(close + tag.len())
}

fn trim_range(text: &str, range: Range<usize>) -> Range<usize> {
    let slice = &text[range.clone()];
    let leading = slice.len() - slice.trim_start().len();
    let trailing = slice.len() - slice.trim_end().len();
    if leading == slice.len() {
        return range.start..range.start;
    }
    (range.start + leading)..(range.end - trailing)
}

fn is_only_comments(text: &str) -> bool {
    let mut has_word = false;
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                index = find_byte(bytes, index, b'\n').unwrap_or(bytes.len());
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = find_subslice(bytes, index + 2, b"*/").map_or(bytes.len(), |end| end + 2);
            }
            byte if byte.is_ascii_whitespace() => index += 1,
            _ => {
                has_word = true;
                break;
            }
        }
    }
    !has_word
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableRef {
    pub schema: Option<String>,
    pub name: String,
}

/// A column of the table being edited, as needed to write DML for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditColumn {
    pub name: String,
    /// Postgres needs an explicit cast from the text parameter to the column's type.
    pub type_name: Option<String>,
    pub type_schema: Option<String>,
}

/// One pending row change, with values as the text the user typed (`None` is SQL `NULL`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowChange {
    Update {
        key: Vec<(usize, Option<String>)>,
        values: Vec<(usize, Option<String>)>,
    },
    Insert {
        values: Vec<(usize, Option<String>)>,
    },
    Delete {
        key: Vec<(usize, Option<String>)>,
    },
}

/// SQL text with numbered placeholders and the values bound to them, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParameterizedStatement {
    pub sql: String,
    pub params: Vec<Option<String>>,
    /// Updates and deletes target one row by key; any other count means the row changed or
    /// vanished since it was read, so the push is rolled back.
    pub expect_one_row: bool,
}

pub fn generate_statements(
    dialect: Dialect,
    table: &TableRef,
    columns: &[Option<EditColumn>],
    changes: &[RowChange],
) -> anyhow::Result<Vec<ParameterizedStatement>> {
    let table_name = dialect.qualified_name(table);
    changes
        .iter()
        .map(|change| {
            let mut builder = StatementBuilder {
                dialect,
                columns,
                params: Vec::new(),
            };
            let (sql, expect_one_row) = match change {
                RowChange::Update { key, values } => {
                    anyhow::ensure!(!values.is_empty(), "update without changed values");
                    let assignments = values
                        .iter()
                        .map(|(column, value)| {
                            Ok(format!(
                                "{} = {}",
                                builder.column_name(*column)?,
                                builder.placeholder(*column, value.clone())?
                            ))
                        })
                        .collect::<anyhow::Result<Vec<_>>>()?
                        .join(", ");
                    let predicate = builder.key_predicate(key)?;
                    (
                        format!("UPDATE {table_name} SET {assignments} WHERE {predicate}"),
                        true,
                    )
                }
                RowChange::Insert { values } => {
                    if values.is_empty() {
                        (format!("INSERT INTO {table_name} DEFAULT VALUES"), false)
                    } else {
                        let names = values
                            .iter()
                            .map(|(column, _)| builder.column_name(*column))
                            .collect::<anyhow::Result<Vec<_>>>()?
                            .join(", ");
                        let placeholders = values
                            .iter()
                            .map(|(column, value)| builder.placeholder(*column, value.clone()))
                            .collect::<anyhow::Result<Vec<_>>>()?
                            .join(", ");
                        (
                            format!("INSERT INTO {table_name} ({names}) VALUES ({placeholders})"),
                            false,
                        )
                    }
                }
                RowChange::Delete { key } => {
                    let predicate = builder.key_predicate(key)?;
                    (format!("DELETE FROM {table_name} WHERE {predicate}"), true)
                }
            };
            Ok(ParameterizedStatement {
                sql,
                params: builder.params,
                expect_one_row,
            })
        })
        .collect()
}

/// Renders statements for review: the exact SQL that runs, each followed by its bound values.
pub fn preview(dialect: Dialect, statements: &[ParameterizedStatement]) -> String {
    let mut output = String::new();
    for statement in statements {
        output.push_str(&statement.sql);
        output.push_str(";\n");
        for (index, param) in statement.params.iter().enumerate() {
            let name = match dialect {
                Dialect::SqlServer => format!("@P{}", index + 1),
                Dialect::Postgres => format!("${}", index + 1),
            };
            let value = match param {
                Some(value) => format!("'{}'", value.replace('\'', "''")),
                None => "NULL".to_string(),
            };
            writeln!(output, "--   {name} = {value}").ok();
        }
    }
    output
}

struct StatementBuilder<'a> {
    dialect: Dialect,
    columns: &'a [Option<EditColumn>],
    params: Vec<Option<String>>,
}

impl StatementBuilder<'_> {
    fn column(&self, index: usize) -> anyhow::Result<&EditColumn> {
        self.columns
            .get(index)
            .and_then(Option::as_ref)
            .ok_or_else(|| {
                anyhow::anyhow!("result column {} isn't a column of the table", index + 1)
            })
    }

    fn column_name(&self, index: usize) -> anyhow::Result<String> {
        Ok(self.dialect.quote_identifier(&self.column(index)?.name))
    }

    fn placeholder(&mut self, column: usize, value: Option<String>) -> anyhow::Result<String> {
        let column = self.column(column)?.clone();
        self.params.push(value);
        let number = self.params.len();
        Ok(match self.dialect {
            Dialect::SqlServer => format!("@P{number}"),
            Dialect::Postgres => match &column.type_name {
                Some(type_name) => {
                    let type_name = match &column.type_schema {
                        Some(schema) if schema != "pg_catalog" => format!(
                            "{}.{}",
                            self.dialect.quote_identifier(schema),
                            self.dialect.quote_identifier(type_name)
                        ),
                        _ => self.dialect.quote_identifier(type_name),
                    };
                    format!("CAST(${number}::text AS {type_name})")
                }
                None => format!("${number}::text"),
            },
        })
    }

    fn key_predicate(&mut self, key: &[(usize, Option<String>)]) -> anyhow::Result<String> {
        anyhow::ensure!(!key.is_empty(), "row has no key values");
        Ok(key
            .iter()
            .map(|(column, value)| {
                let name = self.column_name(*column)?;
                Ok(match value {
                    Some(value) => {
                        format!(
                            "{name} = {}",
                            self.placeholder(*column, Some(value.clone()))?
                        )
                    }
                    None => format!("{name} IS NULL"),
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?
            .join(" AND "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn statements(text: &str, dialect: Dialect) -> Vec<&str> {
        split_statements(text, dialect)
            .into_iter()
            .map(|range| &text[range])
            .collect()
    }

    #[test]
    fn splits_on_semicolons_blank_lines_and_go() {
        let script = "SELECT 1;\nSELECT 'a;b' AS [x;y]\n\nSELECT 2\nGO\n-- note\nSELECT 3";
        assert_eq!(
            statements(script, Dialect::SqlServer),
            vec![
                "SELECT 1",
                "SELECT 'a;b' AS [x;y]",
                "SELECT 2",
                "-- note\nSELECT 3"
            ]
        );
    }

    #[test]
    fn keeps_blocks_and_parentheses_together() {
        let script = "CREATE PROCEDURE p AS\nBEGIN\n  SELECT 1;\n\n  SELECT CASE WHEN 1=1 THEN 2 END;\nEND\n\nSELECT (\n\n  1\n)";
        assert_eq!(
            statements(script, Dialect::SqlServer),
            vec![
                "CREATE PROCEDURE p AS\nBEGIN\n  SELECT 1;\n\n  SELECT CASE WHEN 1=1 THEN 2 END;\nEND",
                "SELECT (\n\n  1\n)"
            ]
        );
        assert_eq!(
            statements(
                "BEGIN TRAN;\nUPDATE t SET a = 1;\nCOMMIT",
                Dialect::SqlServer
            ),
            vec!["BEGIN TRAN", "UPDATE t SET a = 1", "COMMIT"]
        );
    }

    #[test]
    fn postgres_dollar_quotes() {
        let script = "CREATE FUNCTION f() RETURNS int AS $body$ SELECT 1; $body$ LANGUAGE sql;\nSELECT $1, 2";
        assert_eq!(
            statements(script, Dialect::Postgres),
            vec![
                "CREATE FUNCTION f() RETURNS int AS $body$ SELECT 1; $body$ LANGUAGE sql",
                "SELECT $1, 2"
            ]
        );
    }

    #[test]
    fn statement_under_cursor() {
        let script = "SELECT 1;\nSELECT 2;\n\n";
        let at = |offset| statement_at(script, offset, Dialect::SqlServer).map(|r| &script[r]);
        assert_eq!(at(0), Some("SELECT 1"));
        assert_eq!(at(9), Some("SELECT 1"));
        assert_eq!(at(12), Some("SELECT 2"));
        assert_eq!(at(script.len()), Some("SELECT 2"));
    }

    #[test]
    fn detects_row_returning_statements() {
        assert!(may_return_rows("select * from t"));
        assert!(may_return_rows("UPDATE t SET a = 1 OUTPUT inserted.a"));
        assert!(!may_return_rows("UPDATE t SET a = 'select' -- select"));
        assert!(!may_return_rows("INSERT INTO t (a) VALUES (1)"));
        assert!(may_return_rows("sp_who2"));
    }

    #[test]
    fn generates_parameterized_sql_server_dml() {
        let table = TableRef {
            schema: Some("dbo".into()),
            name: "Peo]ple".into(),
        };
        let columns = ["Id", "Name", "Age"]
            .into_iter()
            .map(|name| {
                Some(EditColumn {
                    name: name.into(),
                    type_name: None,
                    type_schema: None,
                })
            })
            .collect::<Vec<_>>();
        let statements = generate_statements(
            Dialect::SqlServer,
            &table,
            &columns,
            &[
                RowChange::Update {
                    key: vec![(0, Some("7".into()))],
                    values: vec![(1, Some("O'Brien; DROP TABLE x".into())), (2, None)],
                },
                RowChange::Insert {
                    values: vec![(1, Some("New".into()))],
                },
                RowChange::Delete {
                    key: vec![(0, Some("9".into()))],
                },
            ],
        )
        .unwrap();
        assert_eq!(
            statements[0].sql,
            "UPDATE [dbo].[Peo]]ple] SET [Name] = @P1, [Age] = @P2 WHERE [Id] = @P3"
        );
        assert_eq!(
            statements[0].params,
            vec![Some("O'Brien; DROP TABLE x".into()), None, Some("7".into())]
        );
        assert!(statements[0].expect_one_row);
        assert_eq!(
            statements[1].sql,
            "INSERT INTO [dbo].[Peo]]ple] ([Name]) VALUES (@P1)"
        );
        assert!(!statements[1].expect_one_row);
        assert_eq!(
            statements[2].sql,
            "DELETE FROM [dbo].[Peo]]ple] WHERE [Id] = @P1"
        );
        assert_eq!(statements[2].params, vec![Some("9".into())]);
    }

    #[test]
    fn generates_postgres_casts() {
        let table = TableRef {
            schema: Some("public".into()),
            name: "items".into(),
        };
        let columns = vec![
            Some(EditColumn {
                name: "id".into(),
                type_name: Some("int4".into()),
                type_schema: Some("pg_catalog".into()),
            }),
            Some(EditColumn {
                name: "mood".into(),
                type_name: Some("mood".into()),
                type_schema: Some("public".into()),
            }),
        ];
        let statements = generate_statements(
            Dialect::Postgres,
            &table,
            &columns,
            &[RowChange::Update {
                key: vec![(0, Some("1".into()))],
                values: vec![(1, Some("happy".into()))],
            }],
        )
        .unwrap();
        assert_eq!(
            statements[0].sql,
            "UPDATE \"public\".\"items\" SET \"mood\" = CAST($1::text AS \"public\".\"mood\") WHERE \"id\" = CAST($2::text AS \"int4\")"
        );
    }
}

//! Context-aware SQL completion over a [`Catalog`]. What surrounds the cursor decides what is
//! offered: tables after `FROM` or `JOIN`, a table's columns after `alias.`, join conditions
//! from foreign keys after `ON`, and the columns of every table in the statement elsewhere.

use std::ops::Range;

use crate::{
    ObjectKind,
    catalog::{Catalog, CatalogObject, ObjectName, same_object},
    sql::{self, Dialect},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidateKind {
    Column,
    Table,
    View,
    Schema,
    /// A table or alias already in the statement.
    Alias,
    /// A table to join, with an `ON` condition from a foreign key.
    Join,
    /// A join condition from a foreign key.
    JoinCondition,
    Keyword,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub label: String,
    pub insert: String,
    pub kind: CandidateKind,
    /// A type or where the item comes from, shown beside the label.
    pub detail: Option<String>,
    /// Lower is offered first.
    pub priority: u8,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Completions {
    /// The text the chosen candidate replaces: the partly typed name before the cursor.
    pub replace: Range<usize>,
    pub candidates: Vec<Candidate>,
}

const KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "AND",
    "OR",
    "NOT",
    "NULL",
    "IS NULL",
    "IS NOT NULL",
    "IN",
    "EXISTS",
    "BETWEEN",
    "LIKE",
    "AS",
    "ON",
    "JOIN",
    "INNER JOIN",
    "LEFT JOIN",
    "RIGHT JOIN",
    "FULL JOIN",
    "CROSS JOIN",
    "GROUP BY",
    "ORDER BY",
    "HAVING",
    "DISTINCT",
    "UNION",
    "UNION ALL",
    "EXCEPT",
    "INTERSECT",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "INSERT INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE FROM",
    "MERGE",
    "CREATE TABLE",
    "ALTER TABLE",
    "DROP TABLE",
    "TRUNCATE TABLE",
    "CREATE VIEW",
    "CREATE INDEX",
    "WITH",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "ASC",
    "DESC",
    "COUNT",
    "SUM",
    "AVG",
    "MIN",
    "MAX",
    "COALESCE",
    "CAST",
    "OVER",
    "PARTITION BY",
];
const SQL_SERVER_KEYWORDS: &[&str] = &[
    "TOP",
    "OFFSET",
    "FETCH NEXT",
    "ROWS ONLY",
    "OUTER APPLY",
    "CROSS APPLY",
    "NOLOCK",
    "DECLARE",
    "EXEC",
    "PRINT",
    "GO",
    "ISNULL",
    "GETDATE",
    "IIF",
    "OUTPUT",
];
const POSTGRES_KEYWORDS: &[&str] = &[
    "LIMIT",
    "OFFSET",
    "RETURNING",
    "ILIKE",
    "ON CONFLICT",
    "DO NOTHING",
    "NOW",
    "LATERAL",
];

/// Words that end a table reference instead of being its alias.
const RESERVED: &[&str] = &[
    "all",
    "and",
    "any",
    "apply",
    "as",
    "asc",
    "between",
    "by",
    "case",
    "cross",
    "default",
    "delete",
    "desc",
    "distinct",
    "else",
    "end",
    "except",
    "exists",
    "fetch",
    "for",
    "from",
    "full",
    "group",
    "having",
    "in",
    "inner",
    "insert",
    "intersect",
    "into",
    "is",
    "join",
    "left",
    "like",
    "limit",
    "merge",
    "natural",
    "not",
    "null",
    "offset",
    "on",
    "option",
    "or",
    "order",
    "outer",
    "output",
    "pivot",
    "returning",
    "right",
    "select",
    "set",
    "table",
    "then",
    "top",
    "union",
    "unpivot",
    "update",
    "using",
    "values",
    "when",
    "where",
    "window",
    "with",
];

/// Keywords after which a table name comes.
const TABLE_KEYWORDS: &[&str] = &["from", "join", "update", "into", "table", "apply"];

#[derive(Clone, Debug, PartialEq, Eq)]
enum TokenKind {
    Word { value: String, quoted: bool },
    Dot,
    Comma,
    OpenParen,
    CloseParen,
    Other,
}

#[derive(Clone, Debug)]
struct Token {
    kind: TokenKind,
    range: Range<usize>,
}

impl Token {
    fn word(&self) -> Option<&str> {
        match &self.kind {
            TokenKind::Word { value, .. } => Some(value),
            _ => None,
        }
    }

    /// A bare word, compared case-insensitively, such as a keyword.
    fn is_keyword(&self, keyword: &str) -> bool {
        matches!(&self.kind, TokenKind::Word { value, quoted: false } if value.eq_ignore_ascii_case(keyword))
    }

    fn is_reserved(&self) -> bool {
        matches!(&self.kind, TokenKind::Word { value, quoted: false } if is_reserved(value))
    }
}

fn is_reserved(word: &str) -> bool {
    RESERVED
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(word))
}

fn is_word_start(character: char) -> bool {
    character.is_alphabetic() || matches!(character, '_' | '@' | '#')
}

fn is_word_char(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '@' | '#' | '$')
}

/// A string or comment: the cursor in one gets no completions. `open` ones (line comments, or
/// anything unterminated) also contain the position at their end.
struct Inert {
    range: Range<usize>,
    open: bool,
}

impl Inert {
    fn contains(&self, offset: usize) -> bool {
        self.range.start < offset
            && (offset < self.range.end || (self.open && offset == self.range.end))
    }
}

fn tokenize(text: &str, dialect: Dialect) -> (Vec<Token>, Vec<Inert>) {
    let mut tokens = Vec::new();
    let mut inert = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((start, character)) = chars.next() {
        let next = chars.peek().map(|(_, next)| *next);
        let kind = match character {
            _ if character.is_whitespace() => continue,
            '-' if next == Some('-') => {
                while chars.next_if(|(_, character)| *character != '\n').is_some() {}
                let end = chars.peek().map_or(text.len(), |(index, _)| *index);
                inert.push(Inert {
                    range: start..end,
                    open: true,
                });
                continue;
            }
            '/' if next == Some('*') => {
                chars.next();
                let mut previous = ' ';
                let mut closed = false;
                while let Some((_, character)) = chars.next() {
                    if previous == '*' && character == '/' {
                        closed = true;
                        break;
                    }
                    previous = character;
                }
                let end = chars.peek().map_or(text.len(), |(index, _)| *index);
                inert.push(Inert {
                    range: start..end,
                    open: !closed,
                });
                continue;
            }
            '\'' => {
                let mut closed = false;
                while let Some((_, character)) = chars.next() {
                    if character == '\'' && chars.next_if(|(_, next)| *next == '\'').is_none() {
                        closed = true;
                        break;
                    }
                }
                let end = chars.peek().map_or(text.len(), |(index, _)| *index);
                inert.push(Inert {
                    range: start..end,
                    open: !closed,
                });
                TokenKind::Other
            }
            '"' | '[' | '`' => {
                let close = match character {
                    '[' if dialect == Dialect::SqlServer => ']',
                    '[' => {
                        tokens.push(Token {
                            kind: TokenKind::Other,
                            range: start..start + 1,
                        });
                        continue;
                    }
                    other => other,
                };
                let mut value = String::new();
                while let Some((_, character)) = chars.next() {
                    if character == close && chars.next_if(|(_, next)| *next == close).is_none() {
                        break;
                    }
                    value.push(character);
                }
                TokenKind::Word {
                    value,
                    quoted: true,
                }
            }
            '$' if dialect == Dialect::Postgres => {
                // A dollar-quoted string: `$tag$ … $tag$`.
                let mut tag = String::from("$");
                while let Some((_, character)) = chars.next_if(|(_, next)| is_word_char(*next)) {
                    tag.push(character);
                }
                if chars.next_if(|(_, next)| *next == '$').is_some() {
                    tag.push('$');
                    let body_start = chars.peek().map_or(text.len(), |(index, _)| *index);
                    let end = text
                        .get(body_start..)
                        .and_then(|rest| rest.find(&tag))
                        .map_or(text.len(), |found| body_start + found + tag.len());
                    while chars.next_if(|(index, _)| *index < end).is_some() {}
                }
                TokenKind::Other
            }
            '.' => TokenKind::Dot,
            ',' => TokenKind::Comma,
            '(' => TokenKind::OpenParen,
            ')' => TokenKind::CloseParen,
            _ if character.is_ascii_digit() => {
                while chars
                    .next_if(|(_, next)| next.is_ascii_alphanumeric() || *next == '.')
                    .is_some()
                {}
                TokenKind::Other
            }
            _ if is_word_start(character) => {
                let mut value = String::from(character);
                while let Some((_, character)) = chars.next_if(|(_, next)| is_word_char(*next)) {
                    value.push(character);
                }
                TokenKind::Word {
                    value,
                    quoted: false,
                }
            }
            _ => TokenKind::Other,
        };
        let end = chars.peek().map_or(text.len(), |(index, _)| *index);
        tokens.push(Token {
            kind,
            range: start..end,
        });
    }
    (tokens, inert)
}

/// A table, view or subquery that a statement reads or writes.
#[derive(Clone, Debug)]
struct TableReference {
    /// Name parts as written (`[dbo]`, `Orders`), for qualifying inserted columns.
    written: Vec<String>,
    /// Name parts without quotes. Empty for a subquery.
    parts: Vec<String>,
    alias: Option<String>,
    written_alias: Option<String>,
    /// Token index where the reference starts.
    start: usize,
    /// Token index just past the reference, alias included.
    end: usize,
    is_join: bool,
}

impl TableReference {
    /// What columns of this table are qualified with: its alias, or else its name.
    fn qualifier(&self) -> String {
        self.written_alias
            .clone()
            .or_else(|| self.written.last().cloned())
            .unwrap_or_default()
    }

    fn matches_qualifier(&self, qualifier: &str) -> bool {
        match &self.alias {
            Some(alias) => alias.eq_ignore_ascii_case(qualifier),
            None => self
                .parts
                .last()
                .is_some_and(|name| name.eq_ignore_ascii_case(qualifier)),
        }
    }

    fn resolve<'a>(&self, catalog: &'a Catalog) -> Option<&'a CatalogObject> {
        match self.parts.as_slice() {
            [name] => catalog.find(None, name),
            [.., schema, name] => catalog.find(Some(schema), name),
            [] => None,
        }
    }
}

fn written(text: &str, token: &Token) -> String {
    text.get(token.range.clone())
        .unwrap_or_default()
        .to_string()
}

fn table_references(text: &str, tokens: &[Token]) -> Vec<TableReference> {
    let mut references = Vec::new();
    // Paren depth of each open FROM list, so a comma at that depth starts another table.
    let mut from_lists = Vec::<usize>::new();
    let mut depth = 0usize;
    let mut index = 0;
    while let Some(token) = tokens.get(index) {
        let starts_reference = match &token.kind {
            TokenKind::OpenParen => {
                depth += 1;
                false
            }
            TokenKind::CloseParen => {
                from_lists.retain(|list_depth| *list_depth < depth);
                depth = depth.saturating_sub(1);
                false
            }
            TokenKind::Comma => from_lists.last() == Some(&depth),
            TokenKind::Word { quoted: false, .. }
                if TABLE_KEYWORDS
                    .iter()
                    .any(|keyword| token.is_keyword(keyword)) =>
            {
                from_lists.retain(|list_depth| *list_depth < depth);
                if token.is_keyword("from") {
                    from_lists.push(depth);
                }
                true
            }
            TokenKind::Word { quoted: false, .. } if token.is_reserved() => {
                if !token.is_keyword("as") {
                    from_lists.retain(|list_depth| *list_depth < depth);
                }
                false
            }
            _ => false,
        };
        index += 1;
        if !starts_reference {
            continue;
        }
        let is_join = token.is_keyword("join") || token.is_keyword("apply");
        let start = index;
        let mut written_parts = Vec::new();
        let mut parts = Vec::new();
        if matches!(
            tokens.get(index).map(|token| &token.kind),
            Some(TokenKind::OpenParen)
        ) {
            // A subquery or a function's arguments: skip to the matching parenthesis.
            let mut nested = 0;
            while let Some(token) = tokens.get(index) {
                index += 1;
                match token.kind {
                    TokenKind::OpenParen => nested += 1,
                    TokenKind::CloseParen => {
                        nested -= 1;
                        if nested == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        } else {
            while let Some(token) = tokens.get(index) {
                let Some(value) = token.word() else {
                    break;
                };
                if token.is_reserved() {
                    break;
                }
                written_parts.push(written(text, token));
                parts.push(value.to_string());
                index += 1;
                if !matches!(
                    tokens.get(index).map(|token| &token.kind),
                    Some(TokenKind::Dot)
                ) {
                    break;
                }
                index += 1;
            }
            if parts.is_empty() {
                continue;
            }
            // A table-valued function's arguments.
            if matches!(
                tokens.get(index).map(|token| &token.kind),
                Some(TokenKind::OpenParen)
            ) && !tokens
                .get(start.saturating_sub(1))
                .is_some_and(|token| token.is_keyword("into"))
            {
                let mut nested = 0;
                while let Some(token) = tokens.get(index) {
                    index += 1;
                    match token.kind {
                        TokenKind::OpenParen => nested += 1,
                        TokenKind::CloseParen => {
                            nested -= 1;
                            if nested == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                parts.clear();
            }
        }
        if tokens
            .get(index)
            .is_some_and(|token| token.is_keyword("as"))
        {
            index += 1;
        }
        let mut alias = None;
        let mut written_alias = None;
        if let Some(token) = tokens.get(index)
            && let Some(value) = token.word()
            && !token.is_reserved()
        {
            alias = Some(value.to_string());
            written_alias = Some(written(text, token));
            index += 1;
        }
        references.push(TableReference {
            written: written_parts,
            parts,
            alias,
            written_alias,
            start,
            end: index,
            is_join,
        });
    }
    references
}

/// Whether `name` can be written without quotes. Postgres folds unquoted names to lower case,
/// so a name with capitals needs quotes there.
fn is_plain_identifier(name: &str, dialect: Dialect) -> bool {
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
        && !(dialect == Dialect::Postgres
            && name.chars().any(|character| character.is_ascii_uppercase()))
        && !is_reserved(name)
}

struct Inserter {
    dialect: Dialect,
    /// The user opened a quoted name, which the replaced range includes.
    quoted: bool,
}

impl Inserter {
    fn name(&self, name: &str) -> String {
        if self.quoted || !is_plain_identifier(name, self.dialect) {
            self.dialect.quote_identifier(name)
        } else {
            name.to_string()
        }
    }

    fn plain(&self, name: &str) -> String {
        if is_plain_identifier(name, self.dialect) {
            name.to_string()
        } else {
            self.dialect.quote_identifier(name)
        }
    }

    /// An object's name, schema-qualified unless it's in the default schema.
    fn object(&self, catalog: &Catalog, object: &ObjectName) -> String {
        let in_default = catalog
            .default_schema
            .as_deref()
            .is_some_and(|schema| schema.eq_ignore_ascii_case(&object.schema));
        if in_default {
            self.name(&object.name)
        } else {
            format!("{}.{}", self.name(&object.schema), self.plain(&object.name))
        }
    }
}

fn column_detail(column: &crate::ColumnSummary) -> Option<String> {
    let mut detail = column.type_name.clone();
    if column.primary_key {
        detail.push_str(" · key");
    }
    (!detail.is_empty()).then_some(detail)
}

fn object_candidate(
    catalog: &Catalog,
    object: &CatalogObject,
    insert: &Inserter,
    priority: u8,
) -> Candidate {
    let in_default = catalog
        .default_schema
        .as_deref()
        .is_some_and(|schema| schema.eq_ignore_ascii_case(&object.name.schema));
    let label = if in_default {
        object.name.name.clone()
    } else {
        format!("{}.{}", object.name.schema, object.name.name)
    };
    let (kind, noun) = match object.kind {
        ObjectKind::Table => (CandidateKind::Table, "table"),
        ObjectKind::View => (CandidateKind::View, "view"),
    };
    Candidate {
        label,
        insert: insert.object(catalog, &object.name),
        kind,
        detail: Some(noun.to_string()),
        priority: priority + u8::from(object.kind == ObjectKind::View),
    }
}

/// A short alias from a table name's word initials: `OrderLines` → `ol`, `order_lines` → `ol`.
fn alias_for(name: &str, taken: &[String]) -> String {
    let mut initials = String::new();
    let mut previous: Option<char> = None;
    for character in name.chars() {
        let starts_word = match previous {
            None => true,
            Some(previous) => {
                (!previous.is_alphanumeric() && character.is_alphanumeric())
                    || (previous.is_lowercase() && character.is_uppercase())
            }
        };
        if starts_word && character.is_alphanumeric() {
            initials.extend(character.to_lowercase());
        }
        previous = Some(character);
    }
    if initials.is_empty()
        || !initials.starts_with(|character: char| character.is_ascii_alphabetic())
    {
        initials.insert(0, 't');
    }
    let is_free = |candidate: &str| {
        !is_reserved(candidate)
            && !taken
                .iter()
                .any(|taken| taken.eq_ignore_ascii_case(candidate))
    };
    if is_free(&initials) {
        return initials;
    }
    (2..)
        .map(|number| format!("{initials}{number}"))
        .find(|candidate| is_free(candidate))
        .unwrap_or(initials)
}

/// `ON` conditions joining `joined` to `other` along their foreign keys.
fn join_conditions(
    catalog: &Catalog,
    joined: (&ObjectName, &str),
    other: (&ObjectName, &str),
) -> Vec<String> {
    let (joined_name, joined_qualifier) = joined;
    let (other_name, other_qualifier) = other;
    let mut conditions = Vec::new();
    for key in catalog.foreign_keys_between(joined_name, other_name) {
        let directions = [
            (same_object(&key.table, joined_name)
                && same_object(&key.referenced_table, other_name))
            .then(|| (&key.columns, &key.referenced_columns)),
            (same_object(&key.table, other_name)
                && same_object(&key.referenced_table, joined_name))
            .then(|| (&key.referenced_columns, &key.columns)),
        ];
        for (joined_columns, other_columns) in directions.into_iter().flatten() {
            let condition = joined_columns
                .iter()
                .zip(other_columns)
                .map(|(joined_column, other_column)| {
                    format!("{joined_qualifier}.{joined_column} = {other_qualifier}.{other_column}")
                })
                .collect::<Vec<_>>()
                .join(" AND ");
            if !conditions.contains(&condition) {
                conditions.push(condition);
            }
        }
    }
    conditions
}

/// Whether running `sql` may add, change or remove tables, views or columns, so completion
/// metadata should be read again.
pub fn may_change_schema(sql: &str, dialect: Dialect) -> bool {
    let (tokens, _) = tokenize(sql, dialect);
    tokens.iter().enumerate().any(|(index, token)| {
        ["create", "alter", "drop", "sp_rename"]
            .iter()
            .any(|keyword| token.is_keyword(keyword))
            // `SELECT … INTO t` creates a table, unlike `INSERT INTO t`.
            || (token.is_keyword("into")
                && !index
                    .checked_sub(1)
                    .and_then(|previous| tokens.get(previous))
                    .is_some_and(|previous| previous.is_keyword("insert") || previous.is_keyword("merge")))
    })
}

pub fn complete(
    catalog: Option<&Catalog>,
    text: &str,
    offset: usize,
    dialect: Dialect,
) -> Completions {
    let offset = offset.min(text.len());
    let statement = sql::statement_at(text, offset, dialect)
        .map(|range| range.start.min(offset)..range.end.max(offset))
        .unwrap_or(0..text.len());
    let statement_text = text.get(statement.clone()).unwrap_or_default();
    let cursor = offset - statement.start;
    let (tokens, inert) = tokenize(statement_text, dialect);
    if inert.iter().any(|inert| inert.contains(cursor)) {
        return Completions {
            replace: offset..offset,
            candidates: Vec::new(),
        };
    }

    // The partly typed name, if the cursor is in or just after one.
    let typed = tokens.iter().position(|token| {
        matches!(token.kind, TokenKind::Word { .. })
            && token.range.start < cursor
            && cursor <= token.range.end
    });
    let (replace_start, quoted, before) = match typed {
        Some(index) => {
            let token = &tokens[index];
            let quoted = matches!(token.kind, TokenKind::Word { quoted: true, .. });
            (token.range.start, quoted, index)
        }
        None => (
            cursor,
            false,
            tokens
                .iter()
                .position(|token| token.range.start >= cursor)
                .unwrap_or(tokens.len()),
        ),
    };
    let replace = statement.start + replace_start..offset;
    let insert = Inserter { dialect, quoted };

    // A qualifier: `alias.`, `schema.` or `schema.table.` right before the typed name.
    let mut qualifier = Vec::new();
    let mut context_index = before;
    while context_index >= 2
        && matches!(tokens[context_index - 1].kind, TokenKind::Dot)
        && let Some(word) = tokens[context_index - 2].word()
    {
        qualifier.insert(0, word.to_string());
        context_index -= 2;
    }
    let previous = context_index
        .checked_sub(1)
        .and_then(|index| tokens.get(index));

    let keywords = || {
        KEYWORDS
            .iter()
            .chain(match dialect {
                Dialect::SqlServer => SQL_SERVER_KEYWORDS,
                Dialect::Postgres => POSTGRES_KEYWORDS,
            })
            .map(|keyword| Candidate {
                label: keyword.to_string(),
                insert: keyword.to_string(),
                kind: CandidateKind::Keyword,
                detail: None,
                priority: 6,
            })
    };

    let Some(catalog) = catalog else {
        let candidates = if qualifier.is_empty() {
            keywords().collect()
        } else {
            Vec::new()
        };
        return Completions {
            replace,
            candidates,
        };
    };

    let references = table_references(statement_text, &tokens)
        .into_iter()
        // A reference whose name is being typed is incomplete.
        .filter(|reference| !(reference.start <= context_index && context_index < reference.end))
        .collect::<Vec<_>>();
    let mut candidates = Vec::new();

    if !qualifier.is_empty() {
        let object = match qualifier.as_slice() {
            [single] => references
                .iter()
                .find(|reference| reference.matches_qualifier(single))
                .and_then(|reference| reference.resolve(catalog))
                .or_else(|| {
                    catalog
                        .find(None, single)
                        .filter(|_| !catalog.has_schema(single))
                }),
            [.., schema, name] => catalog.find(Some(schema), name),
            [] => None,
        };
        match object {
            Some(object) => {
                for column in &object.columns {
                    candidates.push(Candidate {
                        label: column.name.clone(),
                        insert: insert.name(&column.name),
                        kind: CandidateKind::Column,
                        detail: column_detail(column),
                        priority: 0,
                    });
                }
            }
            None => {
                if let [schema] = qualifier.as_slice()
                    && catalog.has_schema(schema)
                {
                    for object in catalog.objects_in(schema) {
                        let mut candidate = object_candidate(catalog, object, &insert, 0);
                        candidate.label = object.name.name.clone();
                        candidate.insert = insert.name(&object.name.name);
                        candidates.push(candidate);
                    }
                }
            }
        }
        return Completions {
            replace,
            candidates,
        };
    }

    let in_from_list = previous.is_some_and(|token| matches!(token.kind, TokenKind::Comma))
        && tokens[..context_index]
            .iter()
            .rev()
            .find(|token| token.is_reserved() && !token.is_keyword("as"))
            .is_some_and(|token| token.is_keyword("from"));
    let wants_table = in_from_list
        || previous.is_some_and(|token| {
            TABLE_KEYWORDS
                .iter()
                .any(|keyword| token.is_keyword(keyword))
        });

    let resolved = references
        .iter()
        .filter_map(|reference| Some((reference, reference.resolve(catalog)?)))
        .collect::<Vec<_>>();

    if wants_table {
        if previous.is_some_and(|token| token.is_keyword("join")) {
            let taken = references
                .iter()
                .map(TableReference::qualifier)
                .collect::<Vec<_>>();
            for object in &catalog.objects {
                let alias = alias_for(&object.name.name, &taken);
                for (reference, existing) in &resolved {
                    for condition in join_conditions(
                        catalog,
                        (&object.name, &alias),
                        (&existing.name, &reference.qualifier()),
                    ) {
                        let table = insert.object(catalog, &object.name);
                        candidates.push(Candidate {
                            label: format!("{} {alias} ON {condition}", object.name.name),
                            insert: format!("{table} {alias} ON {condition}"),
                            kind: CandidateKind::Join,
                            detail: Some("foreign key".into()),
                            priority: 0,
                        });
                    }
                }
            }
        }
        for object in &catalog.objects {
            candidates.push(object_candidate(catalog, object, &insert, 2));
        }
        for schema in &catalog.schemas {
            candidates.push(Candidate {
                label: schema.clone(),
                insert: insert.name(schema),
                kind: CandidateKind::Schema,
                detail: Some("schema".into()),
                priority: 4,
            });
        }
        return Completions {
            replace,
            candidates,
        };
    }

    if previous.is_some_and(|token| token.is_keyword("on"))
        && let Some(joined_index) = resolved
            .iter()
            .rposition(|(reference, _)| reference.is_join && reference.end <= context_index)
    {
        let (joined_reference, joined) = resolved[joined_index];
        for (reference, existing) in resolved.iter().take(joined_index) {
            for condition in join_conditions(
                catalog,
                (&joined.name, &joined_reference.qualifier()),
                (&existing.name, &reference.qualifier()),
            ) {
                candidates.push(Candidate {
                    label: condition.clone(),
                    insert: condition,
                    kind: CandidateKind::JoinCondition,
                    detail: Some("foreign key".into()),
                    priority: 0,
                });
            }
        }
    }

    let qualify = resolved.len() > 1;
    for (reference, object) in &resolved {
        for column in &object.columns {
            let ambiguous = resolved.iter().any(|(other, other_object)| {
                !std::ptr::eq(*other, *reference)
                    && other_object
                        .columns
                        .iter()
                        .any(|other_column| other_column.name.eq_ignore_ascii_case(&column.name))
            });
            let mut detail = column_detail(column).unwrap_or_default();
            if qualify {
                if !detail.is_empty() {
                    detail.push_str(" · ");
                }
                detail.push_str(&reference.qualifier());
            }
            candidates.push(Candidate {
                label: column.name.clone(),
                insert: if ambiguous {
                    format!("{}.{}", reference.qualifier(), insert.plain(&column.name))
                } else {
                    insert.name(&column.name)
                },
                kind: CandidateKind::Column,
                detail: (!detail.is_empty()).then_some(detail),
                priority: 1,
            });
        }
    }
    for reference in &references {
        let qualifier = reference.qualifier();
        if qualifier.is_empty() {
            continue;
        }
        candidates.push(Candidate {
            label: reference
                .alias
                .clone()
                .or_else(|| reference.parts.last().cloned())
                .unwrap_or_default(),
            insert: qualifier,
            kind: CandidateKind::Alias,
            detail: Some(reference.parts.join(".")).filter(|name| !name.is_empty()),
            priority: 3,
        });
    }
    candidates.extend(keywords());
    Completions {
        replace,
        candidates,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ColumnSummary, catalog::ForeignKey};

    fn column(name: &str, type_name: &str, primary_key: bool) -> ColumnSummary {
        ColumnSummary {
            name: name.into(),
            type_name: type_name.into(),
            nullable: !primary_key,
            primary_key,
        }
    }

    fn name(schema: &str, table: &str) -> ObjectName {
        ObjectName {
            schema: schema.into(),
            name: table.into(),
        }
    }

    fn catalog() -> Catalog {
        Catalog {
            default_schema: Some("dbo".into()),
            schemas: vec!["dbo".into(), "sales".into()],
            objects: vec![
                CatalogObject {
                    name: name("dbo", "Orders"),
                    kind: ObjectKind::Table,
                    columns: vec![
                        column("Id", "int", true),
                        column("CustomerId", "int", false),
                        column("Total", "decimal(10,2)", false),
                    ],
                },
                CatalogObject {
                    name: name("dbo", "Customers"),
                    kind: ObjectKind::Table,
                    columns: vec![
                        column("Id", "int", true),
                        column("Name", "nvarchar(50)", false),
                    ],
                },
                CatalogObject {
                    name: name("dbo", "Order Lines"),
                    kind: ObjectKind::Table,
                    columns: vec![column("OrderId", "int", true), column("Qty", "int", false)],
                },
                CatalogObject {
                    name: name("sales", "Regions"),
                    kind: ObjectKind::View,
                    columns: vec![column("Code", "char(2)", false)],
                },
            ],
            foreign_keys: vec![
                ForeignKey {
                    name: "FK_Orders_Customers".into(),
                    table: name("dbo", "Orders"),
                    columns: vec!["CustomerId".into()],
                    referenced_table: name("dbo", "Customers"),
                    referenced_columns: vec!["Id".into()],
                },
                ForeignKey {
                    name: "FK_Lines_Orders".into(),
                    table: name("dbo", "Order Lines"),
                    columns: vec!["OrderId".into()],
                    referenced_table: name("dbo", "Orders"),
                    referenced_columns: vec!["Id".into()],
                },
            ],
        }
    }

    /// Completes at the `|` in `text`.
    fn complete_at(text: &str, dialect: Dialect) -> (String, Vec<Candidate>) {
        let offset = text.find('|').expect("a cursor marker");
        let text = text.replacen('|', "", 1);
        let completions = complete(Some(&catalog()), &text, offset, dialect);
        (
            text[completions.replace].to_string(),
            completions.candidates,
        )
    }

    fn inserts(candidates: &[Candidate], kind: CandidateKind) -> Vec<String> {
        candidates
            .iter()
            .filter(|candidate| candidate.kind == kind)
            .map(|candidate| candidate.insert.clone())
            .collect()
    }

    #[test]
    fn resolves_aliases_after_a_dot() {
        for text in [
            "SELECT o.| FROM dbo.Orders o",
            "SELECT o.To| FROM dbo.Orders AS o WHERE o.Id = 1",
            "SELECT * FROM [dbo].[Orders] o WHERE o.|",
            "SELECT * FROM Orders o JOIN Customers c ON c.Id = o.|",
        ] {
            let (typed, candidates) = complete_at(text, Dialect::SqlServer);
            assert!(typed.is_empty() || typed == "To", "{text}: {typed:?}");
            assert_eq!(
                inserts(&candidates, CandidateKind::Column),
                ["Id", "CustomerId", "Total"],
                "{text}"
            );
            assert_eq!(candidates[0].detail.as_deref(), Some("int · key"));
        }

        let (_, candidates) = complete_at(
            "SELECT c.| FROM Orders o JOIN Customers c ON c.Id = o.CustomerId",
            Dialect::SqlServer,
        );
        assert_eq!(inserts(&candidates, CandidateKind::Column), ["Id", "Name"]);

        // A table's own name qualifies it when it has no alias; a schema lists its objects.
        let (_, candidates) = complete_at("SELECT Customers.| FROM Customers", Dialect::SqlServer);
        assert_eq!(inserts(&candidates, CandidateKind::Column), ["Id", "Name"]);
        let (_, candidates) = complete_at("SELECT * FROM sales.|", Dialect::SqlServer);
        assert_eq!(inserts(&candidates, CandidateKind::View), ["Regions"]);
        let (_, candidates) = complete_at("SELECT * FROM dbo.Ord|", Dialect::SqlServer);
        assert!(inserts(&candidates, CandidateKind::Table).contains(&"[Order Lines]".to_string()));
        // An unknown alias offers nothing rather than guessing.
        let (_, candidates) = complete_at("SELECT x.| FROM Orders o", Dialect::SqlServer);
        assert!(candidates.is_empty());
    }

    #[test]
    fn offers_tables_after_from_and_join() {
        let (typed, candidates) = complete_at("SELECT * FROM Cu|", Dialect::SqlServer);
        assert_eq!(typed, "Cu");
        let tables = inserts(&candidates, CandidateKind::Table);
        assert_eq!(tables, ["Orders", "Customers", "[Order Lines]"]);
        assert_eq!(inserts(&candidates, CandidateKind::View), ["sales.Regions"]);
        assert!(inserts(&candidates, CandidateKind::Keyword).is_empty());

        let (_, candidates) = complete_at("SELECT * FROM Orders o, |", Dialect::SqlServer);
        assert!(!inserts(&candidates, CandidateKind::Table).is_empty());
        let (_, candidates) = complete_at("UPDATE |", Dialect::SqlServer);
        assert!(!inserts(&candidates, CandidateKind::Table).is_empty());
        let (typed, candidates) = complete_at("SELECT * FROM [Ord|", Dialect::SqlServer);
        assert_eq!(typed, "[Ord");
        assert!(inserts(&candidates, CandidateKind::Table).contains(&"[Orders]".to_string()));
    }

    #[test]
    fn suggests_joins_and_conditions_from_foreign_keys() {
        let (_, candidates) = complete_at("SELECT * FROM dbo.Orders o JOIN |", Dialect::SqlServer);
        let joins = inserts(&candidates, CandidateKind::Join);
        assert!(
            joins.contains(&"Customers c ON c.Id = o.CustomerId".to_string()),
            "{joins:?}"
        );
        assert!(
            joins.contains(&"[Order Lines] ol ON ol.OrderId = o.Id".to_string()),
            "{joins:?}"
        );

        let (_, candidates) = complete_at(
            "SELECT * FROM Orders o INNER JOIN Customers AS c ON |",
            Dialect::SqlServer,
        );
        assert_eq!(
            inserts(&candidates, CandidateKind::JoinCondition),
            ["c.Id = o.CustomerId"]
        );
        // Columns come qualified when both tables have them.
        let columns = inserts(&candidates, CandidateKind::Column);
        assert!(columns.contains(&"o.Id".to_string()), "{columns:?}");
        assert!(columns.contains(&"Total".to_string()), "{columns:?}");

        // Unaliased tables are qualified by name, and the join's own alias is avoided.
        let (_, candidates) = complete_at("SELECT * FROM Customers c JOIN |", Dialect::SqlServer);
        let joins = inserts(&candidates, CandidateKind::Join);
        assert!(
            joins.contains(&"Orders o ON o.CustomerId = c.Id".to_string()),
            "{joins:?}"
        );
        let (_, candidates) = complete_at("SELECT * FROM Orders JOIN |", Dialect::SqlServer);
        let joins = inserts(&candidates, CandidateKind::Join);
        assert!(
            joins.contains(&"Customers c ON c.Id = Orders.CustomerId".to_string()),
            "{joins:?}"
        );
    }

    #[test]
    fn offers_columns_in_scope_and_keywords_elsewhere() {
        let (_, candidates) = complete_at("SELECT | FROM Orders", Dialect::SqlServer);
        assert_eq!(
            inserts(&candidates, CandidateKind::Column),
            ["Id", "CustomerId", "Total"]
        );
        assert!(inserts(&candidates, CandidateKind::Keyword).contains(&"TOP".to_string()));

        let (_, candidates) = complete_at("|", Dialect::Postgres);
        assert!(inserts(&candidates, CandidateKind::Column).is_empty());
        let keywords = inserts(&candidates, CandidateKind::Keyword);
        assert!(keywords.contains(&"RETURNING".to_string()));
        assert!(!keywords.contains(&"TOP".to_string()));

        // Only the statement under the cursor counts.
        let (_, candidates) = complete_at(
            "SELECT * FROM Customers;\nSELECT | FROM Orders",
            Dialect::SqlServer,
        );
        assert!(!inserts(&candidates, CandidateKind::Column).contains(&"Name".to_string()));

        // Nothing is offered inside strings and comments.
        for text in [
            "SELECT 'Ord|' FROM Orders",
            "SELECT 'unterminated|",
            "SELECT 1 -- note|",
            "SELECT /* o.| */ 1 FROM Orders o",
        ] {
            assert!(complete_at(text, Dialect::SqlServer).1.is_empty(), "{text}");
        }
        assert!(!complete_at("SELECT 'a' |", Dialect::SqlServer).1.is_empty());

        // Strings and comments don't confuse the analysis.
        let (_, candidates) = complete_at(
            "SELECT 'FROM Customers c' -- JOIN x\n, o.| FROM /* c */ Orders o",
            Dialect::SqlServer,
        );
        assert_eq!(
            inserts(&candidates, CandidateKind::Column),
            ["Id", "CustomerId", "Total"]
        );
    }

    #[test]
    fn quotes_names_for_the_dialect() {
        let (_, candidates) = complete_at("SELECT * FROM |", Dialect::Postgres);
        // Postgres folds unquoted names to lower case, so capitals need quotes.
        assert!(inserts(&candidates, CandidateKind::Table).contains(&"\"Orders\"".to_string()));
        let (typed, candidates) = complete_at("SELECT * FROM \"Ord|", Dialect::Postgres);
        assert_eq!(typed, "\"Ord");
        assert!(inserts(&candidates, CandidateKind::Table).contains(&"\"Orders\"".to_string()));
    }

    #[test]
    fn detects_schema_changes() {
        for sql in [
            "CREATE TABLE t (id int)",
            "alter table t add c int",
            "DROP VIEW v",
            "EXEC sp_rename 'a', 'b'",
            "SELECT * INTO #copy FROM t",
        ] {
            assert!(may_change_schema(sql, Dialect::SqlServer), "{sql}");
        }
        for sql in [
            "SELECT 'DROP TABLE t'",
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET created = 1 -- create",
            "MERGE INTO t USING s ON 1 = 1 WHEN MATCHED THEN DELETE;",
        ] {
            assert!(!may_change_schema(sql, Dialect::SqlServer), "{sql}");
        }
    }

    #[test]
    fn makes_short_unique_aliases() {
        assert_eq!(alias_for("Customers", &[]), "c");
        assert_eq!(alias_for("OrderLines", &[]), "ol");
        assert_eq!(alias_for("order_lines", &[]), "ol");
        assert_eq!(alias_for("Orders", &["o".into()]), "o2");
        // `on` is a keyword.
        assert_eq!(alias_for("OrderNotes", &[]), "on2");
    }

    #[test]
    fn offers_keywords_without_a_catalog() {
        let completions = complete(None, "SEL", 3, Dialect::SqlServer);
        assert_eq!(completions.replace, 0..3);
        assert!(
            completions
                .candidates
                .iter()
                .all(|candidate| candidate.kind == CandidateKind::Keyword)
        );
        assert!(
            complete(None, "SELECT o.", 9, Dialect::SqlServer)
                .candidates
                .is_empty()
        );
    }
}

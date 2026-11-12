//! Catalog metadata for SQL completion: schemas, tables and views with their columns, and
//! foreign keys. Loaded in a few bulk queries per database.

use std::collections::HashMap;

use crate::{ColumnSummary, ObjectKind};

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectName {
    pub schema: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogObject {
    pub name: ObjectName,
    pub kind: ObjectKind,
    pub columns: Vec<ColumnSummary>,
}

/// A foreign key; `columns[i]` references `referenced_columns[i]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForeignKey {
    pub name: String,
    pub table: ObjectName,
    pub columns: Vec<String>,
    pub referenced_table: ObjectName,
    pub referenced_columns: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Catalog {
    /// Schema that unqualified names resolve to (`dbo`, or the first schema on Postgres's
    /// search path).
    pub default_schema: Option<String>,
    pub schemas: Vec<String>,
    pub objects: Vec<CatalogObject>,
    pub foreign_keys: Vec<ForeignKey>,
}

impl Catalog {
    /// Finds a table or view, matching names case-insensitively as both supported servers do
    /// by default. An unqualified name prefers the default schema, then any schema.
    pub fn find(&self, schema: Option<&str>, name: &str) -> Option<&CatalogObject> {
        let matches_name = |object: &&CatalogObject| object.name.name.eq_ignore_ascii_case(name);
        match schema {
            Some(schema) => self
                .objects
                .iter()
                .filter(matches_name)
                .find(|object| object.name.schema.eq_ignore_ascii_case(schema)),
            None => {
                let default_schema = self.default_schema.as_deref();
                self.objects
                    .iter()
                    .filter(matches_name)
                    .find(|object| {
                        default_schema
                            .is_some_and(|default| object.name.schema.eq_ignore_ascii_case(default))
                    })
                    .or_else(|| self.objects.iter().find(matches_name))
            }
        }
    }

    pub fn has_schema(&self, schema: &str) -> bool {
        self.schemas
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(schema))
    }

    pub fn objects_in(&self, schema: &str) -> impl Iterator<Item = &CatalogObject> {
        self.objects
            .iter()
            .filter(move |object| object.name.schema.eq_ignore_ascii_case(schema))
    }

    /// Foreign keys between `left` and `right`, in either direction.
    pub fn foreign_keys_between<'a>(
        &'a self,
        left: &'a ObjectName,
        right: &'a ObjectName,
    ) -> impl Iterator<Item = &'a ForeignKey> {
        self.foreign_keys.iter().filter(move |key| {
            (same_object(&key.table, left) && same_object(&key.referenced_table, right))
                || (same_object(&key.table, right) && same_object(&key.referenced_table, left))
        })
    }
}

pub fn same_object(left: &ObjectName, right: &ObjectName) -> bool {
    left.schema.eq_ignore_ascii_case(&right.schema) && left.name.eq_ignore_ascii_case(&right.name)
}

/// Builds catalog objects from `(schema, table, kind, column)` rows ordered by table, the shape
/// the drivers' bulk column queries return.
pub(crate) fn group_columns(
    rows: impl IntoIterator<Item = (ObjectName, ObjectKind, ColumnSummary)>,
) -> Vec<CatalogObject> {
    let mut objects = Vec::<CatalogObject>::new();
    let mut positions = HashMap::<ObjectName, usize>::new();
    for (name, kind, column) in rows {
        let position = *positions.entry(name.clone()).or_insert_with(|| {
            objects.push(CatalogObject {
                name: name.clone(),
                kind,
                columns: Vec::new(),
            });
            objects.len() - 1
        });
        if let Some(object) = objects.get_mut(position) {
            object.columns.push(column);
        }
    }
    objects
}

/// Builds foreign keys from one row per key column, ordered by key and column position.
pub(crate) fn group_foreign_keys(
    rows: impl IntoIterator<Item = (String, ObjectName, String, ObjectName, String)>,
) -> Vec<ForeignKey> {
    let mut keys = Vec::<ForeignKey>::new();
    for (name, table, column, referenced_table, referenced_column) in rows {
        match keys.last_mut() {
            Some(key) if key.name == name && key.table == table => {
                key.columns.push(column);
                key.referenced_columns.push(referenced_column);
            }
            _ => keys.push(ForeignKey {
                name,
                table,
                columns: vec![column],
                referenced_table,
                referenced_columns: vec![referenced_column],
            }),
        }
    }
    keys
}

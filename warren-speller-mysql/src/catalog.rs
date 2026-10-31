// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! What a respelling reads of a database: each table's columns (the type each compares as, whether
//! it may hold NULL, whether `SELECT *` lists it, the expression a generated column is computed
//! from, whether a key holds it) and its unique keys. It is read from the server's own tables,
//! through the query [`query`] writes, or built by the caller.

use crate::Refused;

/// The tables of one database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalog {
    pub database: String,
    pub tables: Vec<Table>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub name: String,
    /// The columns in the table's order, generated columns included.
    pub columns: Vec<Column>,
    /// The table's unique keys whose columns all hold no NULL: its primary key, and each unique
    /// index on columns alone.
    pub unique: Vec<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub not_null: bool,
    pub kind: Kind,
    /// Whether `SELECT *` returns it: an invisible column it does not.
    pub listed: bool,
    /// The expression a generated column is computed from, as MySQL writes it; none for a column
    /// that is not generated.
    pub generated: Option<String>,
    /// Whether an index of the table holds it as a key part.
    pub keyed: bool,
}

/// How MySQL compares a column's values with another's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// A whole number, of whatever width.
    Integer,
    Decimal,
    /// FLOAT and DOUBLE.
    Float,
    /// Text in a collation.
    Text(String),
    DateTime,
    Date,
    Timestamp,
    /// Any other type, by MySQL's name for it.
    Other(String),
}

impl Table {
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.name == name)
    }
}

impl Catalog {
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.iter().find(|t| t.name == name)
    }

    /// The catalog read from the rows [`query`] returns, each value as text or NULL.
    pub fn from_rows(rows: &[Vec<Option<String>>]) -> Result<Catalog, Refused> {
        let field =
            |row: &[Option<String>], i: usize| -> Option<String> { row.get(i).cloned().flatten() };
        let mut database: Option<String> = None;
        let mut tables: Vec<Table> = Vec::new();
        // (table, index, unique, position, column or none for an expression)
        let mut key_parts: Vec<(String, String, bool, u32, Option<String>)> = Vec::new();
        for (n, row) in rows.iter().enumerate() {
            let at = |what: &str| Refused(format!("catalog row {}: {what}", n + 1));
            if row.len() != FIELDS {
                return Err(at(&format!(
                    "{} fields, and a catalog row has {FIELDS}",
                    row.len()
                )));
            }
            let schema = field(row, 1).ok_or_else(|| at("no database"))?;
            match &database {
                None => database = Some(schema),
                Some(d) if *d != schema => {
                    return Err(at(&format!("the database {schema}, after {d}")));
                }
                Some(_) => {}
            }
            let table = field(row, 2).ok_or_else(|| at("no table"))?;
            match field(row, 0).as_deref() {
                Some("column") => {
                    let name = field(row, 3).ok_or_else(|| at("no column name"))?;
                    let nullable = field(row, 4).ok_or_else(|| at("no nullability"))?;
                    let data_type = field(row, 5).ok_or_else(|| at("no data type"))?;
                    let collation = field(row, 6);
                    let expression = field(row, 7).unwrap_or_default();
                    let extra = field(row, 8).unwrap_or_default();
                    let kind = kind_named(&data_type, collation.as_deref())
                        .ok_or_else(|| at(&format!("{data_type} is text with no collation")))?;
                    let column = Column {
                        name,
                        not_null: nullable == "NO",
                        kind,
                        listed: !extra.to_ascii_uppercase().contains("INVISIBLE"),
                        generated: (!expression.is_empty()).then_some(expression),
                        keyed: false,
                    };
                    match tables.iter_mut().find(|t| t.name == table) {
                        Some(t) => t.columns.push(column),
                        None => tables.push(Table {
                            name: table,
                            columns: vec![column],
                            unique: Vec::new(),
                        }),
                    }
                }
                Some("key") => {
                    let index = field(row, 3).ok_or_else(|| at("no index name"))?;
                    let unique = field(row, 4).as_deref() == Some("0");
                    let position: u32 = field(row, 5)
                        .and_then(|s| s.parse().ok())
                        .ok_or_else(|| at("no position in the index"))?;
                    key_parts.push((table, index, unique, position, field(row, 6)));
                }
                _ => return Err(at("neither a column nor a key part")),
            }
        }
        let database = database.ok_or_else(|| Refused("the catalog holds no table".into()))?;
        key_parts.sort_by(|a, b| (&a.0, &a.1, a.3).cmp(&(&b.0, &b.1, b.3)));
        for t in &mut tables {
            let indexes: Vec<&String> = {
                let mut v: Vec<&String> = key_parts
                    .iter()
                    .filter(|p| p.0 == t.name)
                    .map(|p| &p.1)
                    .collect();
                v.dedup();
                v
            };
            for index in indexes {
                let parts: Vec<_> = key_parts
                    .iter()
                    .filter(|p| p.0 == t.name && p.1 == *index)
                    .collect();
                if !parts.iter().all(|p| p.2) {
                    continue;
                }
                let columns: Option<Vec<String>> = parts.iter().map(|p| p.4.clone()).collect();
                let Some(columns) = columns else { continue };
                let not_null = columns
                    .iter()
                    .all(|c| t.columns.iter().any(|x| x.name == *c && x.not_null));
                if not_null && !t.unique.contains(&columns) {
                    t.unique.push(columns);
                }
            }
            for c in &mut t.columns {
                c.keyed = key_parts
                    .iter()
                    .any(|p| p.0 == t.name && p.4.as_ref() == Some(&c.name));
            }
        }
        Ok(Catalog { database, tables })
    }

    /// The catalog read from what the `mysql` client prints for [`query`] with `--batch
    /// --skip-column-names`: a row a line, its values between tabs, NULL as `NULL`, and a tab, a
    /// line break, a backslash or a NUL in a value escaped with a backslash.
    pub fn parse(text: &str) -> Result<Catalog, Refused> {
        let rows: Vec<Vec<Option<String>>> = text
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| {
                l.split('\t')
                    .map(|v| if v == "NULL" { None } else { Some(unescape(v)) })
                    .collect()
            })
            .collect();
        Catalog::from_rows(&rows)
    }
}

/// The number of values in a row of [`query`].
const FIELDS: usize = 9;

/// The query whose rows are a database's catalog: a row for each column, and a row for each part
/// of each index. A key row's last two values are NULL: nothing reads them, and MariaDB's
/// `STATISTICS` has neither `EXPRESSION` nor `IS_VISIBLE`, so the query runs on both servers.
pub fn query(database: &str) -> String {
    let db = format!("'{}'", database.replace('\\', "\\\\").replace('\'', "''"));
    format!(
        "SELECT 'column', TABLE_SCHEMA, TABLE_NAME, COLUMN_NAME, IS_NULLABLE, DATA_TYPE, COLLATION_NAME, \
         GENERATION_EXPRESSION, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = {db}\n\
         UNION ALL\n\
         SELECT 'key', TABLE_SCHEMA, TABLE_NAME, INDEX_NAME, NON_UNIQUE, SEQ_IN_INDEX, COLUMN_NAME, \
         NULL, NULL FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = {db};\n"
    )
}

fn unescape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('0') => out.push('\0'),
            Some('\\') => out.push('\\'),
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// The kind of a column by MySQL's name for its type; none for text with no collation.
fn kind_named(data_type: &str, collation: Option<&str>) -> Option<Kind> {
    let t = data_type.to_ascii_lowercase();
    Some(match t.as_str() {
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" => Kind::Integer,
        "decimal" | "numeric" => Kind::Decimal,
        "float" | "double" | "real" => Kind::Float,
        "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" => {
            Kind::Text(collation?.to_string())
        }
        "datetime" => Kind::DateTime,
        "date" => Kind::Date,
        "timestamp" => Kind::Timestamp,
        _ => Kind::Other(t),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(values: &[&str]) -> Vec<Option<String>> {
        values
            .iter()
            .map(|v| {
                if *v == "NULL" {
                    None
                } else {
                    Some(v.to_string())
                }
            })
            .collect()
    }

    #[test]
    fn a_unique_index_on_columns_that_hold_no_null_is_a_key() {
        let c = Catalog::from_rows(&[
            row(&["column", "d", "t", "a", "NO", "int", "NULL", "", ""]),
            row(&[
                "column",
                "d",
                "t",
                "b",
                "YES",
                "varchar",
                "utf8mb4_0900_bin",
                "",
                "",
            ]),
            row(&[
                "column",
                "d",
                "t",
                "c",
                "NO",
                "varchar",
                "utf8mb4_0900_ai_ci",
                "",
                "",
            ]),
            row(&["key", "d", "t", "PRIMARY", "0", "1", "a", "NULL", "YES"]),
            row(&["key", "d", "t", "u_b", "0", "1", "b", "NULL", "YES"]),
            row(&["key", "d", "t", "u_ca", "0", "2", "a", "NULL", "NO"]),
            row(&["key", "d", "t", "u_ca", "0", "1", "c", "NULL", "NO"]),
            row(&["key", "d", "t", "u_f", "0", "1", "NULL", "(`a` + 1)", "YES"]),
            row(&["key", "d", "t", "k_c", "1", "1", "c", "NULL", "YES"]),
        ])
        .unwrap();
        let t = c.table("t").unwrap();
        assert_eq!(c.database, "d");
        assert_eq!(
            t.unique,
            vec![
                vec!["a".to_string()],
                vec!["c".to_string(), "a".to_string()]
            ]
        );
        assert_eq!(t.columns[2].kind, Kind::Text("utf8mb4_0900_ai_ci".into()));
        assert!(t.columns[0].not_null && !t.columns[1].not_null);
    }

    #[test]
    fn a_generated_column_and_whether_a_key_holds_it_are_read() {
        let c = Catalog::from_rows(&[
            row(&["column", "d", "t", "at", "NO", "datetime", "NULL", "", ""]),
            row(&[
                "column",
                "d",
                "t",
                "at_month",
                "YES",
                "tinyint",
                "NULL",
                "month(`at`)",
                "VIRTUAL GENERATED INVISIBLE",
            ]),
            row(&[
                "column",
                "d",
                "t",
                "at_year",
                "YES",
                "smallint",
                "NULL",
                "year(`at`)",
                "VIRTUAL GENERATED",
            ]),
            row(&["key", "d", "t", "k", "1", "1", "at_month", "NULL", "YES"]),
        ])
        .unwrap();
        let t = c.table("t").unwrap();
        assert_eq!(
            t.columns
                .iter()
                .map(|c| (c.name.as_str(), c.generated.as_deref(), c.keyed, c.listed))
                .collect::<Vec<_>>(),
            [
                ("at", None, false, true),
                ("at_month", Some("month(`at`)"), true, false),
                ("at_year", Some("year(`at`)"), false, true)
            ]
        );
    }

    #[test]
    fn the_clients_batch_output_is_read_with_its_escapes() {
        let text = "column\td\tt\ta\\tb\tNO\tint\tNULL\t\t\n\
                    key\td\tt\tPRIMARY\t0\t1\ta\\tb\tNULL\tYES\n";
        let c = Catalog::parse(text).unwrap();
        assert_eq!(c.table("t").unwrap().unique, vec![vec!["a\tb".to_string()]]);
    }

    #[test]
    fn rows_of_two_databases_or_of_another_shape_are_refused() {
        let why = Catalog::from_rows(&[
            row(&["column", "d", "t", "a", "NO", "int", "NULL", "", ""]),
            row(&["column", "e", "t", "b", "NO", "int", "NULL", "", ""]),
        ])
        .unwrap_err()
        .0;
        assert!(why.contains("the database e, after d"), "{why}");
        let why = Catalog::from_rows(&[row(&["column", "d", "t"])])
            .unwrap_err()
            .0;
        assert!(why.contains("3 fields"), "{why}");
        assert!(Catalog::from_rows(&[]).unwrap_err().0.contains("no table"));
    }

    #[test]
    fn the_query_quotes_the_database_name() {
        assert!(query("it's").contains("TABLE_SCHEMA = 'it''s'"));
    }
}

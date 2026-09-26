//! The scene a generated statement reads: a source of rows, how the planner
//! can reach them, what they hold, and the declared type of the column under
//! test.
//!
//! Invariant: **every source kind presents the same three columns, `k`, `a`
//! and `b`, holding the same rows, so a template writes one statement and the
//! axis decides only where the rows come from.** `k` names a row, `a` is the
//! column under test, whose declared type is the affinity axis, and `b` is a
//! text column. A template refers to them through [`Relation`]'s expressions,
//! never by spelling a table name, because a table valued function or an FTS5
//! table does not call its columns `k`, `a` and `b`.
//!
//! What persists goes in the case's setup and is shared through a fixture:
//! tables, views, indexes and their rows. What a reopen loses goes in the
//! case's own records ahead of the statement: a `TEMP` table, an attached
//! database, and their rows.

use crate::statement_matrix::case::Record;

/// The source kinds of section 4.2, less `inillucent_search`, which has no
/// SQLite equivalent and is graded in the `vector` family.
pub const SOURCES: &[&str] = &[
    "rowid",
    "ipk",
    "without_rowid",
    "strict",
    "view",
    "derived",
    "cte",
    "materialized",
    "tvf",
    "fts5",
    "temp",
    "attached",
];

/// The access paths of section 4.2.
pub const ACCESS: &[&str] = &[
    "none",
    "index",
    "covering",
    "partial",
    "expression",
    "collate",
    "unique",
    "analyze",
    "not_indexed",
];

/// The data sets of section 4.2.
pub const DATA: &[&str] = &[
    "empty",
    "one",
    "nulls",
    "duplicates",
    "mixed",
    "numeric_text",
    "nocase",
    "limits",
];

/// The declared types of the column under test. `text_nocase` is a `TEXT`
/// column declared `COLLATE NOCASE`, which is how `min`, `max` and an
/// aggregate `ORDER BY` meet a collation (section 1.1 of the design).
pub const AFFINITIES: &[&str] = &[
    "integer",
    "real",
    "text",
    "blob",
    "numeric",
    "none",
    "text_nocase",
];

/// The sources whose rows live in a table the statement can name and write.
pub const TABLES: &[&str] = &[
    "rowid",
    "ipk",
    "without_rowid",
    "strict",
    "temp",
    "attached",
];

/// Whether an access path means anything for a source, and whether the data
/// can be put in it. A shared constraint every family that takes these axes
/// applies.
///
/// @param source - the source kind, if chosen
/// @param access - the access path, if chosen
/// @param data - the data set, if chosen
pub fn scene_allowed(source: Option<&str>, access: Option<&str>, data: Option<&str>) -> bool {
    let bare = |value: Option<&str>, list: &[&str]| value.is_some_and(|one| list.contains(&one));
    // A table valued function and an FTS5 table have no index to reach.
    if bare(source, &["tvf", "fts5"]) && access.is_some_and(|one| one != "none") {
        return false;
    }
    // NOT INDEXED is written on a table name, and only a table has one.
    if access == Some("not_indexed") && source.is_some_and(|one| !TABLES.contains(&one)) {
        return false;
    }
    // JSON has no blob, and FTS5 tokenizes text; neither holds the mixed or
    // limit values meaningfully.
    if bare(source, &["tvf", "fts5"]) && bare(data, &["mixed", "limits"]) {
        return false;
    }
    true
}

/// A built scene: what the statement reads and how it is made.
#[derive(Clone, Debug, Default)]
pub struct Relation {
    /// Statements that persist: the case's setup.
    pub setup: Vec<Record>,
    /// Statements a reopen loses: the case's first records.
    pub prelude: Vec<Record>,
    /// A `WITH` clause the statement must start with, for a CTE source.
    pub with: String,
    /// The `FROM` term, aliased `s`.
    pub from: String,
    /// The row name, the column under test and the text column.
    pub k: String,
    /// The column under test.
    pub a: String,
    /// The text column.
    pub b: String,
    /// The table the rows are stored in, when the statement may write it.
    pub table: Option<String>,
    /// An expression naming each stored row once, for a table.
    pub key: String,
}

impl Relation {
    /// `SELECT <columns> FROM <source>`, with the `WITH` clause in front.
    ///
    /// @param columns - the select list
    /// @param rest - everything after the FROM term
    pub fn select(&self, columns: &str, rest: &str) -> String {
        let with = if self.with.is_empty() {
            String::new()
        } else {
            format!("{} ", self.with)
        };
        format!("{with}SELECT {columns} FROM {} {rest}", self.from)
            .trim_end()
            .to_string()
    }
}

/// The rows of a data set, as `(k, a, b)` SQL literals.
///
/// @param data - the data set
pub fn rows(data: &str) -> Vec<(i64, &'static str, &'static str)> {
    match data {
        "empty" => vec![],
        "one" => vec![(1, "5", "'x'")],
        "nulls" => vec![
            (1, "NULL", "'p'"),
            (2, "1", "NULL"),
            (3, "NULL", "NULL"),
            (4, "2", "'q'"),
        ],
        "duplicates" => vec![
            (1, "1", "'p'"),
            (2, "1", "'p'"),
            (3, "2", "'q'"),
            (4, "2", "'q'"),
            (5, "2", "'r'"),
            (6, "3", "'r'"),
        ],
        "mixed" => vec![
            (1, "1", "'p'"),
            (2, "2.5", "'q'"),
            (3, "'three'", "'r'"),
            (4, "x'04'", "'s'"),
            (5, "NULL", "'t'"),
            (6, "'5'", "'u'"),
        ],
        "numeric_text" => vec![
            (1, "'10'", "'p'"),
            (2, "'9'", "'q'"),
            (3, "'1e2'", "'r'"),
            (4, "' 7'", "'s'"),
            (5, "'0x10'", "'t'"),
        ],
        "nocase" => vec![
            (1, "'abc'", "'p'"),
            (2, "'ABC'", "'q'"),
            (3, "'Abc'", "'r'"),
            (4, "'b'", "'s'"),
            (5, "'B'", "'t'"),
        ],
        _ => vec![
            (1, "9223372036854775807", "'p'"),
            (2, "-9223372036854775808", "'q'"),
            (3, "1.7976931348623157e308", "'r'"),
            (4, "-0.0", "'s'"),
            (5, "4.9e-324", "'t'"),
        ],
    }
}

/// The declared type for an affinity, and its `STRICT` form.
///
/// @param affinity - the affinity axis value
/// @param strict - whether the table is `STRICT`
pub fn declared(affinity: &str, strict: bool) -> &'static str {
    match (affinity, strict) {
        ("integer", _) => "INTEGER",
        ("real", _) => "REAL",
        ("text", _) => "TEXT",
        ("blob", _) => "BLOB",
        ("text_nocase", _) => "TEXT COLLATE NOCASE",
        ("numeric", false) => "NUMERIC",
        ("none", false) => "",
        _ => "ANY",
    }
}

/// The statements that make one table of the scene's rows.
///
/// @param name - the table's qualified name
/// @param source - the source kind, which decides the table's form
/// @param affinity - the declared type of `a`
/// @param data - the rows
fn table_statements(name: &str, source: &str, affinity: &str, data: &str) -> Vec<Record> {
    let strict = source == "strict";
    let kind = declared(affinity, strict);
    if strict {
        let mut out = vec![Record::ok(format!(
            "CREATE TABLE {name}(k INTEGER, a {kind}, b TEXT) STRICT"
        ))];
        out.extend(insert_strict_rows(name, kind, data));
        return out;
    }
    let (key, tail) = match source {
        "ipk" => ("k INTEGER PRIMARY KEY", ""),
        "without_rowid" => ("k INTEGER PRIMARY KEY", " WITHOUT ROWID"),
        "strict" => ("k INTEGER", " STRICT"),
        _ => ("k INTEGER", ""),
    };
    let temp = if source == "temp" { "TEMP " } else { "" };
    let mut out = vec![Record::ok(format!(
        "CREATE {temp}TABLE {name}({key}, a {kind}, b TEXT){tail}"
    ))];
    out.extend(insert_rows(name, "k, a, b", data));
    out
}

/// `INSERT OR IGNORE` statements for a data set, which a `STRICT` column skips
/// a mistyped row of instead of failing the setup.
///
/// @param name - the table
/// @param columns - the column list
/// @param data - the rows
pub fn insert_rows(name: &str, columns: &str, data: &str) -> Vec<Record> {
    insert_rows_typed(name, columns, data, "ANY")
}

/// `INSERT OR IGNORE` statements for a data set into a table whose `a` column
/// is declared `kind`, leaving out the rows a `STRICT` column of that type
/// refuses. `ANY` keeps every row.
///
/// @param name - the table
/// @param columns - the column list
/// @param data - the rows
/// @param kind - the declared `STRICT` type of `a`, or `ANY`
pub fn insert_rows_typed(name: &str, columns: &str, data: &str, kind: &str) -> Vec<Record> {
    let values: Vec<String> = rows(data)
        .into_iter()
        .filter(|(_, a, _)| strict_accepts(kind, a))
        .map(|(k, a, b)| format!("({k}, {a}, {b})"))
        .collect();
    if values.is_empty() {
        return Vec::new();
    }
    vec![Record::ok(format!(
        "INSERT OR IGNORE INTO {name}({columns}) VALUES {}",
        values.join(", ")
    ))]
}

/// Whether a `STRICT` column of a declared type accepts a value, after the
/// type's own affinity has converted it.
///
/// `INSERT OR IGNORE` does not skip a value a `STRICT` column refuses: SQLite
/// raises the datatype error whatever the conflict clause says, so a setup
/// that inserted every row of a data set failed on both engines and graded
/// nothing. The rows a `STRICT` table cannot hold are left out instead, by
/// the rules SQLite documents for `STRICT` tables.
///
/// @param kind - the declared `STRICT` type
/// @param literal - the value as a SQL literal
pub fn strict_accepts(kind: &str, literal: &str) -> bool {
    let text = literal
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''));
    let blob = literal.starts_with("x'");
    if literal == "NULL" || kind == "ANY" {
        return true;
    }
    let integral = |value: f64| value.is_finite() && value.fract() == 0.0 && value.abs() < 9.2e18;
    let number = match text {
        Some(body) => body.trim().parse::<f64>().ok(),
        None if blob => None,
        None => literal.parse::<f64>().ok(),
    };
    let exact_integer = match text {
        Some(body) => body.trim().parse::<i64>().is_ok() || number.is_some_and(integral),
        None => literal.parse::<i64>().is_ok() || number.is_some_and(integral),
    };
    let base = kind.split_whitespace().next().unwrap_or(kind);
    match base {
        "INTEGER" => !blob && exact_integer,
        "REAL" => !blob && number.is_some(),
        "TEXT" => !blob,
        "BLOB" => blob,
        _ => true,
    }
}

/// The insert for a `STRICT` table: only the rows its column type accepts.
fn insert_strict_rows(name: &str, kind: &str, data: &str) -> Vec<Record> {
    let values: Vec<String> = rows(data)
        .into_iter()
        .filter(|(_, a, _)| strict_accepts(kind, a))
        .map(|(k, a, b)| format!("({k}, {a}, {b})"))
        .collect();
    if values.is_empty() {
        return Vec::new();
    }
    vec![Record::ok(format!(
        "INSERT INTO {name}(k, a, b) VALUES {}",
        values.join(", ")
    ))]
}

/// The index statements for an access path on a table.
///
/// @param access - the access path
/// @param schema - `aux.` for an attached table, or empty
/// @param table - the table's name, without its schema
/// @param tag - the scene's tag, which every index name carries
fn index_statements(access: &str, schema: &str, table: &str, tag: &str) -> Vec<Record> {
    let index = |sql: String| vec![Record::ok(sql)];
    match access {
        "index" => index(format!("CREATE INDEX {schema}ia{tag} ON {table}(a)")),
        "covering" => index(format!("CREATE INDEX {schema}iab{tag} ON {table}(a, b, k)")),
        "partial" => index(format!(
            "CREATE INDEX {schema}ip{tag} ON {table}(a) WHERE a IS NOT NULL"
        )),
        "expression" => index(format!(
            "CREATE INDEX {schema}ie{tag} ON {table}(coalesce(a, 0))"
        )),
        "collate" => index(format!(
            "CREATE INDEX {schema}ic{tag} ON {table}(a COLLATE NOCASE)"
        )),
        "unique" => index(format!("CREATE UNIQUE INDEX {schema}iu{tag} ON {table}(k)")),
        "analyze" => vec![
            Record::ok(format!("CREATE INDEX {schema}ia{tag} ON {table}(a)")),
            Record::ok(if schema.is_empty() {
                format!("ANALYZE {table}")
            } else {
                "ANALYZE aux".to_string()
            }),
        ],
        _ => Vec::new(),
    }
}

/// The suffix a scene's persistent objects carry: a short hash of the four
/// values that decide what they hold.
///
/// **Unique names are what let many cases share one fixture.** The runner
/// merges the setups of read only cases into one database when no two of them
/// define the same name differently (see `run.rs`), and without a suffix every
/// scene's table would be `t0`, so no two scenes could share one. A `TEMP`
/// table and an attached one are made in the case itself and keep plain names.
///
/// @param source - the source kind
/// @param access - the access path
/// @param data - the data set
/// @param affinity - the declared type of `a`
pub fn scene_tag(source: &str, access: &str, data: &str, affinity: &str) -> String {
    let digest =
        crate::hash::sha3_256_hex(format!("{source}|{access}|{data}|{affinity}").as_bytes());
    format!("_{}", digest.get(..6).unwrap_or(&digest))
}

/// Builds a scene.
///
/// @param source - the source kind
/// @param access - the access path
/// @param data - the data set
/// @param affinity - the declared type of `a`
pub fn relation(source: &str, access: &str, data: &str, affinity: &str) -> Relation {
    let mut relation = Relation {
        k: "s.k".to_string(),
        a: "s.a".to_string(),
        b: "s.b".to_string(),
        key: "k".to_string(),
        ..Relation::default()
    };
    let not_indexed = if access == "not_indexed" {
        " NOT INDEXED"
    } else {
        ""
    };
    let tag = scene_tag(source, access, data, affinity);
    let table = format!("t0{tag}");
    match source {
        "rowid" | "ipk" | "without_rowid" | "strict" => {
            relation.setup = table_statements(&table, source, affinity, data);
            relation
                .setup
                .extend(index_statements(access, "", &table, &tag));
            relation.from = format!("{table} AS s{not_indexed}");
            relation.key = if source == "without_rowid" {
                "k"
            } else {
                "rowid"
            }
            .to_string();
            relation.table = Some(table);
        }
        "temp" => {
            relation.prelude = table_statements("t0", "temp", affinity, data);
            relation
                .prelude
                .extend(index_statements(access, "", "t0", ""));
            relation.from = format!("t0 AS s{not_indexed}");
            relation.table = Some("t0".to_string());
            relation.key = "rowid".to_string();
        }
        "attached" => {
            relation.prelude = vec![Record::ok("ATTACH '%SCRATCH%/aux.db' AS aux")];
            relation
                .prelude
                .extend(table_statements("aux.t0", "attached", affinity, data));
            relation
                .prelude
                .extend(index_statements(access, "aux.", "t0", ""));
            relation.from = format!("aux.t0 AS s{not_indexed}");
            relation.table = Some("aux.t0".to_string());
            relation.key = "rowid".to_string();
        }
        "fts5" => {
            relation.setup = vec![Record::ok(format!(
                "CREATE VIRTUAL TABLE {table} USING fts5(a, b)"
            ))];
            relation
                .setup
                .extend(insert_rows(&table, "rowid, a, b", data));
            relation.from = format!("{table} AS s");
            relation.k = "s.rowid".to_string();
            relation.key = "rowid".to_string();
            relation.table = Some(table);
        }
        "tvf" => {
            relation.setup = table_statements(&table, "rowid", affinity, data);
            relation.from = format!(
                "json_each((SELECT json_group_array(a) FROM (SELECT a FROM {table} ORDER BY k))) AS s"
            );
            relation.k = "s.key".to_string();
            relation.a = "s.value".to_string();
            relation.b = "s.type".to_string();
        }
        wrapped => {
            relation.setup = table_statements(&table, "rowid", affinity, data);
            relation
                .setup
                .extend(index_statements(access, "", &table, &tag));
            match wrapped {
                "view" => {
                    relation.setup.push(Record::ok(format!(
                        "CREATE VIEW v0{tag} AS SELECT k, a, b FROM {table}"
                    )));
                    relation.from = format!("v0{tag} AS s");
                }
                "derived" => relation.from = format!("(SELECT k, a, b FROM {table}) AS s"),
                "cte" => {
                    relation.with = format!("WITH s AS (SELECT k, a, b FROM {table})");
                    relation.from = "s".to_string();
                }
                _ => {
                    relation.with = format!("WITH s AS MATERIALIZED (SELECT k, a, b FROM {table})");
                    relation.from = "s".to_string();
                }
            }
        }
    }
    relation
}

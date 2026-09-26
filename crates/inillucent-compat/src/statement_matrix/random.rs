//! Layer 4: random statements over a random small schema, graded against the
//! pinned SQLite, run by the nightly job from a seed that is the date.
//!
//! Invariant: **a seed and an index name one case exactly.** The generator is
//! a splitmix64 stream seeded per case from the run's seed and the case's
//! index, so `random-<seed>` case `n` is the same statements on every machine
//! and every day, and a failure the nightly prints can be run again with
//! `inillucent-matrix random --seed <seed> --only <n>`. Section 5.4 of
//! `tasks/task-2135-sql-statement-matrix-tdd.md` is the design.
//!
//! The grammar reaches what the axes of Layer 2 name and the combinations
//! they leave out: expressions nested to a depth of four, joins, grouping,
//! compounds, subqueries and writes, over tables whose declared types and
//! values are drawn at random. It avoids what would make two correct engines
//! disagree: no `random()`, no clock, no `group_concat` without an order, and
//! reals drawn only from values whose sums are exact in binary, so the order a
//! sum is taken in cannot change it.
//!
//! A random case's id changes when the generator changes, so `known.list`
//! cannot name one. A difference that is a recorded defect is matched by
//! construct instead, by a rule in `corpora/matrix/random-known.toml` that
//! names the bug's number; any other difference fails the nightly and is
//! shrunk and written out for a person to retain.

use crate::statement_matrix::case::{Case, Record, Sort};

/// A splitmix64 stream: small, fast and the same on every platform.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    /// A stream for one case of one run.
    ///
    /// @param seed - the run's seed
    /// @param index - the case's index in the run
    pub fn for_case(seed: u64, index: u64) -> Rng {
        Rng(seed ^ index.wrapping_mul(0x9E37_79B9_7F4A_7C15))
    }

    /// The next 64 bits.
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number below `n`, or 0 when `n` is 0.
    ///
    /// @param n - the bound
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.next() % n as u64) as usize
    }

    /// True with the given chance in a hundred.
    ///
    /// @param percent - the chance
    pub fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    /// One item of a list, which must not be empty.
    ///
    /// @param items - the list
    pub fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        let at = self.below(items.len());
        items.get(at).copied().unwrap_or("")
    }
}

/// The seed for a date written `YYYY-MM-DD`: the date as the number
/// `YYYYMMDD`, so the seed can be read off the history file.
///
/// @param date - the date
pub fn seed_of_date(date: &str) -> u64 {
    date.chars()
        .filter(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap_or(0)
}

/// Today's date in UTC, `YYYY-MM-DD`, from the system clock.
pub fn today() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let (year, month, day) = civil_from_days((seconds / 86_400) as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Converts days since 1970-01-01 to a calendar date (Howard Hinnant's
/// algorithm), so no date crate is needed.
///
/// @param days - days since the epoch
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted + 2) / 5 + 1;
    let month = if shifted < 10 {
        shifted + 3
    } else {
        shifted - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// One table of a random schema.
#[derive(Clone, Debug)]
struct Table {
    /// Its name.
    name: String,
    /// Its columns' names.
    columns: Vec<String>,
    /// Whether its first column is an `INTEGER PRIMARY KEY`, which takes only
    /// integers.
    keyed: bool,
}

/// The declared types a column draws from, including none at all.
const TYPES: &[&str] = &[
    "INTEGER",
    "REAL",
    "TEXT",
    "BLOB",
    "NUMERIC",
    "",
    "VARCHAR(10)",
    "TEXT COLLATE NOCASE",
];

/// Values a row draws from. Every real is a sum of powers of two, so any sum
/// of a few of them is exact.
const VALUES: &[&str] = &[
    "NULL",
    "0",
    "1",
    "2",
    "-3",
    "7",
    "9223372036854775807",
    "-9223372036854775808",
    "0.5",
    "2.25",
    "-1.75",
    "1e3",
    "'a'",
    "'B'",
    "'abc'",
    "''",
    "' 7'",
    "'12'",
    "'1.5'",
    "'x%y'",
    "x'00ff'",
    "x''",
];

/// Scalar functions that answer the same on every run, with their arity.
const FUNCTIONS: &[(&str, usize)] = &[
    ("abs", 1),
    ("coalesce", 2),
    ("ifnull", 2),
    ("nullif", 2),
    ("length", 1),
    ("lower", 1),
    ("upper", 1),
    ("substr", 3),
    ("trim", 1),
    ("typeof", 1),
    ("round", 2),
    ("instr", 2),
    ("replace", 3),
    ("hex", 1),
    ("quote", 1),
    ("iif", 3),
    ("max", 2),
    ("min", 2),
    ("sign", 1),
    ("unicode", 1),
];

/// Writes the setup: two or three tables, their rows, and an index or two.
///
/// @param rng - the case's stream
/// @param setup - where the statements go
fn schema(rng: &mut Rng, setup: &mut Vec<Record>) -> Vec<Table> {
    let count = 2 + rng.below(2);
    let mut tables = Vec::new();
    for number in 0..count {
        let name = format!("t{number}");
        let width = 2 + rng.below(3);
        let columns: Vec<String> = ["a", "b", "c", "d"]
            .iter()
            .take(width)
            .map(|column| (*column).to_string())
            .collect();
        let keyed = rng.chance(30);
        let declared: Vec<String> = columns
            .iter()
            .enumerate()
            .map(|(at, column)| match (at, keyed) {
                (0, true) => format!("{column} INTEGER PRIMARY KEY"),
                _ => format!("{column} {}", rng.pick(TYPES))
                    .trim_end()
                    .to_string(),
            })
            .collect();
        setup.push(Record::ok(format!(
            "CREATE TABLE {name}({})",
            declared.join(", ")
        )));
        let rows = rng.below(8);
        if rows > 0 {
            let values: Vec<String> = (0..rows)
                .map(|row| {
                    let cells: Vec<String> = (0..width)
                        .map(|at| match (at, keyed) {
                            (0, true) => (row + 1).to_string(),
                            _ => rng.pick(VALUES).to_string(),
                        })
                        .collect();
                    format!("({})", cells.join(", "))
                })
                .collect();
            setup.push(Record::ok(format!(
                "INSERT INTO {name} VALUES {}",
                values.join(", ")
            )));
        }
        if rng.chance(40) {
            let column = columns.get(rng.below(width)).cloned().unwrap_or_default();
            setup.push(Record::ok(format!(
                "CREATE INDEX {name}_i ON {name}({column})"
            )));
        }
        tables.push(Table {
            name,
            columns,
            keyed,
        });
    }
    tables
}

/// A random expression over the columns in scope.
///
/// @param rng - the case's stream
/// @param scope - the qualified column names a statement can see
/// @param depth - how many more levels it may nest
fn expr(rng: &mut Rng, scope: &[String], depth: usize) -> String {
    if depth == 0 || rng.chance(30) {
        return leaf(rng, scope);
    }
    let next = depth - 1;
    match rng.below(12) {
        0 => format!(
            "({} {} {})",
            expr(rng, scope, next),
            rng.pick(&["+", "-", "*", "/", "%", "||"]),
            expr(rng, scope, next)
        ),
        1 | 2 => predicate(rng, scope, next),
        3 => format!("(- {})", expr(rng, scope, next)),
        4 => format!(
            "(CASE WHEN {} THEN {} ELSE {} END)",
            predicate(rng, scope, next),
            expr(rng, scope, next),
            expr(rng, scope, next)
        ),
        5 | 6 => call(rng, scope, next),
        7 => format!(
            "CAST({} AS {})",
            expr(rng, scope, next),
            rng.pick(&["INTEGER", "REAL", "TEXT", "NUMERIC", "BLOB"])
        ),
        8 => format!("({} COLLATE NOCASE)", expr(rng, scope, next)),
        _ => leaf(rng, scope),
    }
}

/// A column in scope, or a value.
///
/// @param rng - the case's stream
/// @param scope - the qualified column names
fn leaf(rng: &mut Rng, scope: &[String]) -> String {
    if !scope.is_empty() && rng.chance(65) {
        return scope
            .get(rng.below(scope.len()))
            .cloned()
            .unwrap_or_default();
    }
    rng.pick(VALUES).to_string()
}

/// A function call from [`FUNCTIONS`].
///
/// @param rng - the case's stream
/// @param scope - the qualified column names
/// @param depth - how many more levels it may nest
fn call(rng: &mut Rng, scope: &[String], depth: usize) -> String {
    let (name, arity) = FUNCTIONS
        .get(rng.below(FUNCTIONS.len()))
        .copied()
        .unwrap_or(("abs", 1));
    let arguments: Vec<String> = (0..arity).map(|_| expr(rng, scope, depth)).collect();
    format!("{name}({})", arguments.join(", "))
}

/// A random condition.
///
/// @param rng - the case's stream
/// @param scope - the qualified column names
/// @param depth - how many more levels it may nest
fn predicate(rng: &mut Rng, scope: &[String], depth: usize) -> String {
    let next = depth.saturating_sub(1);
    match rng.below(9) {
        0 | 1 => format!(
            "({} {} {})",
            expr(rng, scope, next),
            rng.pick(&["=", "<>", "<", "<=", ">", ">=", "IS", "IS NOT"]),
            expr(rng, scope, next)
        ),
        2 if depth > 0 => format!(
            "({} {} {})",
            predicate(rng, scope, next),
            rng.pick(&["AND", "OR"]),
            predicate(rng, scope, next)
        ),
        3 => format!("(NOT {})", predicate(rng, scope, next)),
        4 => format!(
            "({} BETWEEN {} AND {})",
            expr(rng, scope, next),
            leaf(rng, scope),
            leaf(rng, scope)
        ),
        5 => format!(
            "({} IN ({}, {}, {}))",
            expr(rng, scope, next),
            rng.pick(VALUES),
            rng.pick(VALUES),
            rng.pick(VALUES)
        ),
        6 => format!(
            "({} {} '{}')",
            expr(rng, scope, next),
            rng.pick(&["LIKE", "GLOB", "NOT LIKE"]),
            rng.pick(&["a%", "%b%", "_", "1*", "x\\%y", "%"])
        ),
        7 => format!(
            "({} IS {}NULL)",
            expr(rng, scope, next),
            rng.pick(&["", "NOT "])
        ),
        _ => format!("({} = {})", leaf(rng, scope), leaf(rng, scope)),
    }
}

/// The qualified columns of some tables.
///
/// @param tables - the tables in the FROM clause
fn columns_of(tables: &[&Table]) -> Vec<String> {
    tables
        .iter()
        .flat_map(|table| {
            table
                .columns
                .iter()
                .map(move |column| format!("{}.{column}", table.name))
        })
        .collect()
}

/// The FROM clause: one table, or two joined, and the columns they bring.
///
/// @param rng - the case's stream
/// @param tables - the schema
fn from_clause<'t>(rng: &mut Rng, tables: &'t [Table]) -> (String, Vec<&'t Table>) {
    let first_at = rng.below(tables.len());
    let Some(first) = tables.get(first_at) else {
        return (String::new(), Vec::new());
    };
    if rng.chance(60) || tables.len() < 2 {
        return (first.name.clone(), vec![first]);
    }
    let second_at = (first_at + 1 + rng.below(tables.len() - 1)) % tables.len();
    let Some(second) = tables.get(second_at) else {
        return (first.name.clone(), vec![first]);
    };
    let join = rng.pick(&["JOIN", "LEFT JOIN", "CROSS JOIN", "JOIN"]);
    let left = columns_of(&[first]);
    let right = columns_of(&[second]);
    let on = format!(
        "{} = {}",
        left.get(rng.below(left.len())).cloned().unwrap_or_default(),
        right
            .get(rng.below(right.len()))
            .cloned()
            .unwrap_or_default()
    );
    let text = match join {
        "CROSS JOIN" => format!("{} CROSS JOIN {}", first.name, second.name),
        _ => format!("{} {join} {} ON {on}", first.name, second.name),
    };
    (text, vec![first, second])
}

/// A subquery condition over another table, correlated or not.
///
/// @param rng - the case's stream
/// @param tables - the schema
/// @param scope - the outer statement's columns
fn subquery(rng: &mut Rng, tables: &[Table], scope: &[String]) -> String {
    let Some(inner) = tables.get(rng.below(tables.len())) else {
        return "1".to_string();
    };
    let alias = inner.name.replace('t', "q");
    let column = inner.columns.first().cloned().unwrap_or_default();
    let outer = leaf(rng, scope);
    match rng.below(3) {
        0 => format!(
            "EXISTS (SELECT 1 FROM {} AS {alias} WHERE {alias}.{column} = {outer})",
            inner.name
        ),
        1 => format!(
            "({outer} IN (SELECT {alias}.{column} FROM {} AS {alias}))",
            inner.name
        ),
        _ => format!(
            "((SELECT max({alias}.{column}) FROM {} AS {alias}) {} {outer})",
            inner.name,
            rng.pick(&["=", "<", ">"])
        ),
    }
}

/// A random SELECT, plain or grouped, sometimes with a subquery condition.
///
/// @param rng - the case's stream
/// @param tables - the schema
fn select(rng: &mut Rng, tables: &[Table]) -> String {
    let (from, joined) = from_clause(rng, tables);
    let scope = columns_of(&joined);
    let mut filter = String::new();
    if rng.chance(70) {
        filter = format!(" WHERE {}", predicate(rng, &scope, 3));
    }
    if rng.chance(15) {
        let joiner = if filter.is_empty() {
            " WHERE "
        } else {
            " AND "
        };
        filter.push_str(joiner);
        filter.push_str(&subquery(rng, tables, &scope));
    }
    if rng.chance(25) {
        return grouped(rng, &from, &scope, &filter);
    }
    let width = 1 + rng.below(3);
    let list: Vec<String> = (1..=width)
        .map(|at| format!("{} AS c{at}", expr(rng, &scope, 3)))
        .collect();
    let distinct = if rng.chance(15) { "DISTINCT " } else { "" };
    let mut sql = format!("SELECT {distinct}{} FROM {from}{filter}", list.join(", "));
    if rng.chance(20) {
        let order: Vec<String> = (1..=width).map(|at| at.to_string()).collect();
        sql.push_str(&format!(
            " ORDER BY {} LIMIT {} OFFSET {}",
            order.join(", "),
            rng.below(4),
            rng.below(3)
        ));
    }
    sql
}

/// A grouped SELECT: one key and a few aggregates, with a HAVING sometimes.
///
/// @param rng - the case's stream
/// @param from - the FROM clause
/// @param scope - its columns
/// @param filter - the WHERE clause, or empty
fn grouped(rng: &mut Rng, from: &str, scope: &[String], filter: &str) -> String {
    let key = expr(rng, scope, 1);
    let argument = leaf(rng, scope);
    let aggregate = rng.pick(&[
        "count(*)",
        "count(DISTINCT {})",
        "sum({})",
        "total({})",
        "min({})",
        "max({})",
        "avg({})",
    ]);
    let aggregate = aggregate.replace("{}", &argument);
    let having = if rng.chance(30) {
        format!(" HAVING count(*) {} 1", rng.pick(&[">", ">=", "="]))
    } else {
        String::new()
    };
    format!("SELECT {key} AS c1, {aggregate} AS c2 FROM {from}{filter} GROUP BY 1{having}")
}

/// A compound of two single table SELECTs of the same width.
///
/// @param rng - the case's stream
/// @param tables - the schema
fn compound(rng: &mut Rng, tables: &[Table]) -> String {
    let arm = |rng: &mut Rng| -> String {
        let table = tables.get(rng.below(tables.len()));
        let scope = table.map(|table| columns_of(&[table])).unwrap_or_default();
        let name = table.map(|table| table.name.clone()).unwrap_or_default();
        format!(
            "SELECT {}, {} FROM {name} WHERE {}",
            expr(rng, &scope, 2),
            expr(rng, &scope, 2),
            predicate(rng, &scope, 2)
        )
    };
    let left = arm(rng);
    let right = arm(rng);
    let op = rng.pick(&["UNION", "UNION ALL", "INTERSECT", "EXCEPT"]);
    format!("{left} {op} {right}")
}

/// A write to one table, and the query that reads the table back.
///
/// @param rng - the case's stream
/// @param tables - the schema
fn write(rng: &mut Rng, tables: &[Table]) -> Vec<Record> {
    let Some(table) = tables.get(rng.below(tables.len())) else {
        return Vec::new();
    };
    let scope = columns_of(&[table]);
    let name = &table.name;
    let column = table.columns.last().cloned().unwrap_or_default();
    // `RETURNING *` makes each write a query, graded engine against engine:
    // a write both engines refuse agrees, where `statement ok` would call it
    // a failure of the case.
    let sql = match rng.below(3) {
        0 => format!(
            "UPDATE {name} SET {column} = {} WHERE {} RETURNING *",
            expr(rng, &scope, 2),
            predicate(rng, &scope, 2)
        ),
        1 => format!(
            "DELETE FROM {name} WHERE {} RETURNING *",
            predicate(rng, &scope, 2)
        ),
        _ => {
            let values: Vec<String> = table
                .columns
                .iter()
                .enumerate()
                .map(|(at, _)| match (at, table.keyed) {
                    (0, true) => (10 + rng.below(90)).to_string(),
                    _ => leaf(rng, &[]),
                })
                .collect();
            let key = table.columns.first().cloned().unwrap_or_default();
            format!(
                "INSERT INTO {name} SELECT {} WHERE NOT EXISTS (SELECT 1 FROM {name} WHERE {key} = {}) RETURNING *",
                values.join(", "),
                values.first().cloned().unwrap_or_default()
            )
        }
    };
    vec![
        Record::query(Sort::RowSort, sql),
        Record::query(Sort::RowSort, format!("SELECT * FROM {name}")),
    ]
}

/// Generates one case.
///
/// @param seed - the run's seed
/// @param index - the case's index in the run
pub fn case(seed: u64, index: u64) -> Case {
    let mut rng = Rng::for_case(seed, index);
    let mut case = Case::new("random", &format!("random seed {seed} case {index}"));
    let tables = schema(&mut rng, &mut case.setup);
    let statements = 2 + rng.below(3);
    for _ in 0..statements {
        match rng.below(10) {
            0 => case.records.extend(write(&mut rng, &tables)),
            1 => case
                .records
                .push(Record::query(Sort::RowSort, compound(&mut rng, &tables))),
            _ => case
                .records
                .push(Record::query(Sort::RowSort, select(&mut rng, &tables))),
        }
    }
    case.assign_id();
    case
}

/// Generates a run's cases.
///
/// @param seed - the run's seed
/// @param count - how many
pub fn generate(seed: u64, count: u64) -> Vec<Case> {
    (0..count).map(|index| case(seed, index)).collect()
}

#[cfg(test)]
mod tests {
    use super::{case, civil_from_days, seed_of_date};

    #[test]
    fn a_seed_and_an_index_name_one_case() {
        assert_eq!(case(20260925, 7), case(20260925, 7));
        assert_ne!(case(20260925, 7).id, case(20260925, 8).id);
        assert_ne!(case(20260925, 7).id, case(20260926, 7).id);
    }

    #[test]
    fn the_date_is_the_seed() {
        assert_eq!(seed_of_date("2026-09-25"), 20_260_925);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_721), (2026, 9, 25));
    }
}

/// What one group of a random run found.
#[derive(Debug, Default)]
pub struct RandomReport {
    /// How many cases ran.
    pub cases: usize,
    /// How many failed and were covered by a rule, deliberate or a recorded bug.
    pub expected: usize,
    /// Every failure no rule covers, rendered with its shrunk case.
    pub problems: Vec<String>,
    /// Whether the pinned SQLite oracle was missing, so nothing was graded.
    pub oracle_missing: bool,
}

/// The most failures one group shrinks. Shrinking reruns a case many times,
/// and a night with hundreds of new failures has one cause worth finding
/// first, not hundreds.
const MOST_SHRUNK: usize = 10;

/// Runs one group's share of a random run at the default arm and judges it.
///
/// A failure is covered when a rule in `deliberate.toml` or
/// `random-known.toml` covers every difference it showed. Each uncovered one
/// is shrunk and written to `_agent_output/matrix/random/<id>.slt`, where a
/// person can read it and save it to the retained corpus.
///
/// @param seed - the run's seed
/// @param count - how many cases the whole run has
/// @param group - this test function's index
/// @param groups - how many test functions share the run
/// @param scratch - the directory scratch files go under
pub fn run_group(
    seed: u64,
    count: u64,
    group: usize,
    groups: usize,
    scratch: &std::path::Path,
) -> Result<RandomReport, String> {
    use crate::statement_matrix::{group as grouping, known, run::Runner};
    let root = known::corpus_root();
    let mut rules = known::read_deliberate(&root.join("deliberate.toml"))?;
    rules.extend(known::read_deliberate(&root.join("random-known.toml"))?);
    let cases: Vec<Case> = generate(seed, count)
        .into_iter()
        .filter(|case| grouping::owns(&case.id, group, groups))
        .collect();
    let arm = grouping::every_arm()
        .into_iter()
        .find(|arm| arm.name == "default")
        .ok_or("no default arm")?;
    // The shard is in the path: every shard process runs every group number,
    // and two processes writing one directory corrupt each other's databases,
    // which the first nightly run showed as a disk I/O error and a missing table.
    let directory = scratch
        .join(format!("s{}", grouping::shard().0))
        .join(format!("random-{group}"));
    let mut runner = Runner::new(arm.clone(), &directory);
    let oracle_missing = !runner.has_oracle();
    let borrowed: Vec<&Case> = cases.iter().collect();
    let verdicts = runner.run_all(&borrowed);
    runner.finish();
    let mut report = RandomReport {
        cases: cases.len(),
        oracle_missing,
        ..RandomReport::default()
    };
    let empty = std::collections::BTreeMap::new();
    for (case, verdict) in cases.iter().zip(verdicts) {
        let (failures, ran) = match verdict {
            crate::statement_matrix::grade::Verdict::Passed => {
                (Vec::new(), vec![arm.name.to_string()])
            }
            crate::statement_matrix::grade::Verdict::Failed(failures) => {
                (failures, vec![arm.name.to_string()])
            }
            crate::statement_matrix::grade::Verdict::Skipped(_) => (Vec::new(), Vec::new()),
        };
        let showed_differences = !failures.is_empty();
        match known::judge(&case.id, failures, &ran, &empty, &rules) {
            known::Judged::Fail(left) => {
                let shrunk = if report.problems.len() < MOST_SHRUNK {
                    let mut shrinker = Runner::new(arm.clone(), &directory.join("shrink"));
                    let found = crate::statement_matrix::shrink::shrink(&mut shrinker, case);
                    shrinker.finish();
                    found.map(|(small, _)| small)
                } else {
                    None
                };
                report
                    .problems
                    .push(describe(seed, case, &left, shrunk.as_ref()));
            }
            // No random case is listed, so a case whose every difference a
            // rule covers is judged a pass; it is counted as covered here.
            known::Judged::Pass if showed_differences => report.expected += 1,
            _ => {}
        }
    }
    let _ = std::fs::remove_dir_all(&directory);
    Ok(report)
}

/// Renders one uncovered failure, and writes its shrunk case where a person
/// can retain it.
///
/// @param seed - the run's seed
/// @param case - the case as generated
/// @param failures - what no rule covered
/// @param shrunk - the smallest case that still fails the same way, when shrinking ran
fn describe(
    seed: u64,
    case: &Case,
    failures: &[crate::statement_matrix::grade::Failure],
    shrunk: Option<&Case>,
) -> String {
    let mut text = format!("RANDOM {} ({}, seed {seed})", case.id, case.origin);
    for failure in failures.iter().take(3) {
        text.push_str(&format!("\n{}", failure.render()));
    }
    if let Some(small) = shrunk {
        let path = crate::workspace_root()
            .join("_agent_output/matrix/random")
            .join(format!("{}.slt", case.id));
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut saved = small.clone();
        saved.id = format!("retained-{}", case.id);
        let _ = std::fs::write(&path, saved.render());
        text.push_str(&format!(
            "\n      shrunk to {}:\n{}",
            path.display(),
            saved.render()
        ));
    }
    text
}

//! An FTS5 index written by the older two-table layout still answers.
//!
//! Invariant: **a file this build did not write reads correctly, and says so by
//! being read rather than by being reasoned about.** task-1911 merged the
//! dictionary row and the doclist row into one: `%_idx`'s third column now
//! carries the doclist inline, where it used to carry an integer naming the
//! `%_data` row the doclist lived in. Old files keep working because the column
//! is self-describing - `term_value` in `crates/inillucent-ext/src/vtab/fts5`
//! reads an `Integer` as the old indirection and a `Blob` as the new inline
//! form - and nothing exercised the `Integer` branch, because every other test
//! in the suite creates a fresh table, which takes the new form on its first
//! write.
//!
//! That is the branch that decides how an existing index is read. Left
//! untested, the failure it would have is not an error: a term whose doclist
//! could not be resolved is a term with no rows, so a search over an index full
//! of documents would quietly answer nothing - the same shape as the vector
//! index defect this ticket exists to close.
//!
//! So the old layout is **manufactured from a file this build wrote**: every
//! doclist is moved out into a `%_data` row and the page number put back in
//! `%_idx`, which is byte for byte what an older build would have left behind,
//! and then the same questions are asked again.

use inillucent_compat::facade::{Connection, Database};
use inillucent_value::Value;

/// Where this suite's scratch databases live.
const AREA: &str = "fts5-legacy";

/// The documents every case indexes.
const DOCUMENTS: [&str; 4] = [
    "the quick brown fox jumps over the lazy dog",
    "a slow green turtle walks past the sleepy cat",
    "quick foxes and quick dogs run quickly",
    "the dog barks and the fox runs away quick",
];

/// Returns a database file of this test's own, under the gitignored root.
///
/// A serial keeps two cases running in parallel from colliding on one file.
fn scratch() -> std::path::PathBuf {
    let root = inillucent_compat::workspace_root()
        .join("_agent_output")
        .join(AREA);
    let _ = std::fs::create_dir_all(&root);
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = root.join(format!("{}-{serial}.rdb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

/// Builds the index, with the current inline layout.
///
/// @param connection - a connection to an empty database
fn build(connection: &Connection) {
    connection
        .execute("CREATE VIRTUAL TABLE docs USING fts5(body)")
        .expect("creates the index");
    for document in DOCUMENTS {
        let mut statement = connection
            .prepare("INSERT INTO docs (body) VALUES (?1)")
            .expect("prepares");
        statement.bind_text(1, document).expect("binds");
        while statement.step().expect("steps") {}
    }
}

/// Returns the rowids a term matches, ascending.
///
/// @param connection - a connection to the database holding `docs`
/// @param term - the term to search for
fn matching(connection: &Connection, term: &str) -> Vec<i64> {
    let mut statement = connection
        .prepare("SELECT rowid FROM docs WHERE docs MATCH ?1 ORDER BY rowid")
        .expect("prepares the search");
    statement.bind_text(1, term).expect("binds");
    let mut found = Vec::new();
    while statement.step().expect("steps") {
        if let Some(rowid) = statement.value_integer(0) {
            found.push(rowid);
        }
    }
    found.sort_unstable();
    found
}

/// Rewrites every `%_idx` row into the layout an older build wrote.
///
/// **The doclist bytes are not changed, only where they live.** Each term's
/// inline doclist is written into a `%_data` row of its own and the `%_idx`
/// column is replaced with that row's integer id, which is exactly the pair of
/// rows the older build maintained. Anything else would be testing a file this
/// project never produced.
///
/// The `%_data` ids start above anything the module allocates for itself, so
/// this cannot collide with a row the index already holds.
///
/// @param connection - a connection to the database holding `docs`
/// @returns how many terms were moved
fn make_it_look_old(connection: &Connection) -> usize {
    let rows = connection
        .query("SELECT segid, term, doclist FROM docs_idx")
        .expect("the dictionary reads");
    let mut moved = 0usize;
    for (nth, row) in rows.iter().enumerate() {
        let (Some(segid), Some(term), Some(doclist)) = (row.first(), row.get(1), row.get(2)) else {
            continue;
        };
        let Some(bytes) = doclist.as_blob().map(|blob| blob.raw().to_vec()) else {
            // Already a page number, which would mean the build under test did
            // not write the layout this test is converting from.
            continue;
        };
        let page = 900_000i64 + nth as i64;

        let mut into_data = connection
            .prepare("INSERT INTO docs_data (id, block) VALUES (?1, ?2)")
            .expect("prepares the %_data write");
        into_data.bind_integer(1, page).expect("binds the page");
        into_data.bind_blob(2, &bytes).expect("binds the doclist");
        while into_data.step().expect("steps") {}
        drop(into_data);

        let mut point_at_it = connection
            .prepare("UPDATE docs_idx SET doclist = ?1 WHERE segid = ?2 AND term = ?3")
            .expect("prepares the %_idx rewrite");
        point_at_it.bind_integer(1, page).expect("binds the page");
        bind_value(&mut point_at_it, 2, segid);
        bind_value(&mut point_at_it, 3, term);
        while point_at_it.step().expect("steps") {}
        moved = moved.saturating_add(1);
    }
    moved
}

/// Returns how many `%_idx` rows now name a `%_data` page rather than carrying
/// their doclist inline.
///
/// **The check that stops this suite proving nothing.** Everything below rests
/// on `make_it_look_old` having actually rewritten the dictionary, and a
/// shadow-table `UPDATE` that quietly changed no rows would leave every row
/// inline - so the searches would pass by never reaching the branch they exist
/// to exercise, and the suite would report green over an untested read path.
/// That is the failure mode the testing standard calls worse than no test, so
/// it is asserted rather than assumed.
///
/// @param connection - a connection to the database holding `docs`
fn rows_naming_a_page(connection: &Connection) -> usize {
    connection
        .query("SELECT doclist FROM docs_idx")
        .expect("the dictionary reads")
        .iter()
        .filter(|row| matches!(row.first(), Some(Value::Integer(_))))
        .count()
}

/// Binds one already-read value back into a statement, whatever type it is.
///
/// `%_idx`'s key columns are untyped, so a case that assumed text would fail on
/// a segid held as an integer and say nothing useful about why.
///
/// @param statement - the statement being bound
/// @param index - which parameter
/// @param value - the value read out of the row
fn bind_value(
    statement: &mut inillucent_compat::facade::Statement<'_>,
    index: u32,
    value: &Value<'static>,
) {
    let outcome = match value {
        Value::Integer(number) => statement.bind_integer(index, *number),
        Value::Real(number) => statement.bind_real(index, *number),
        Value::Text(text) => statement.bind_text(index, &String::from_utf8_lossy(text.raw())),
        Value::Blob(blob) => statement.bind_blob(index, blob.raw()),
        Value::Null => statement.bind_null(index),
    };
    outcome.expect("binds a key column");
}

/// An index in the older two-table layout answers the same rows.
#[test]
fn an_index_in_the_old_layout_still_answers() {
    let path = scratch();
    let expected;
    {
        let database = Database::open(&path).expect("opens");
        let connection = database.connect().expect("connects");
        build(&connection);
        expected = matching(&connection, "quick");
        assert_eq!(
            expected,
            vec![1, 3, 4],
            "the term is in three of the four documents before anything is converted"
        );
        let moved = make_it_look_old(&connection);
        assert!(
            moved > 0,
            "no dictionary row carried an inline doclist, so nothing was converted and this test \
             would have proved nothing"
        );
        assert_eq!(
            rows_naming_a_page(&connection),
            moved,
            "every converted row now names a %_data page, so the searches below really do go \
             through the old layout's indirection"
        );
    }

    let database = Database::open(&path).expect("reopens");
    let connection = database.connect().expect("connects");
    assert_eq!(
        matching(&connection, "quick"),
        expected,
        "the old layout answers the rows it holds, rather than answering none"
    );
    assert_eq!(
        matching(&connection, "turtle"),
        vec![2],
        "a term in one document is still found in the old layout"
    );
    assert!(
        matching(&connection, "kangaroo").is_empty(),
        "a term in no document is still found in none of them"
    );
}

/// Writing to an index in the old layout keeps the terms it already held.
///
/// This is the half that a conversion gets wrong quietly: a term whose row is
/// rewritten inline must keep the documents it already named, and the terms
/// nothing touched must still resolve through the old indirection. An index
/// where half the dictionary has converted and half has not is the state every
/// existing file passes through on its first write.
#[test]
fn a_write_to_an_old_layout_index_keeps_what_it_held() {
    let path = scratch();
    {
        let database = Database::open(&path).expect("opens");
        let connection = database.connect().expect("connects");
        build(&connection);
        let moved = make_it_look_old(&connection);
        assert!(moved > 0, "something was converted");
        assert_eq!(
            rows_naming_a_page(&connection),
            moved,
            "the dictionary really is in the old layout before anything writes to it"
        );
    }

    {
        let database = Database::open(&path).expect("reopens");
        let connection = database.connect().expect("connects");
        let mut statement = connection
            .prepare("INSERT INTO docs (body) VALUES (?1)")
            .expect("prepares");
        statement
            .bind_text(1, "a quick kangaroo among the sleepy dogs")
            .expect("binds");
        while statement.step().expect("steps") {}
    }

    let database = Database::open(&path).expect("reopens again");
    let connection = database.connect().expect("connects");
    assert_eq!(
        matching(&connection, "quick"),
        vec![1, 3, 4, 5],
        "the three documents the old layout held and the one just written are all found"
    );
    assert_eq!(
        matching(&connection, "kangaroo"),
        vec![5],
        "a term that did not exist before the write is found"
    );
    assert_eq!(
        matching(&connection, "turtle"),
        vec![2],
        "a term nothing touched still resolves through the old indirection"
    );
}

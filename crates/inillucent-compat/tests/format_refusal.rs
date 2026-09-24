//! An index this build cannot read is refused by name, not answered with zero
//! rows.
//!
//! Invariant: **the only thing worse than a refusal is an empty result set
//! nobody asked for.** A search that returns nothing is a legitimate answer, so
//! an index the reader does not understand is the one failure an application
//! cannot tell from a correct answer - and it fails at the point where somebody
//! is deciding whether the documents exist.
//!
//! That is not hypothetical here. task-2036's interop suite runs every
//! published release against a file the current build wrote, and 0.1.1 answers
//! `SELECT count(*) FROM note_fts` as five, `SELECT rowid, title FROM note_fts`
//! as all five rows, and `WHERE note_fts MATCH 'segment'` as **no rows at
//! all** - because the FTS5 index layout changed in 0.1.2 and nothing in the
//! index said which layout wrote it. 0.1.1 is published and its answer can
//! never be fixed. task-2053 fixed the next one: an index now records the
//! layout it is in and the release that wrote it, and a reader that meets a
//! layout it has not got refuses with the status `unsupported`, naming both.
//!
//! ## What each case manufactures, and why by hand
//!
//! There is no build that writes a later layout, because this build is the
//! latest one there is. So the record is written by hand, in the bytes
//! `crates/inillucent-ext/src/vtab/fts5/layout.rs` writes with a different
//! number in it - which is also what makes this a check rather than a round
//! trip: a change to that encoding leaves these bytes unreadable as a record,
//! the refusal does not happen, and the case fails.

use inillucent_base::DbError;
use inillucent_compat::cliproc::{program, run, text_field};
use inillucent_compat::facade::{Connection, Database, Statement};
use inillucent_value::Value;

/// Where this suite's scratch databases live.
const AREA: &str = "format-refusal";

/// The `%_data` row the FTS5 layout record lives in.
///
/// Written out here rather than read from the module, because the point of the
/// case is that the record is at a fixed place in the file: a build that moved
/// it would be a build an older one cannot find it in.
const LAYOUT_ROW: i64 = 2;

/// The eight bytes an FTS5 layout record begins with.
const LAYOUT_MAGIC: &[u8; 8] = b"RDBFTS5\0";

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

/// Builds a full-text index over [`DOCUMENTS`].
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

/// Returns the bytes of a layout record naming a build that does not exist.
///
/// The same eight byte magic, a little-endian layout number and a
/// length-prefixed release string - which is exactly what `layout.rs::encode`
/// produces, with a number this build cannot have written.
///
/// @param layout - the layout to claim the index is in
/// @param writer - the release to claim wrote it
fn record_from_the_future(layout: u32, writer: &str) -> Vec<u8> {
    let mut bytes = Vec::from(*LAYOUT_MAGIC);
    bytes.extend_from_slice(&layout.to_le_bytes());
    bytes.extend_from_slice(&(writer.len() as u16).to_le_bytes());
    bytes.extend_from_slice(writer.as_bytes());
    bytes
}

/// Replaces the index's layout record with one naming a later build.
///
/// @param connection - a connection to the database holding `docs`
/// @param layout - the layout to claim the index is in
/// @param writer - the release to claim wrote it
fn claim_a_later_layout(connection: &Connection, layout: u32, writer: &str) {
    let mut statement = connection
        .prepare("UPDATE docs_data SET block = ?1 WHERE id = ?2")
        .expect("prepares the layout rewrite");
    statement
        .bind_blob(1, &record_from_the_future(layout, writer))
        .expect("binds the record");
    statement
        .bind_integer(2, LAYOUT_ROW)
        .expect("binds the row");
    while statement.step().expect("steps") {}
    drop(statement);
    assert_eq!(
        layout_record(connection).as_deref(),
        Some(record_from_the_future(layout, writer).as_slice()),
        "the rewrite changed no rows, so every case below would pass by never reaching the \
         refusal it exists to check"
    );
}

/// Returns the bytes the layout record holds, when the index has one.
///
/// @param connection - a connection to the database holding `docs`
fn layout_record(connection: &Connection) -> Option<Vec<u8>> {
    let rows = connection
        .query(&format!(
            "SELECT block FROM docs_data WHERE id = {LAYOUT_ROW}"
        ))
        .expect("the index's own rows read");
    rows.first()
        .and_then(|row| row.first())
        .and_then(Value::as_blob)
        .map(|blob| blob.raw().to_vec())
}

/// Returns the rowids a term matches, or the refusal the search reported.
///
/// @param connection - a connection to the database holding `docs`
/// @param term - the term to search for
fn matching(connection: &Connection, term: &str) -> Result<Vec<i64>, DbError> {
    matching_in(connection, "docs", term)
}

/// Returns the rowids a term matches in a named table, or the refusal.
///
/// @param connection - a connection to the database
/// @param table - the full-text table to search
/// @param term - the term to search for
fn matching_in(connection: &Connection, table: &str, term: &str) -> Result<Vec<i64>, DbError> {
    let mut statement =
        connection.prepare(&format!("SELECT rowid FROM {table} WHERE {table} MATCH ?1"))?;
    statement.bind_text(1, term)?;
    let mut found = Vec::new();
    while statement.step()? {
        if let Some(rowid) = statement.value_integer(0) {
            found.push(rowid);
        }
    }
    found.sort_unstable();
    Ok(found)
}

/// Asserts that a refusal is the layout one, and says what it names.
///
/// @param refused - the error the engine reported
/// @param layout - the layout the record claims
/// @param writer - the release the record claims
fn a_layout_refusal(refused: &DbError, layout: u32, writer: &str) {
    assert_eq!(
        refused.unsupported(),
        Some(format!("a full-text index in layout {layout}").as_str()),
        "the refusal carries the status `unsupported`, which is how a caller tells `upgrade \
         inillucent` from `your query is wrong` without reading the sentence: {refused:?}"
    );
    let message = refused.message();
    assert!(
        message.contains("docs"),
        "the refusal names the table: {message}"
    );
    assert!(
        message.contains(&format!("layout {layout}")),
        "the refusal names the layout it found: {message}"
    );
    assert!(
        message.contains(writer),
        "the refusal names the release that wrote it, so a reader knows what to install: \
         {message}"
    );
}

/// An index this build creates records which layout it is in and who wrote it.
///
/// **The whole of what a later build has to go on.** Every case below rests on
/// the record being there at all, and a file written before task-2053 has none;
/// this is the one that says a file written after it does.
#[test]
fn an_index_this_build_creates_records_its_layout() {
    let path = scratch();
    let database = Database::open(&path).expect("opens");
    let connection = database.session().expect("connects");
    build(&connection);

    let record = layout_record(&connection).expect("the index carries a layout record");
    assert_eq!(
        record.get(..LAYOUT_MAGIC.len()),
        Some(LAYOUT_MAGIC.as_slice()),
        "the record starts with the magic, so a doclist an older build writes over it reads as \
         no record rather than as a layout number"
    );
    let version = env!("CARGO_PKG_VERSION");
    assert!(
        record.ends_with(version.as_bytes()),
        "the record names the release that wrote it, which is {version}"
    );
}

/// A search against an index in a later layout refuses instead of answering
/// nothing.
///
/// **This is the ticket.** Both halves are asserted: that the same query
/// answers three documents before the record is changed, and that it refuses
/// afterwards - because a case that only checked the refusal would pass against
/// an engine that refused every search.
#[test]
fn a_search_over_a_later_layout_refuses_rather_than_answering_no_rows() {
    let path = scratch();
    {
        let database = Database::open(&path).expect("opens");
        let connection = database.session().expect("connects");
        build(&connection);
        assert_eq!(
            matching(&connection, "quick").expect("the search answers"),
            vec![1, 3, 4],
            "the term is in three of the four documents while the index is this build's own"
        );
        claim_a_later_layout(&connection, 99, "9.9.9");
    }

    let database = Database::open(&path).expect("reopens");
    let connection = database.session().expect("connects");
    let refused = matching(&connection, "quick")
        .expect_err("a layout this build has not got is refused, not answered with no rows");
    a_layout_refusal(&refused, 99, "9.9.9");
}

/// The rows themselves still read, which is why the silence was so hard to see.
///
/// 0.1.1 answered `count(*)` over a newer index correctly and answered the
/// `MATCH` with nothing, and that combination is what made the break invisible:
/// the table plainly had documents in it. The refusal is on the one query whose
/// answer depends on the dictionary, and this case is what says the other
/// queries were not made to refuse along with it.
#[test]
fn a_later_layout_does_not_stop_the_documents_being_read() {
    let path = scratch();
    {
        let database = Database::open(&path).expect("opens");
        let connection = database.session().expect("connects");
        build(&connection);
        claim_a_later_layout(&connection, 99, "9.9.9");
    }

    let database = Database::open(&path).expect("reopens");
    let connection = database.session().expect("connects");
    let counted = connection
        .query("SELECT count(*) FROM docs")
        .expect("counting the rows still works");
    assert_eq!(
        counted
            .first()
            .and_then(|row| row.first())
            .and_then(Value::as_integer),
        Some(DOCUMENTS.len() as i64),
        "the documents are stored in `%_content`, which every layout holds the same way"
    );
}

/// A write to an index in a later layout refuses too.
///
/// **A refusal to read that still allowed a write would be worse than no
/// refusal at all.** An append writes this build's rows into a dictionary whose
/// other rows it does not understand, which leaves an index half in each layout
/// and readable by neither.
#[test]
fn a_write_to_a_later_layout_refuses() {
    let path = scratch();
    {
        let database = Database::open(&path).expect("opens");
        let connection = database.session().expect("connects");
        build(&connection);
        claim_a_later_layout(&connection, 7, "0.9.0");
    }

    let database = Database::open(&path).expect("reopens");
    let connection = database.session().expect("connects");
    let mut statement = connection
        .prepare("INSERT INTO docs (body) VALUES ('a document this build must not write')")
        .expect("prepares");
    let refused = statement
        .step()
        .expect_err("a write into a layout this build has not got is refused");
    a_layout_refusal(&refused, 7, "0.9.0");
}

/// One unreadable index does not stop the other indexes in the same database
/// being written.
///
/// **The refusal is about one index, so it has to be raised where one index is
/// written.** The engine tells *every* connected module that a write
/// transaction has started, on the first write to any one of them - so a check
/// in `begin` refuses a write to a perfectly readable table because some other
/// full-text table in the same database was written by a newer build. A
/// database with one index an application cannot use is still a database whose
/// other indexes it can use.
#[test]
fn an_unreadable_index_does_not_refuse_a_write_to_its_neighbour() {
    let path = scratch();
    {
        let database = Database::open(&path).expect("opens");
        let connection = database.session().expect("connects");
        build(&connection);
        connection
            .execute("CREATE VIRTUAL TABLE others USING fts5(body)")
            .expect("creates a second index");
        connection
            .execute("INSERT INTO others (body) VALUES ('a document in the other index')")
            .expect("writes to it");
        claim_a_later_layout(&connection, 99, "9.9.9");
    }

    let database = Database::open(&path).expect("reopens");
    let connection = database.session().expect("connects");
    connection
        .execute("INSERT INTO others (body) VALUES ('another document in the other index')")
        .expect("the readable index still takes a write while its neighbour is refused");
    assert_eq!(
        matching_in(&connection, "others", "document").expect("the readable index answers"),
        vec![1, 2],
        "both documents are in the index nothing was wrong with"
    );
    assert!(
        matching(&connection, "quick").is_err(),
        "and the unreadable one is still refused, so this case is not passing because the check \
         stopped happening"
    );
}

/// The vocabulary of an index in a later layout refuses rather than reporting
/// an empty one.
///
/// `fts5vocab` reads the dictionary and nothing else, so an index it cannot
/// read has nothing it can honestly report - and an empty vocabulary over an
/// index full of terms is the same silence in a different table.
#[test]
fn a_vocabulary_of_a_later_layout_refuses() {
    let path = scratch();
    {
        let database = Database::open(&path).expect("opens");
        let connection = database.session().expect("connects");
        build(&connection);
        connection
            .execute("CREATE VIRTUAL TABLE terms USING fts5vocab(docs, 'row')")
            .expect("creates the vocabulary");
        let before = connection
            .query("SELECT count(*) FROM terms")
            .expect("the vocabulary reads");
        assert!(
            matches!(before.first().and_then(|row| row.first()), Some(Value::Integer(count)) if *count > 0),
            "the vocabulary holds terms before the record is changed: {before:?}"
        );
        claim_a_later_layout(&connection, 99, "9.9.9");
    }

    let database = Database::open(&path).expect("reopens");
    let connection = database.session().expect("connects");
    let refused = connection
        .query("SELECT count(*) FROM terms")
        .expect_err("the vocabulary of an unreadable index is refused");
    assert_eq!(
        refused.unsupported(),
        Some("a full-text index in layout 99"),
        "{refused:?}"
    );
}

/// An index with no layout record at all still answers.
///
/// **Every file published before task-2053 has no record**, and a reader that
/// refused them would refuse every database in existence. A missing record
/// means "some layout up to and including this build's", which is read by the
/// per-row rule `fts5_legacy_layout.rs` exercises - not refused.
#[test]
fn an_index_with_no_record_is_read_rather_than_refused() {
    let path = scratch();
    {
        let database = Database::open(&path).expect("opens");
        let connection = database.session().expect("connects");
        build(&connection);
        connection
            .execute(&format!("DELETE FROM docs_data WHERE id = {LAYOUT_ROW}"))
            .expect("removes the record, leaving the file an older build would have written");
        assert_eq!(
            layout_record(&connection),
            None,
            "the record really is gone, so the case below is about a file without one"
        );
    }

    let database = Database::open(&path).expect("reopens");
    let connection = database.session().expect("connects");
    assert_eq!(
        matching(&connection, "quick").expect("an index with no record still answers"),
        vec![1, 3, 4],
        "a file written before the record existed reads exactly as it did"
    );
}

/// A rebuild puts the record back, and is how a file that predates it gets one.
///
/// `rebuild` reads every row out of `%_content` and writes the dictionary again
/// from nothing, so after it the claim the record makes is true of every row -
/// which is exactly the claim an ordinary insert cannot make, and why an
/// ordinary insert does not stamp one.
#[test]
fn a_rebuild_stamps_the_record_onto_a_file_that_had_none() {
    let path = scratch();
    {
        let database = Database::open(&path).expect("opens");
        let connection = database.session().expect("connects");
        build(&connection);
        connection
            .execute(&format!("DELETE FROM docs_data WHERE id = {LAYOUT_ROW}"))
            .expect("removes the record");
    }

    {
        let database = Database::open(&path).expect("reopens");
        let connection = database.session().expect("connects");
        connection
            .execute("INSERT INTO docs (docs) VALUES ('rebuild')")
            .expect("rebuilds the index");
        assert!(
            layout_record(&connection).is_some(),
            "a rebuild writes the whole index in this build's layout, so it may say so"
        );
    }

    let database = Database::open(&path).expect("reopens again");
    let connection = database.session().expect("connects");
    assert_eq!(
        matching(&connection, "quick").expect("the rebuilt index answers"),
        vec![1, 3, 4],
        "the rebuild kept the documents it was derived from"
    );
}

/// A dictionary row whose doclist cannot be found refuses rather than matching
/// nothing.
///
/// **The same silence, inside this build.** A `%_idx` row naming a `%_data` row
/// that is not there resolved to "no doclist", and every reader but
/// `integrity-check` read that as "this term is in no documents" - so a search
/// over an index full of documents answered nothing, with no error anywhere.
/// One path was worse: `term_row` staged the missing doclist as an empty one
/// and the flush wrote it back, so a write to the table destroyed the postings
/// it could not read.
#[test]
fn a_dictionary_row_pointing_at_nothing_refuses() {
    let path = scratch();
    {
        let database = Database::open(&path).expect("opens");
        let connection = database.session().expect("connects");
        build(&connection);
        point_every_term_at_a_missing_page(&connection);
    }

    let database = Database::open(&path).expect("reopens");
    let connection = database.session().expect("connects");
    let refused = matching(&connection, "quick")
        .expect_err("a doclist that cannot be read is refused, not answered with no rows");
    let message = refused.message();
    assert!(
        message.contains("doclist"),
        "the refusal says what could not be read: {message}"
    );
}

/// Points every dictionary row at a `%_data` row that does not exist.
///
/// The older two-table layout put an integer there - the `%_data` row the
/// doclist lived in - so this is a well formed row of that layout naming a page
/// nothing wrote. `fts5_legacy_layout.rs` builds the same shape with the page
/// present, which is the case that must keep working.
///
/// @param connection - a connection to the database holding `docs`
fn point_every_term_at_a_missing_page(connection: &Connection) {
    let rows = connection
        .query("SELECT segid, term FROM docs_idx")
        .expect("the dictionary reads");
    let mut moved = 0usize;
    for row in &rows {
        let (Some(segid), Some(term)) = (row.first(), row.get(1)) else {
            continue;
        };
        let mut statement = connection
            .prepare("UPDATE docs_idx SET doclist = 900000 WHERE segid = ?1 AND term = ?2")
            .expect("prepares the dictionary rewrite");
        bind_value(&mut statement, 1, segid);
        bind_value(&mut statement, 2, term);
        while statement.step().expect("steps") {}
        moved = moved.saturating_add(1);
    }
    assert!(
        moved > 0,
        "the dictionary held no rows, so nothing was pointed anywhere and this case would have \
         proved nothing"
    );
}

/// Binds one already-read value back into a statement, whatever type it is.
///
/// `%_idx`'s key columns are untyped, so a case that assumed text would fail on
/// a segid held as an integer and say nothing useful about why.
///
/// @param statement - the statement being bound
/// @param index - which parameter
/// @param value - the value read out of the row
fn bind_value(statement: &mut Statement<'_>, index: u32, value: &Value<'static>) {
    let outcome = match value {
        Value::Integer(number) => statement.bind_integer(index, *number),
        Value::Real(number) => statement.bind_real(index, *number),
        Value::Text(text) => statement.bind_text(index, &String::from_utf8_lossy(text.raw())),
        Value::Blob(blob) => statement.bind_blob(index, blob.raw()),
        Value::Null => statement.bind_null(index),
    };
    outcome.expect("binds a key column");
}

/// A search index whose `%_config` names a later format refuses by name.
///
/// The retrieval half's own answer to the same question. `inillucent_search`
/// has recorded a format number since it was written and refused a mismatch -
/// but as a plain statement error with no status, so a driver reported it the
/// way it reports a syntax mistake, and the message named no release to go and
/// install. task-2053 made it `unsupported` and gave it the release.
///
/// **The number here is 3 rather than 2** (task-2067). A search table that
/// declares a facet column is written in format 2 and this build reads it, so 2
/// stopped being a format from the future the day facets landed. The two
/// numbers and why there are two are in `options.rs`.
#[test]
fn a_search_index_in_a_later_format_refuses_as_unsupported() {
    let path = scratch();
    {
        let database = Database::open(&path).expect("opens");
        let connection = database.session().expect("connects");
        connection
            .execute("CREATE VIRTUAL TABLE corpus USING inillucent_search(content, dims = 8, mode = 'exact')")
            .expect("creates the search index");
        connection
            .execute("INSERT INTO corpus (rowid, content) VALUES (1, 'the ledger holds a segment')")
            .expect("indexes a document");
        connection
            .execute("UPDATE corpus_config SET v = '3' WHERE k = 'format'")
            .expect("claims a later format");
        connection
            .execute("UPDATE corpus_config SET v = '9.9.9' WHERE k = 'writer'")
            .expect("claims a later release wrote it");
    }

    let database = Database::open(&path).expect("reopens");
    let connection = database.session().expect("connects");
    let refused = connection
        .query("SELECT rowid FROM corpus")
        .expect_err("a format this build has not got is refused");
    assert_eq!(
        refused.unsupported(),
        Some("a search index in format 3"),
        "the refusal carries the status, so the command line exits 3: {refused:?}"
    );
    let message = refused.message();
    assert!(
        message.contains("9.9.9"),
        "the refusal names the release to install: {message}"
    );
}

/// The command line exits 3 and reports `unsupported` for a later layout.
///
/// **Exit 3 is the contract AGENTS.md states**: "this engine has not built
/// that", a different code from 1 on purpose, so a script can branch on it
/// without matching on a message. The in-process cases above prove the engine
/// refuses; this proves the refusal survives the whole way out to a caller who
/// only has an exit code and a JSON object - which is what an application
/// upgrading one machine and not another actually has.
#[test]
fn the_command_line_exits_three_for_a_later_layout() {
    let binary = program("inillucent");
    let path = scratch();
    {
        let database = Database::open(&path).expect("opens");
        let connection = database.session().expect("connects");
        build(&connection);
        claim_a_later_layout(&connection, 99, "9.9.9");
    }

    let ran = run(
        &binary,
        &[
            "--db",
            &path.to_string_lossy(),
            "query",
            "SELECT rowid FROM docs WHERE docs MATCH 'quick'",
            "--output",
            "json",
        ],
    );
    assert_eq!(
        ran.code,
        3,
        "a layout this build has not got exits 3, not 0 with an empty result set: {}",
        ran.said()
    );
    assert_eq!(
        text_field(&ran.stdout, "status"),
        "unsupported",
        "{}",
        ran.said()
    );
    let message = text_field(&ran.stdout, "message");
    assert!(
        message.contains("layout 99") && message.contains("9.9.9"),
        "the refusal names the layout and the release: {message}"
    );
}

/// A search index this build creates records which release wrote it.
#[test]
fn a_search_index_this_build_creates_records_the_release() {
    let path = scratch();
    let database = Database::open(&path).expect("opens");
    let connection = database.session().expect("connects");
    connection
        .execute(
            "CREATE VIRTUAL TABLE corpus USING inillucent_search(content, dims = 8, mode = 'exact')",
        )
        .expect("creates the search index");
    let rows = connection
        .query("SELECT v FROM corpus_config WHERE k = 'writer'")
        .expect("the configuration reads");
    assert_eq!(
        rows.first()
            .and_then(|row| row.first())
            .and_then(Value::as_text)
            .map(|text| String::from_utf8_lossy(text.raw()).into_owned()),
        Some(env!("CARGO_PKG_VERSION").to_string()),
        "the configuration names the release, so the refusal above has one to name"
    );
}

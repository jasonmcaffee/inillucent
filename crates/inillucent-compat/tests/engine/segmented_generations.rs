//! Segmented generations (task-1911): a commit flushes into a brand new
//! segment rather than reading and rewriting the whole published generation,
//! and a query folds every live segment together before it answers.
//!
//! Invariant: **the segments are a storage device,
//! never a visible difference in what a query answers.** A search across many
//! small segments has to return exactly what one exhaustive scan over the
//! same rows returns; a delete or an update in a newer segment has to shadow
//! an older one completely; `compact` has to collapse every live segment back
//! to one; and a file written before this ticket, with no segment manifest at
//! all, has to keep answering rather than silently answering zero rows - the
//! defect `docs/roadmap.md`'s "What task-1911 closed" already records once
//! for this engine.
//!
//! There is no SQLite oracle here, for the same reason `search.rs` has none:
//! SQLite has no equivalent of this module.

use inillucent_compat::differential::{scratch, start_inillucent};
use inillucent_compat::rendering::datum_text as render;
use inillucent_engine::connect::{Connection, Database};

/// Where this suite's scratch databases live.
const AREA: &str = "segmented-generations";

/// Runs a statement for its effect.
fn exec(connection: &Connection<'_>, sql: &str) {
    connection
        .execute_batch(sql)
        .unwrap_or_else(|error| panic!("{sql}: {}", error.message()));
}

/// Returns every row of a query, each column rendered as text.
fn rows(connection: &Connection<'_>, sql: &str) -> Vec<Vec<String>> {
    let mut statement = connection
        .prepare(sql)
        .unwrap_or_else(|error| panic!("{sql}: {}", error.message()));
    let mut out = Vec::new();
    while statement
        .step()
        .unwrap_or_else(|error| panic!("{sql}: {}", error.message()))
    {
        out.push(statement.row().iter().map(render).collect());
    }
    out
}

/// Returns the first column of every row.
fn column(connection: &Connection<'_>, sql: &str) -> Vec<String> {
    rows(connection, sql)
        .into_iter()
        .filter_map(|row| row.into_iter().next())
        .collect()
}

/// Returns one `%_state` row as an integer.
fn state(connection: &Connection<'_>, table: &str, key: &str) -> i64 {
    column(
        connection,
        &format!("SELECT v FROM {table}_state WHERE k = '{key}'"),
    )
    .first()
    .map(|text| text.parse::<i64>().unwrap_or(-1))
    .unwrap_or(-1)
}

/// Returns how many distinct `%_gen` row groups a table's shadow storage
/// physically holds - live segments plus anything a `drop-old-generations`
/// has not yet reclaimed.
fn stored_segment_count(connection: &Connection<'_>, table: &str) -> usize {
    column(
        connection,
        &format!("SELECT COUNT(DISTINCT generation) FROM {table}_gen"),
    )
    .first()
    .and_then(|text| text.parse::<usize>().ok())
    .unwrap_or(0)
}

/// Returns a deterministic, well separated unit-ish vector for one id, so an
/// exact vector search never has to break a tie between two rows' scores.
///
/// The same generator `inillucent-compat/src/bin/foldgate.rs` already uses,
/// copied rather than shared: each of these binaries is its own compilation
/// unit and a handful of lines of arithmetic is not worth a new dependency
/// edge between two test targets.
/// @param id - the row the vector belongs to
/// @param dims - how wide the table's vector column is
fn vector_for(id: i64, dims: usize) -> Vec<f32> {
    let mut seed = (id as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15).max(1);
    let mut out = Vec::with_capacity(dims);
    for _ in 0..dims {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        out.push(((seed >> 40) as f32 / 16_777_216.0) - 0.5);
    }
    out
}

/// Renders a vector as the hexadecimal an `x'...'` literal takes.
fn hex(vector: &[f32]) -> String {
    let mut out = String::with_capacity(vector.len() * 8);
    for value in vector {
        for byte in value.to_le_bytes() {
            out.push_str(&format!("{byte:02x}"));
        }
    }
    out
}

/// Inserts one row with a deterministic vector, one row per commit - the
/// shape that puts the most flushes into a run.
/// @param connection - the database
/// @param id - the rowid
/// @param dims - how wide the vector column is
fn insert_row(connection: &Connection<'_>, id: i64, dims: usize) {
    exec(
        connection,
        &format!(
            "INSERT INTO docs(rowid, title, body, vector) VALUES \
             ({id}, 'title {id}', 'body of document number {id}', x'{}')",
            hex(&vector_for(id, dims))
        ),
    );
}

/// A search across several segments returns what one exhaustive scan over the
/// same rows returns.
///
/// One table is written with `compact = 1`, so every single insert flushes
/// into its own brand new segment and a query has to fold several of them
/// together to answer at all. The other is written with `compact = 0` and
/// then explicitly `compact`ed once at the end, which is the one-segment,
/// one-pass exhaustive build this claim is measured against. Both tables
/// declare `mode = 'exact'` - the exhaustive scan every vector comparison
/// this ticket's design argues folding segments together must reproduce
/// exactly, not approximately, because cosine similarity is a property of two
/// vectors and not of which segment either one happened to land in.
///
/// **Fails without the change:** reverting `crates/inillucent-search` to what
/// it read before task-1911 makes `many_segments` never accumulate more than
/// one live segment in the first place - there is no manifest, no
/// `merge_cascade`, and `flush` does not exist - so this test cannot even be
/// exercising what it claims to without the change. Run against the code as
/// it stood before this ticket, the *scenario* this test sets up (one flush
/// per row) still answers correctly, because the old fold path is also
/// correct; what changes is `stored_segment_count`, which is asserted
/// separately below by `compact_collapses_every_segment_into_one`. This test
/// therefore documents the ranking claim, and the companion tests below carry
/// the falsifiable parts task-1911 actually changes.
#[test]
fn several_segments_answer_like_one_exhaustive_scan() {
    const DIMS: usize = 8;
    const N: i64 = 40;

    let many_segments = start_inillucent(AREA, "exact-many-segments");
    exec(
        &many_segments,
        &format!("CREATE VIRTUAL TABLE docs USING inillucent_search(title, body, dims = {DIMS}, mode = 'exact', compact = 1, segment_merge = 3)"),
    );
    for id in 1..=N {
        insert_row(&many_segments, id, DIMS);
    }
    assert!(
        stored_segment_count(&many_segments, "docs") > 1,
        "one flush per row must leave more than one segment behind"
    );

    let one_segment = start_inillucent(AREA, "exact-one-segment");
    exec(
        &one_segment,
        &format!("CREATE VIRTUAL TABLE docs USING inillucent_search(title, body, dims = {DIMS}, mode = 'exact', compact = 0)"),
    );
    for id in 1..=N {
        insert_row(&one_segment, id, DIMS);
    }
    exec(&one_segment, "INSERT INTO docs(docs) VALUES ('compact')");
    assert_eq!(stored_segment_count(&one_segment, "docs"), 1);

    for query_id in [3i64, 17, 40, 1] {
        let query = hex(&vector_for(query_id, DIMS));
        let sql = format!(
            "SELECT rowid FROM docs WHERE vector = x'{query}' AND k = 10 AND recall = 1.0 ORDER BY rank"
        );
        let segmented = column(&many_segments, &sql);
        let exhaustive = column(&one_segment, &sql);
        assert_eq!(
            segmented, exhaustive,
            "query against row {query_id}'s vector must rank the same over several segments"
        );
        assert!(
            !exhaustive.is_empty(),
            "the exhaustive side found something"
        );
    }
}

/// A row deleted in a newer segment does not come back.
///
/// `compact = 1` flushes on every single delta, so the insert and the delete
/// each land in their own segment: an older one that still holds the row's
/// chunk, and a newer one whose manifest names the id as tombstoned with no
/// content of its own (`store::SegmentMeta::tombstoned`). A query has to read
/// that list and refuse to answer, or the older segment's chunk resurfaces.
///
/// **Fails without the change:** save `crates/inillucent-search/src/module.rs`
/// aside, `git checkout` it back to what task-1911 started from, and this
/// test still passes - the pre-existing fold path already deletes correctly,
/// which is exactly why the report reverts the whole crate together rather
/// than one file (see the report). What a revert of the whole crate's diff
/// changes is `stored_segment_count` staying at one throughout, which the
/// companion assertion in this file's sibling test is what actually goes red.
#[test]
fn a_delete_in_a_newer_segment_does_not_come_back() {
    const DIMS: usize = 4;
    let connection = start_inillucent(AREA, "delete-shadow");
    exec(
        &connection,
        &format!(
            "CREATE VIRTUAL TABLE docs USING inillucent_search(title, body, dims = {DIMS}, compact = 1)"
        ),
    );
    insert_row(&connection, 1, DIMS);
    assert_eq!(state(&connection, "docs", "folds"), 1, "the insert flushed");
    assert!(
        stored_segment_count(&connection, "docs") >= 1,
        "the insert's own segment is on disk"
    );

    exec(&connection, "DELETE FROM docs WHERE rowid = 1");
    assert_eq!(
        state(&connection, "docs", "folds"),
        2,
        "the delete flushed into a second segment"
    );

    let found = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'document'",
    );
    assert!(
        found.is_empty(),
        "the deleted row must not answer: {found:?}"
    );
    let by_rowid = column(&connection, "SELECT rowid FROM docs WHERE rowid = 1");
    assert!(
        by_rowid.is_empty(),
        "a direct rowid lookup must not find it either"
    );
}

/// A row updated in a newer segment returns the new vector and not the old.
///
/// The update lands in a second segment the same way the delete above does.
/// The check is a vector match: the new vector's exact nearest neighbour must
/// be this row, and the old vector must not still find it - which is what
/// would happen if the older segment's now-stale chunk were still being
/// searched instead of being shadowed by the newer one.
///
/// **Fails without the change:** as with the delete test above, the
/// pre-existing fold path already applies an update correctly; what this
/// suite as a whole depends on task-1911 for is that the update's own segment
/// is a *new* one rather than a rewrite of the one the insert used - see
/// `compact_collapses_every_segment_into_one` for the assertion that
/// actually distinguishes the two.
#[test]
fn an_update_in_a_newer_segment_wins_over_an_older_one() {
    const DIMS: usize = 4;
    let connection = start_inillucent(AREA, "update-shadow");
    exec(
        &connection,
        &format!(
            "CREATE VIRTUAL TABLE docs USING inillucent_search(title, body, dims = {DIMS}, mode = 'exact', compact = 1)"
        ),
    );
    // Three decoys, each with its own distinct vector, so that a query for
    // `old_vector` after the update below has somewhere else to land. With
    // row 1 as the only document in the table, an exact `k = 1` search always
    // returns *a* rowid regardless of how far away it is - the decoys are
    // what makes "found row 1" and "found something else" a real difference
    // rather than a foregone conclusion.
    for id in [101i64, 102, 103] {
        insert_row(&connection, id, DIMS);
    }
    let old_vector = vector_for(1, DIMS);
    exec(
        &connection,
        &format!(
            "INSERT INTO docs(rowid, title, body, vector) VALUES (1, 'title', 'original body', x'{}')",
            hex(&old_vector)
        ),
    );
    let folds_after_insert = state(&connection, "docs", "folds");

    // A distinct new vector, generated from a different seed so it cannot
    // collide with the old one or with a decoy's.
    let new_vector = vector_for(999, DIMS);
    exec(
        &connection,
        &format!(
            "UPDATE docs SET body = 'revised body', vector = x'{}' WHERE rowid = 1",
            hex(&new_vector)
        ),
    );
    assert!(
        state(&connection, "docs", "folds") > folds_after_insert,
        "the update flushed its own new segment"
    );

    let matches_new = column(
        &connection,
        &format!(
            "SELECT rowid FROM docs WHERE vector = x'{}' AND k = 1 AND recall = 1.0",
            hex(&new_vector)
        ),
    );
    assert_eq!(
        matches_new,
        vec!["1".to_string()],
        "the new vector finds row 1"
    );

    let matches_old = column(
        &connection,
        &format!(
            "SELECT rowid FROM docs WHERE vector = x'{}' AND k = 1 AND recall = 1.0",
            hex(&old_vector)
        ),
    );
    assert_ne!(
        matches_old,
        vec!["1".to_string()],
        "the old vector must not still find row 1 through a stale, shadowed \
         chunk - it should land on whichever decoy is nearest instead: {matches_old:?}"
    );
}

/// `compact` still collapses every live segment into one.
///
/// Four single-row flushes leave four live segments behind (`segment_merge`
/// is pinned high enough that none of them merge on their own), which
/// `drop-old-generations` then reduces to exactly four physical row groups
/// (nothing to reclaim yet, since nothing has been superseded). `compact`
/// replaces the whole manifest with one fresh segment, and a second
/// `drop-old-generations` reclaims the four it replaced, leaving one.
///
/// **Fails without the change:** revert `crates/inillucent-search` to what
/// task-1911 started from and re-run this test - `stored_segment_count`
/// after the four inserts reads `1` throughout, because the old code folds
/// every commit into the same single published generation rather than
/// writing a new one; the assertion that it must be greater than one is the
/// one this test's whole point rests on, and it is the one a revert turns
/// red. Compare `_agent_output/task-1911-segmented-generations/` for the
/// captured message.
#[test]
fn compact_collapses_every_segment_into_one() {
    const DIMS: usize = 4;
    let connection = start_inillucent(AREA, "compact-collapses");
    exec(
        &connection,
        &format!(
            "CREATE VIRTUAL TABLE docs USING inillucent_search(title, body, dims = {DIMS}, compact = 1, segment_merge = 64)"
        ),
    );
    for id in 1..=4 {
        insert_row(&connection, id, DIMS);
    }
    assert_eq!(state(&connection, "docs", "folds"), 4);
    exec(
        &connection,
        "INSERT INTO docs(docs) VALUES ('drop-old-generations')",
    );
    assert!(
        stored_segment_count(&connection, "docs") > 1,
        "four independent flushes must leave more than one segment on disk"
    );

    exec(&connection, "INSERT INTO docs(docs) VALUES ('compact')");
    exec(
        &connection,
        "INSERT INTO docs(docs) VALUES ('drop-old-generations')",
    );
    assert_eq!(
        stored_segment_count(&connection, "docs"),
        1,
        "compact must collapse every segment into one, and dropping the old \
         ones must leave exactly that one behind"
    );
    assert_eq!(
        state(&connection, "docs", "folds"),
        0,
        "compact starts the lineage again"
    );

    let found = column(&connection, "SELECT rowid FROM docs ORDER BY rowid");
    assert_eq!(found, vec!["1", "2", "3", "4"], "compact changed no row");
}

/// A file written before segmented generations - no `segments` row in
/// `%_state` at all - still answers, rather than answering zero rows.
///
/// The `segments` row is removed by hand after building an ordinary, single
/// segment table, which is what a file written before task-1911 actually
/// looks like: it has the one generation `state::GENERATION` and
/// `state::COVERED` already name, and no manifest naming it as a list. That is
/// simulated here rather than produced by an old binary, because there is no
/// old binary left to produce it with once this ticket lands - the same
/// reason `an_old_generation_survives_until_it_is_dropped` in `search.rs`
/// manipulates `%_gen` directly instead of arranging for two generations to
/// exist some other way.
///
/// **Fails without the change:** this scenario cannot even be constructed
/// without the change, since the `segments` state row this test deletes does
/// not exist before task-1911. Read the other direction, the claim this test
/// actually protects is `merge::live_segments`'s fallback: deleting its
/// `if let Some(segments) = store.read_segments(context)? { return Ok(segments); }`
/// early return (so the function always falls through to the zero-segment
/// path) makes this test fail with the query returning no rows, which is
/// captured in the report.
#[test]
fn a_pre_segment_file_still_answers() {
    const DIMS: usize = 4;
    let path = scratch(AREA, "legacy-shape", "inillucent");
    {
        let database = Database::open(&path).expect("it opens");
        let connection = database.session();
        exec(
            &connection,
            &format!(
                "CREATE VIRTUAL TABLE docs USING inillucent_search(title, body, dims = {DIMS}, compact = 0)"
            ),
        );
        for id in 1..=8 {
            insert_row(&connection, id, DIMS);
        }
        // One clean, single-pass segment - the shape a table written before
        // this ticket always had, one generation and nothing else.
        exec(&connection, "INSERT INTO docs(docs) VALUES ('compact')");
        assert_eq!(stored_segment_count(&connection, "docs"), 1);
        // The one row a manifest-aware build writes that a pre-task-1911 file
        // never had at all.
        exec(&connection, "DELETE FROM docs_state WHERE k = 'segments'");
    }

    let database = Database::open(&path).expect("it reopens");
    let connection = database.session();
    let found = column(&connection, "SELECT rowid FROM docs ORDER BY rowid");
    assert_eq!(
        found,
        (1..=8).map(|id| id.to_string()).collect::<Vec<_>>(),
        "a table with no segment manifest at all must still answer every row \
         it holds, not zero of them"
    );
    let matched = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'document' ORDER BY rowid",
    );
    assert_eq!(
        matched.len(),
        8,
        "the lexical branch must also see every row"
    );
}

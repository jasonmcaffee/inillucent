//! Bounding a segment merge's per commit cost (task-1911's follow-on): a
//! commit folds at most `Options::merge_budget_chunks` chunks while merging
//! segments, checkpoints what it could not finish, and the next commit
//! resumes it rather than starting the level over. A level whose segment
//! count reaches `Options::crisis_at` is the one exception - it is merged to
//! completion in one commit regardless, because a level that far behind is
//! already costing every query more than one expensive commit costs once.
//!
//! Invariant: **a half finished merge must never be visible to a query as
//! anything other than every original segment still live, or the one merged
//! segment that replaces them - never a mixture that drops or doubles a
//! row.** The tests below force a merge to stop mid way (a tiny
//! `merge_budget` relative to `segment_merge`, so the "at least one input
//! folds" progress guarantee cannot finish a level in a single commit) and
//! then read the table exactly as an application would, through ordinary SQL,
//! while that state is sitting in `%_state` under the `merge` key.
//!
//! Every scenario also picks parameters that stay well clear of
//! `Options::crisis_at` (`segment_fanin` squared), so what each test proves is
//! the *bounded* path, not the crisis escape hatch.

use inillucent_compat::differential::{scratch, start_inillucent};
use inillucent_engine::connect::{Connection, Database};
use inillucent_tree::datum::OwnedDatum;

/// Where this suite's scratch databases live.
const AREA: &str = "segment-merge-bound";

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

/// Returns one `%_state` row as an integer, or `-1` when it has never been
/// written.
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
/// physically holds - every live segment plus anything not yet reclaimed by
/// `drop-old-generations`, including an in-flight merge's own checkpoint.
fn stored_segment_count(connection: &Connection<'_>, table: &str) -> usize {
    column(
        connection,
        &format!("SELECT COUNT(DISTINCT generation) FROM {table}_gen"),
    )
    .first()
    .and_then(|text| text.parse::<usize>().ok())
    .unwrap_or(0)
}

/// Renders one value as text.
fn render(value: &OwnedDatum) -> String {
    match value {
        OwnedDatum::Null => "NULL".to_string(),
        OwnedDatum::Int(number) => number.to_string(),
        OwnedDatum::Real(number) => format!("{number:.6}"),
        OwnedDatum::Text(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        OwnedDatum::Blob(bytes) => format!("blob:{}", bytes.len()),
    }
}

/// Returns a deterministic, well separated unit-ish vector for one id, so an
/// exact vector search never has to break a tie between two rows' scores.
///
/// The same generator `write_latency.rs` and `segmented_generations.rs` both
/// already use, copied rather than shared: each test target is its own
/// compilation unit and a handful of lines of arithmetic is not worth a new
/// dependency edge between two test targets.
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

/// Inserts one row with a deterministic vector, one row per commit.
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

/// A search across a genuinely half finished merge answers exactly what the
/// same rows answer after a full, one pass `compact`.
///
/// `segment_merge = 3` and `merge_budget = 3` (each flushed row is its own
/// one-chunk segment, since `compact = 3`'s flush covers exactly one segment's
/// worth of rows) mean the third level zero segment triggers a merge whose
/// first commit can fold only one of its two non-base inputs before the
/// budget runs out - a merge is provably still in flight, not merely
/// "possibly still running", because folding both would cost 6 and only 3 is
/// allowed. `crisis_at` here is 9 (`segment_merge` squared), far above the 3
/// segments level zero ever holds, so this is the bounded path and not the
/// crisis escape hatch.
///
/// **Fails without the change:** `merge_budget`, `Options::crisis_at` and the
/// checkpoint/resume machinery in `continue_merge`/`finish_merge` do not
/// exist before this ticket - a merge either ran to completion inside the
/// commit that triggered it or (reverted further, to before task-1911) never
/// existed at all. Read the other direction, what this test actually
/// discriminates is whether a query folds a half finished merge's state
/// correctly: a version of `merge_cascade` that checkpoints a merge but
/// forgets to keep its original inputs live in the manifest until
/// `finish_merge` swaps them (or one that publishes an incomplete
/// accumulator early) would leave this test's two tables disagreeing.
#[test]
fn a_partially_merged_index_answers_like_a_fully_merged_one() {
    const DIMS: usize = 4;
    const N: i64 = 9;

    let partial = start_inillucent(AREA, "exact-partially-merged");
    exec(
        &partial,
        &format!(
            "CREATE VIRTUAL TABLE docs USING inillucent_search(title, body, dims = {DIMS}, \
             mode = 'exact', compact = 3, segment_merge = 3, merge_budget = 3)"
        ),
    );
    for id in 1..=N {
        insert_row(&partial, id, DIMS);
    }
    // Three flushes of three rows each is exactly what triggers the one merge
    // this test is about; a merge that finished in one commit anyway would
    // make the rest of this test a no-op rather than a proof, so this is
    // checked rather than assumed.
    assert!(
        state(&partial, "docs", "merge_work") > 0,
        "the third level zero segment must have started a merge"
    );
    assert!(
        stored_segment_count(&partial, "docs") > 1,
        "a merge still in flight leaves more than one physical segment behind"
    );

    let full = start_inillucent(AREA, "exact-fully-merged");
    exec(
        &full,
        &format!(
            "CREATE VIRTUAL TABLE docs USING inillucent_search(title, body, dims = {DIMS}, \
             mode = 'exact', compact = 0)"
        ),
    );
    for id in 1..=N {
        insert_row(&full, id, DIMS);
    }
    exec(&full, "INSERT INTO docs(docs) VALUES ('compact')");
    assert_eq!(stored_segment_count(&full, "docs"), 1);

    let all_partial = column(&partial, "SELECT rowid FROM docs ORDER BY rowid");
    let all_full = column(&full, "SELECT rowid FROM docs ORDER BY rowid");
    assert_eq!(
        all_partial, all_full,
        "a half finished merge must not drop or duplicate a row"
    );

    for query_id in [1i64, 5, 9, 3] {
        let query = hex(&vector_for(query_id, DIMS));
        let sql = format!(
            "SELECT rowid FROM docs WHERE vector = x'{query}' AND k = {N} AND recall = 1.0 ORDER BY rank"
        );
        let from_partial = column(&partial, &sql);
        let from_full = column(&full, &sql);
        assert_eq!(
            from_partial, from_full,
            "query against row {query_id}'s vector must rank the same over a \
             half finished merge as over a fully collapsed index"
        );
        assert!(
            !from_full.is_empty(),
            "the fully merged side found something"
        );
    }
}

/// A row deleted while a merge is genuinely still in flight stays deleted.
///
/// `segment_merge = 4` and `merge_budget = 1` mean the fourth level zero
/// segment's commit can fold only one of its three non-base inputs (the
/// progress guarantee lets exactly one through regardless of budget), so the
/// merge is still two inputs short of finishing when the row is deleted.
/// `crisis_at` is 16 here, far above the 4 segments level zero ever holds.
///
/// **Fails without the change:** before `merge::fold_segment` applied a
/// folded segment's own bare-deleted ids (this ticket's fix, described in its
/// own doc comment), this specific failure mode was a *different* row - one
/// whose put and whose bare delete were folded together in the same merge.
/// This test instead deletes a row from the merge's own **base** input, which
/// a query answers correctly by shadowing through `SegmentMeta::tombstoned`
/// on the *later* segment the delete flushes into, regardless of whether the
/// older merge underneath has finished - so what this test actually pins is
/// that the resumable merge's checkpoint-and-resume machinery does not
/// disturb that shadowing: reverting to a version of `finish_merge` that
/// forgets to recompute the merged segment's own tombstoned list from the
/// *original* inputs (rather than the accumulator's mid-fold state) would
/// still pass this one, because the later delete-only segment shadows it
/// either way - it is `a_merge_correctly_drops_a_row_whose_own_delete_has_no_live_chunk`,
/// below, that goes red on that specific defect.
#[test]
fn a_row_deleted_while_a_merge_is_in_flight_stays_deleted() {
    const DIMS: usize = 4;
    let connection = start_inillucent(AREA, "delete-during-merge");
    exec(
        &connection,
        &format!(
            "CREATE VIRTUAL TABLE docs USING inillucent_search(title, body, dims = {DIMS}, \
             mode = 'exact', compact = 1, segment_merge = 4, merge_budget = 1)"
        ),
    );
    for id in 1..=4 {
        insert_row(&connection, id, DIMS);
    }
    assert!(
        state(&connection, "docs", "merge_work") > 0,
        "the fourth level zero segment must have started a merge"
    );

    exec(&connection, "DELETE FROM docs WHERE rowid = 1");
    assert!(
        state(&connection, "docs", "merge_work") > 0,
        "the delete's own flush must have continued the merge further"
    );
    assert!(
        stored_segment_count(&connection, "docs") > 1,
        "a merge with a tiny budget cannot have collapsed everything to one \
         segment yet"
    );

    let found = column(&connection, "SELECT rowid FROM docs WHERE rowid = 1");
    assert!(
        found.is_empty(),
        "the deleted row must not answer a direct rowid lookup: {found:?}"
    );
    let matched = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'document' ORDER BY rowid",
    );
    assert_eq!(
        matched,
        vec!["2".to_string(), "3".to_string(), "4".to_string()],
        "the surviving rows must still answer and the deleted one must not"
    );
}

/// A row updated while a merge is genuinely still in flight returns the new
/// value, not the one the merge's own inputs were built from.
///
/// Same shape as the delete test above: `segment_merge = 4` and
/// `merge_budget = 1` leave the merge two inputs short of finishing when the
/// update lands. Three decoys with their own distinct vectors give an exact
/// `k = 1` search somewhere else to land, so "found row 1" and "found
/// something else" is a real difference rather than a foregone conclusion
/// with only one row in the table.
///
/// **Fails without the change:** as with the delete test, the shadowing this
/// depends on (a newer segment's `replace_document` winning over an older
/// one) already existed before this ticket; what is new here is that a merge
/// resuming across several commits must not, in the middle of that, ever
/// answer from the accumulator's half folded, mid-merge state instead of from
/// the live manifest a query actually reads - `SearchTable::merge_cascade`'s
/// invariant that the manifest names either every original input or the one
/// finished replacement, never the accumulator in between.
#[test]
fn a_row_updated_while_a_merge_is_in_flight_returns_the_new_value() {
    const DIMS: usize = 4;
    let connection = start_inillucent(AREA, "update-during-merge");
    exec(
        &connection,
        &format!(
            "CREATE VIRTUAL TABLE docs USING inillucent_search(title, body, dims = {DIMS}, \
             mode = 'exact', compact = 1, segment_merge = 4, merge_budget = 1)"
        ),
    );
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
    assert!(
        state(&connection, "docs", "merge_work") > 0,
        "the fourth level zero segment must have started a merge"
    );

    let new_vector = vector_for(999, DIMS);
    exec(
        &connection,
        &format!(
            "UPDATE docs SET body = 'revised body', vector = x'{}' WHERE rowid = 1",
            hex(&new_vector)
        ),
    );
    assert!(
        state(&connection, "docs", "merge_work") > 0,
        "the update's own flush must have continued the merge further"
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
        "the old vector must not still find row 1 through a stale, mid-merge \
         chunk - it should land on whichever decoy is nearest instead: {matches_old:?}"
    );
}

/// A merge correctly drops a row whose only delete was a bare tombstone -
/// recorded in one merged segment's own metadata, with no live chunk of its
/// own anywhere in that segment to carry the fact.
///
/// The put and the bare delete are folded together in the *same* merge here,
/// which is the shape `merge::fold_segment`'s fix is actually about: segment
/// A holds row 1 alive, segment B is a decoy, and segment C is *only* a
/// tombstone for row 1 (`compact = 1` flushes the delete into its own segment
/// with no content of its own, since a delete never creates a chunk). All
/// three land at level zero, `segment_merge = 3` merges them together in one
/// commit (a generous default `merge_budget`, since this test is about the
/// merge's *output* rather than its pacing), and only the finished, published
/// result is read - never a segment still shadowing the delete on its own.
///
/// **Fails without the change:** before this ticket, folding a segment during
/// a merge called only `replace_document` over its live chunks and never
/// consulted `SegmentMeta::tombstoned` at all (`merge::replay_into`, as it
/// was). Delete the `for id in tombstoned { accumulator.tombstone(...) }`
/// loop `merge::fold_segment` now has and this test goes from `["2"]` to
/// `["1", "2"]`: row 1 comes back, because segment C contributes nothing
/// through `replace_document` and nothing else in the merge ever asks it
/// whether an id died.
#[test]
fn a_merge_correctly_drops_a_row_whose_own_delete_has_no_live_chunk() {
    const DIMS: usize = 4;
    let connection = start_inillucent(AREA, "bare-tombstone-merge");
    exec(
        &connection,
        &format!(
            "CREATE VIRTUAL TABLE docs USING inillucent_search(title, body, dims = {DIMS}, \
             mode = 'exact', compact = 1, segment_merge = 3)"
        ),
    );
    insert_row(&connection, 1, DIMS); // segment A: row 1 alive
    insert_row(&connection, 2, DIMS); // segment B: row 2 alive, a decoy
    assert_eq!(
        state(&connection, "docs", "folds"),
        2,
        "two inserts, two flushes, no merge yet at only two segments"
    );

    // segment C: a bare delete of row 1, with no chunk of its own - and the
    // third level zero segment, which triggers the merge.
    exec(&connection, "DELETE FROM docs WHERE rowid = 1");
    assert!(
        state(&connection, "docs", "merge_work") > 0,
        "the third segment (the delete) must have triggered a merge"
    );

    let found = column(&connection, "SELECT rowid FROM docs ORDER BY rowid");
    assert_eq!(
        found,
        vec!["2".to_string()],
        "row 1's bare tombstone, folded together with its own put in the same \
         merge, must still remove it: {found:?}"
    );
}

/// A commit never folds more than its declared `merge_budget` - a count read
/// back from `%_state`, not a stopwatch.
///
/// `segment_merge = 3` (so a merge needs two non-base folds, four chunks of
/// work in all) and `merge_budget = 3` (enough for one of those two, not
/// both) mean the triggering commit can spend at most 3 and the merge must
/// then wait for a second commit to finish the rest. If a commit ever folded
/// the whole remaining backlog regardless of the budget - the defect this
/// whole ticket exists to close - the first read below would be 6, not 3.
///
/// **Fails without the change:** `Options::merge_budget_chunks` and
/// `state::MERGE_WORK` do not exist before this ticket, and the merge this
/// pins did not exist as a resumable operation at all before task-1911 - it
/// either ran to completion inside the triggering commit (unbounded) or
/// there was no segmented merge to bound in the first place.
#[test]
fn a_commit_never_folds_more_than_its_merge_budget() {
    const DIMS: usize = 4;
    let connection = start_inillucent(AREA, "bounded-merge-work");
    exec(
        &connection,
        &format!(
            "CREATE VIRTUAL TABLE docs USING inillucent_search(title, body, dims = {DIMS}, \
             compact = 3, segment_merge = 3, merge_budget = 3)"
        ),
    );
    for id in 1..=9 {
        insert_row(&connection, id, DIMS);
    }
    assert_eq!(
        state(&connection, "docs", "merge_work"),
        3,
        "the triggering commit must fold exactly one segment's worth (3 \
         chunks), never the other pending one as well (which would read 6)"
    );

    for id in 10..=12 {
        insert_row(&connection, id, DIMS);
    }
    assert_eq!(
        state(&connection, "docs", "merge_work"),
        3,
        "the next flush must have resumed the same merge and finished the \
         remaining segment, again bounded at 3 rather than folding a fresh \
         backlog unbounded"
    );

    let found = column(&connection, "SELECT rowid FROM docs ORDER BY rowid");
    assert_eq!(
        found,
        (1..=12).map(|id| id.to_string()).collect::<Vec<_>>(),
        "every row survives the merge, spread across two commits or not"
    );
}

/// A table with no merge in flight - the ordinary case - still opens and
/// answers after being closed and reopened.
///
/// Durability rule: the assertion is made through a handle that did not
/// write the data (`tests/inillucent-testing-tdd.md` 1.4), so what is read
/// back has been through the write-ahead log and recovery rather than a
/// page pool that happened to still hold it. This is the regression guard
/// for the merge machinery this ticket adds: an empty `state::MERGE` row
/// (`Store::read_merge_states` returning nothing) must round trip through a
/// close and reopen exactly as it did before this ticket existed to have an
/// opinion about it at all.
#[test]
fn a_table_with_no_merge_in_flight_still_opens_and_answers() {
    const DIMS: usize = 4;
    let path = scratch(AREA, "no-merge-in-flight", "inillucent");
    {
        let database = Database::open(&path).expect("it opens");
        let connection = database.connect();
        exec(
            &connection,
            &format!(
                "CREATE VIRTUAL TABLE docs USING inillucent_search(title, body, dims = {DIMS})"
            ),
        );
        for id in 1..=5 {
            insert_row(&connection, id, DIMS);
        }
        assert!(
            state(&connection, "docs", "merge_work") <= 0,
            "five rows never even cross the default 1024-row flush threshold, \
             so `merge_cascade` is never reached at all and `merge_work` stays \
             unwritten (read back as -1) or, if it ever is written, zero"
        );
    }

    let database = Database::open(&path).expect("it reopens");
    let connection = database.connect();
    let found = column(&connection, "SELECT rowid FROM docs ORDER BY rowid");
    assert_eq!(
        found,
        (1..=5).map(|id| id.to_string()).collect::<Vec<_>>(),
        "every row must still be there after a close and reopen"
    );
    let matched = column(
        &connection,
        "SELECT rowid FROM docs WHERE docs MATCH 'document' ORDER BY rowid",
    );
    assert_eq!(
        matched.len(),
        5,
        "the lexical branch must also see every row"
    );
}

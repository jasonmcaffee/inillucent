//! A merge finished through task-1911's segment delta chain
//! (`inillucent_core::persist`'s `KIND_SEGMENT_DELTA`, written by
//! `SearchTable::continue_merge`) answers exactly what the same rows answer
//! after a full, one pass `compact`.
//!
//! Invariant: **the worst commit of a size tiered merge no longer has to
//! write the whole accumulator.** Before this ticket a
//! checkpoint reloaded the accumulator and re-serialised it whole with
//! `persist::write_index`, however little that checkpoint itself folded - so
//! a merge several levels up, whose accumulator already held a large share of
//! the corpus, paid that whole size on every checkpoint. A checkpoint now
//! writes only a small delta - a pointer to the checkpoint it continues, plus
//! the rows this one folded - and the finished chain is what a live segment
//! actually is from then on, never flattened back into one blob. This suite
//! does not measure that cost directly (`write_latency.rs` does); it proves
//! the chain answers correctly once it exists: `segment_merge_bound.rs`
//! already proves a merge still *mid flight* answers like a full compact,
//! and this is the same claim for a merge that has actually finished and
//! left a sealed, multi-link chain as the level it merged into's one live
//! segment.
//!
//! `compact = 1` flushes every row into its own segment, so `segment_merge =
//! 3` triggers a merge on the third row and `merge_budget = 1` - which the
//! "at least one input folds" progress guarantee turns into exactly one
//! input per commit regardless - spreads it over the two commits that
//! follow. By the fourth row the merge is provably finished: level zero
//! holds only that fourth row's own fresh segment, and the chain merging the
//! first three sits one level up, sealed. A fifth row is added so the table
//! is read with more than the three merged rows in it, and stops there
//! rather than a sixth: three more level zero segments would themselves
//! reach `segment_merge` and start a *second* merge, which is not what this
//! suite is about.

use inillucent_compat::differential::start_inillucent;
use inillucent_engine::connect::Connection;
use inillucent_tree::datum::OwnedDatum;

/// Where this suite's scratch databases live.
const AREA: &str = "segment-delta-chain";

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
/// `drop-old-generations`, chain links included.
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
/// The same generator `write_latency.rs`, `segmented_generations.rs` and
/// `segment_merge_bound.rs` each already carry their own copy of.
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

/// A search across a table whose top segment was written incrementally, as a
/// finished chain of small checkpoints, answers exactly what the same
/// documents answer after a full, one pass `compact`.
///
/// **Fails without the change**: revert `crates/inillucent-search` to before
/// this ticket and `continue_merge` writes each checkpoint by reloading the
/// accumulator and calling `persist::write_index` on the whole thing - which
/// still answers this test's queries correctly (that behaviour is not what
/// is broken), so what actually discriminates is `stored_segment_count`
/// staying meaningful under the new format and `integrity-check` walking a
/// chain-shaped segment through `merge::load_segment`'s new dispatch: revert
/// only the `is_segment_delta`/`KIND_STREAM` dispatch in
/// `merge::load_segment_bytes` back to an unconditional `read_index` call
/// and this test's own `integrity-check` line fails outright, because a
/// chain's bytes do not parse as the older monolithic stream at all.
#[test]
fn a_merge_finished_through_a_segment_delta_chain_answers_like_a_full_compact() {
    const DIMS: usize = 4;
    const N: i64 = 5;

    let chained = start_inillucent(AREA, "exact-chained");
    exec(
        &chained,
        &format!(
            "CREATE VIRTUAL TABLE docs USING inillucent_search(title, body, dims = {DIMS}, \
             mode = 'exact', compact = 1, segment_merge = 3, merge_budget = 1)"
        ),
    );
    for id in 1..=3 {
        insert_row(&chained, id, DIMS);
    }
    // The third row's own flush brings level zero to `segment_merge` (3)
    // segments, which starts the merge this suite is about and folds its
    // first non-free input in the same commit - checked here, right when it
    // happens, because `merge_work` reports only the most recent commit's
    // own share and a later row with nothing to merge would read back zero.
    assert!(
        state(&chained, "docs", "merge_work") > 0,
        "the third row's own flush must have started a merge"
    );
    assert_eq!(
        column(
            &chained,
            "SELECT COUNT(*) FROM docs_state WHERE k = 'merge'"
        ),
        vec!["1".to_string()],
        "a merge_budget of 1 must leave the merge checkpointed rather than finished in one commit"
    );

    for id in 4..=N {
        insert_row(&chained, id, DIMS);
    }
    let in_flight = column(
        &chained,
        "SELECT COUNT(*) FROM docs_state WHERE k = 'merge'",
    );
    assert_eq!(
        in_flight,
        vec!["0".to_string()],
        "by the fourth row the level zero merge triggered by the third must \
         have finished, leaving nothing checkpointed"
    );
    assert!(
        stored_segment_count(&chained, "docs") > 1,
        "a finished merge's chain leaves its own earlier checkpoints behind \
         until drop-old-generations reclaims them"
    );

    // Genuinely readable end to end, not merely "happens to work": this
    // walks every live segment - the chain included - through the same
    // strict reader a query uses.
    exec(
        &chained,
        "INSERT INTO docs(docs) VALUES ('integrity-check')",
    );

    let full = start_inillucent(AREA, "exact-full-compact");
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

    let all_chained = column(&chained, "SELECT rowid FROM docs ORDER BY rowid");
    let all_full = column(&full, "SELECT rowid FROM docs ORDER BY rowid");
    assert_eq!(
        all_chained, all_full,
        "a chain merged segment must not drop or duplicate a row"
    );

    for query_id in [1i64, 2, 3, 5] {
        let query = hex(&vector_for(query_id, DIMS));
        let sql = format!(
            "SELECT rowid FROM docs WHERE vector = x'{query}' AND k = {N} AND recall = 1.0 ORDER BY rank"
        );
        let from_chained = column(&chained, &sql);
        let from_full = column(&full, &sql);
        assert_eq!(
            from_chained, from_full,
            "query against row {query_id}'s vector must rank the same over the \
             chain merged segment as over a fully compacted one"
        );
        assert!(
            !from_full.is_empty(),
            "the fully compacted side found something"
        );
    }

    // And the chain is not a dead end: a further insert must fold onto it
    // correctly, the same as any other live segment.
    insert_row(&chained, N + 1, DIMS);
    let after = column(&chained, "SELECT rowid FROM docs ORDER BY rowid");
    assert_eq!(
        after.len(),
        (N + 1) as usize,
        "the table must still accept writes after the merge"
    );
}

/// `drop-old-generations` protects a whole in-flight chain, not only the id
/// `MergeState` names.
///
/// Before this ticket an in-flight merge's accumulator was always one
/// self-contained blob, so protecting the one id `state::MERGE` recorded was
/// the whole of what the command had to keep. A segment delta chain's newest
/// link instead points at an *earlier* checkpoint's own id - unreferenced by
/// anything else in `%_state` - so reclaiming generations by the old rule
/// would delete the very bytes the merge's next checkpoint needs in order to
/// resume, and the next row inserted would fail outright rather than
/// silently answer wrong: `merge::load_segment_resumable` would report the
/// base link "named but not stored".
///
/// **Fails without the change:** `command("drop-old-generations")` naming
/// only `merge_state.accumulator` (this ticket's `merge::chain_ids` is what
/// walks the rest of the chain) deletes segment one's bytes here the moment
/// the second checkpoint tries to resume from them, and the row inserted
/// right after `drop-old-generations` below raises an error where it must
/// instead succeed.
#[test]
fn drop_old_generations_protects_a_whole_in_flight_chain() {
    const DIMS: usize = 4;

    let connection = start_inillucent(AREA, "protects-chain");
    exec(
        &connection,
        &format!(
            "CREATE VIRTUAL TABLE docs USING inillucent_search(title, body, dims = {DIMS}, \
             mode = 'exact', compact = 1, segment_merge = 3, merge_budget = 1)"
        ),
    );
    for id in 1..=3 {
        insert_row(&connection, id, DIMS);
    }
    assert_eq!(
        column(
            &connection,
            "SELECT COUNT(*) FROM docs_state WHERE k = 'merge'"
        ),
        vec!["1".to_string()],
        "the third row must leave exactly one checkpointed merge, one link into its chain"
    );

    // Reclaims every generation the manifest and the in-flight merge do not
    // still need - and must leave the chain's own base link alone.
    exec(
        &connection,
        "INSERT INTO docs(docs) VALUES ('drop-old-generations')",
    );

    // The next row resumes the merge, which has to reload that base link to
    // keep folding. This is the line that raises "segment N is named but not
    // stored" without `chain_ids` protecting the whole chain.
    insert_row(&connection, 4, DIMS);
    assert_eq!(
        column(
            &connection,
            "SELECT COUNT(*) FROM docs_state WHERE k = 'merge'"
        ),
        vec!["0".to_string()],
        "the fourth row must finish the merge that survived the reclaim"
    );

    let rows = column(&connection, "SELECT rowid FROM docs ORDER BY rowid");
    assert_eq!(
        rows,
        vec!["1", "2", "3", "4"],
        "no row was lost across the reclaim and the resumed merge"
    );
}

-- The retrieval half of the interop question, asked backwards (task-2053).
--
-- `verify.sql` asks every reader what an `inillucent_search` table holds and
-- what its rows say. It never asks it to **search**: nothing in the suite ran a
-- term query, a ranked query or a nearest-neighbour query from an older binary
-- against a graph a newer build wrote. That is the same question the FTS5
-- layout break turned out to be, asked of the half of the file SQLite has no
-- equivalent of - and the half a format change is most likely to move.
--
-- ## Why this is a second file rather than more lines in `verify.sql`
--
-- `verify.sql`'s answers are recorded per release in
-- `tests/interop/<version>/expected.tsv`, written by that release's own binary
-- at the moment it was published. A question added there has no recorded answer
-- for any of the six, and the only way to get one is to run every released
-- binary again over a rebuilt fixture - which rewrites six checked-in files
-- that exist precisely because they were produced once, by that release, and
-- never touched since.
--
-- These questions need no recorded answer. They are asked of **one** database,
-- the one `release_format_history.rs` writes with the current build, and the
-- reference answer is what the current build gives for the same question on the
-- same file. The comparison is the whole point: the two sides are the two
-- builds, not this build and a file.
--
-- ## The shape each line has to have
--
-- The same as `verify.sql`: a `-- name: <label>` line, then one statement on
-- one line answering with one row of one column, with every ordering written
-- down.
--
-- ## Why the questions are what they are
--
-- The table `retrieval-build.sql` creates is `mode = 'approximate'`, which is
-- the HNSW graph rather than a scan - and then every question is asked in a
-- form whose answer does not depend on which nodes a traversal happened to
-- visit, because a difference in traversal is not a format break and an
-- assertion that cannot tell the two apart is worse than none:
--
--   * `nearest` asks for **one** neighbour of a vector that is byte for byte a
--     stored row's own. Its distance is zero, so any traversal that finds
--     anything at all finds it first.
--   * `term` and `ranked` are the lexical half. `ranked` names one document
--     that repeats its term, so the top of the ranking is decided by the
--     scores rather than by a tie.

-- name: vectors.rows
SELECT count(*) FROM vectors;

-- name: vectors.term
SELECT group_concat(id, ';') FROM (SELECT rowid AS id FROM vectors WHERE vectors MATCH 'extent' ORDER BY rowid);

-- name: vectors.ranked
SELECT rowid FROM vectors WHERE vectors MATCH 'ledger' ORDER BY rank LIMIT 1;

-- name: vectors.nearest
SELECT rowid FROM vectors WHERE vector = X'3333333F0000000000000000000000000000000000000000000000000000803F' AND k = 1;

-- name: vectors.content
SELECT group_concat(line, ';') FROM (SELECT rowid || '|' || content AS line FROM vectors ORDER BY rowid);

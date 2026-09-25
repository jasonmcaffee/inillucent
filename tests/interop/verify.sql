-- The questions every interop fixture is asked, by whoever is reading it.
--
-- One list, two readers. `tools/build-interop-fixture.ps1` runs these against a
-- database that release's own binary has just written, and records the answers
-- as `tests/interop/<version>/expected.tsv`.
-- `crates/inillucent-compat/tests/e2e/release_format.rs` runs the same lines
-- against the same file with the current build and compares. Neither side
-- holds a copy of the SQL, so neither can drift from the other.
--
-- ## The shape each line has to have
--
--   * a `-- name: <label>` line, then exactly one statement on one line
--   * the statement returns **one row of one column**, so the answer is a value
--     rather than a rendering that a later version could format differently
--   * every ordering is written down. A fingerprint built over rows in whatever
--     order the planner chose would change when the planner changed and say
--     nothing about the file.
--
-- ## Why the answers are the same for every release
--
-- `build.sql` is deterministic, so a fixture written by 0.1.1 and a fixture
-- written by 0.1.7 hold the same values. The per-release `expected.tsv` records
-- what *that* release answered, so a release that answered differently is the
-- finding rather than a mismatch nobody can locate.

-- name: note.rows
SELECT count(*) FROM note;

-- name: note.fingerprint
SELECT group_concat(line, ';') FROM (SELECT id || '|' || title || '|' || length(body) || '|' || coalesce(weight, -1) || '|' || tag AS line FROM note ORDER BY id);

-- name: note.by.title
SELECT group_concat(id, ';') FROM (SELECT id FROM note WHERE title LIKE 'note 11%' ORDER BY title);

-- name: note.collation
SELECT count(*) FROM note WHERE tag = 'ledger';

-- name: attachment.fingerprint
SELECT group_concat(line, ';') FROM (SELECT note_id || '|' || name || '|' || coalesce(length(payload), -1) AS line FROM attachment ORDER BY note_id, name);

-- name: attachment.overflow
SELECT length(payload) || ':' || substr(payload, 1, 8) || ':' || substr(payload, 39993, 8) FROM attachment WHERE name = 'over-a-page.bin';

-- name: fts.rows
SELECT count(*) FROM note_fts;

-- name: fts.match
SELECT group_concat(rowid, ';') FROM (SELECT rowid FROM note_fts WHERE note_fts MATCH 'segment' ORDER BY rowid);

-- name: corpus.rows
SELECT count(*) FROM corpus;

-- name: corpus.content
SELECT group_concat(line, ';') FROM (SELECT rowid || '|' || content AS line FROM corpus ORDER BY rowid);

-- name: log.only
SELECT title || '|' || body FROM note WHERE id = 9001;

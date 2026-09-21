-- A searchable graph, written by the current build for an older one to query.
--
-- Run by `release_format_history.rs` immediately after `build.sql`, on the
-- database it hands to every downloaded release. It is **not** run by
-- `tools/build-interop-fixture.ps1` and is not part of any checked-in fixture:
-- the six `expected.tsv` files record what `verify.sql` asked, and adding rows
-- to a table those questions count would change every one of their recorded
-- answers.
--
-- So this creates a table of its own rather than adding to `corpus`. Nothing in
-- `verify.sql` names `vectors`, and nothing here names `corpus`.
--
-- ## Why the numbers are what they are
--
--   * `mode = 'approximate'` is the HNSW graph. `corpus` in `build.sql` is
--     `mode = 'exact'`, which is the same stored rows read by comparing all of
--     them - so the graph, which is the part of the retrieval format most
--     likely to move, was never in a file an older binary was handed.
--   * `dims = 8` matches `corpus`, so the vectors below are short enough to
--     write out as blob literals and a reader can check one by eye.
--   * Twelve rows. The questions this answers are about the format, not about
--     recall, and every one of them is asked in a form a traversal cannot
--     change the answer to - see `retrieval.sql`.
--   * The `compact` command at the end is what makes the file worth handing
--     over at all: without it every row is still in the delta log, and the
--     immutable generation stream in `%_gen` - the largest and newest part of
--     this format - would never be read by the older binary.
--
-- A vector is a blob of little-endian 32-bit floats. Each row's is one at the
-- position its rowid picks, with its rowid over ten in the first position, so
-- no two rows share a direction and row 7's is unique - which is what makes
-- `retrieval.sql`'s `nearest` question have one answer.

CREATE VIRTUAL TABLE vectors USING inillucent_search(content, dims = 8, mode = 'approximate');

INSERT INTO vectors (rowid, content, vector) VALUES (1, 'the ledger holds a segment', X'CDCCCC3D0000803F000000000000000000000000000000000000000000000000');
INSERT INTO vectors (rowid, content, vector) VALUES (2, 'an extent is many pages', X'CDCC4C3E000000000000803F0000000000000000000000000000000000000000');
INSERT INTO vectors (rowid, content, vector) VALUES (3, 'a page is the unit of io', X'9A99993E00000000000000000000803F00000000000000000000000000000000');
INSERT INTO vectors (rowid, content, vector) VALUES (4, 'a segment is closed and never written again', X'CDCCCC3E0000000000000000000000000000803F000000000000000000000000');
INSERT INTO vectors (rowid, content, vector) VALUES (5, 'the ledger is read forward', X'0000003F000000000000000000000000000000000000803F0000000000000000');
INSERT INTO vectors (rowid, content, vector) VALUES (6, 'a ledger ledger ledger of ledgers', X'9A99193F00000000000000000000000000000000000000000000803F00000000');
INSERT INTO vectors (rowid, content, vector) VALUES (7, 'the extent map names every page', X'3333333F0000000000000000000000000000000000000000000000000000803F');
INSERT INTO vectors (rowid, content, vector) VALUES (8, 'a segment header names its extent', X'0000803F00000000000000000000000000000000000000000000000000000000');
INSERT INTO vectors (rowid, content, vector) VALUES (9, 'pages are written forward and read forward', X'6666663F0000803F000000000000000000000000000000000000000000000000');
INSERT INTO vectors (rowid, content, vector) VALUES (10, 'the io unit is the page', X'0000803F000000000000803F0000000000000000000000000000000000000000');
INSERT INTO vectors (rowid, content, vector) VALUES (11, 'a closed segment is never rewritten', X'CDCC8C3F00000000000000000000803F00000000000000000000000000000000');
INSERT INTO vectors (rowid, content, vector) VALUES (12, 'the forward ledger holds every extent', X'9A99993F0000000000000000000000000000803F000000000000000000000000');

INSERT INTO vectors (vectors) VALUES ('compact');

-- The database every published release writes, so a later build can read it.
--
-- `tools/build-interop-fixture.ps1 <version>` runs this file with that
-- release's own `inillucent.exe` and leaves the result in
-- `tests/interop/<version>/`. `crates/inillucent-compat/tests/release_format.rs`
-- opens those files with the current build and asks `verify.sql`'s questions of
-- them.
--
-- ## Every statement here has to run on 0.1.1
--
-- It is the oldest release with a fixture, and the file is run unchanged by all
-- of them, so a construct the oldest refuses cannot be used at all. Two that
-- were tried and are not here:
--
--   * `INSERT INTO <virtual table> SELECT ...` - 0.1.1 answers `syntax`, so the
--     FTS5 and search rows are written with `VALUES`.
--   * `group_concat(x, char(10))` - 0.1.1 answers `unsupported` for a computed
--     separator, so `verify.sql` separates with a literal.
--
-- ## What is deliberately in it
--
--   * an ordinary rowid table with an index over a text column, which is the
--     shape almost every application's tables have
--   * a `WITHOUT ROWID` table whose rows are index entries rather than table
--     rows, because the two are read by different code
--   * a **blob larger than a page**, so at least one value is stored outside
--     the leaf that holds its row
--   * an **FTS5 table**, whose shadow tables are storage no ordinary statement
--     can reach
--   * an **`inillucent_search` table**, which is the half of the file SQLite
--     has no equivalent of, and the half a format change is most likely to move
--
-- ## Why the numbers are small
--
-- Six releases have a fixture and each one is checked in, so the cost of a row
-- here is paid six times and for ever. What the case needs is one of each kind
-- of storage rather than a lot of any of them: the sizes are dominated by the
-- page each tree root takes, not by the rows. See `tests/interop/README.md`.

CREATE TABLE note (
  id     INTEGER PRIMARY KEY,
  title  TEXT NOT NULL,
  body   TEXT NOT NULL,
  weight REAL,
  tag    TEXT COLLATE NOCASE
);
CREATE INDEX note_title_idx ON note (title);

CREATE TABLE attachment (
  note_id INTEGER NOT NULL,
  name    TEXT NOT NULL,
  payload BLOB,
  PRIMARY KEY (note_id, name)
) WITHOUT ROWID;

CREATE VIRTUAL TABLE note_fts USING fts5(title, body);

CREATE VIRTUAL TABLE corpus USING inillucent_search(content, dims = 8, mode = 'exact');

INSERT INTO note (id, title, body, weight, tag)
  SELECT value,
         'note ' || value,
         'the body of note ' || value || ', which names a ledger, a segment and an extent',
         value / 4.0,
         CASE value % 3 WHEN 0 THEN 'Ledger' WHEN 1 THEN 'segment' ELSE 'EXTENT' END
    FROM generate_series(1, 120);

-- One value larger than a 32,768 byte page, so it is not stored in the leaf
-- that holds its row. `hex` doubles what `zeroblob` gives, so this is 40,000
-- bytes.
INSERT INTO attachment (note_id, name, payload) VALUES
  (1, 'over-a-page.bin', CAST(replace(hex(zeroblob(20000)), '0', 'p') AS BLOB)),
  (1, 'empty.bin', zeroblob(0)),
  (2, 'absent.bin', NULL);

INSERT INTO note_fts (rowid, title, body) VALUES
  (1, 'note 1', 'the ledger holds a segment'),
  (2, 'note 2', 'an extent is many pages'),
  (3, 'note 3', 'a page is the unit of io'),
  (4, 'note 4', 'a segment is closed and never written again'),
  (5, 'note 5', 'the ledger is read forward');

INSERT INTO corpus (rowid, content) VALUES
  (1, 'the ledger holds a segment'),
  (2, 'an extent is many pages'),
  (3, 'a page is the unit of io'),
  (4, 'a segment is closed and never written again'),
  (5, 'the ledger is read forward');

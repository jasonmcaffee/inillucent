-- A browser's history, modelled on Firefox's `places.sqlite`.
--
-- Built by `tools/build-realistic-fixtures.sh` with the pinned SQLite 3.53.4
-- shell, so the fixture is reproducible from this file and this file is what a
-- reviewer reads.
--
-- ## What it is for
--
-- `crates/inillucent-compat/tests/migrate_sqlite.rs` migrates small synthetic
-- fixtures: a few tables, a few rows, plain columns. None of them has a
-- `NOCASE` collation, a `DESC` index, a trigger, a view, a `WITHOUT ROWID`
-- table or a URL outside ASCII - and the combination is what a real database
-- has. The Nikaya corpus that broke the migration had all of them at once.
--
-- ## What is deliberately in it
--
--   * `NOCASE` on a column *and* on an index, which are two different carries
--   * a `DESC` index, whose direction a migration has to preserve or a keyset
--     read comes back in the wrong order
--   * two triggers keeping a denormalised count, so the migration has to carry
--     the trigger text and the count it produced
--   * a view over a join
--   * `WITHOUT ROWID` with a composite primary key
--   * URLs and titles outside ASCII, including a decomposed form
--   * a partial index, which is what Nikaya's queue uses
--   * an `AUTOINCREMENT` table whose high rows have been deleted, so the
--     `sqlite_sequence` row says a larger number than `max(id)` does and a
--     migration that dropped it can be told from one that carried it

PRAGMA foreign_keys = ON;

-- One visited address. `url` is NOCASE because a browser treats a host
-- case-insensitively, and that collation has to survive the migration or two
-- rows that were one become two.
CREATE TABLE place (
  id            INTEGER PRIMARY KEY,
  url           TEXT NOT NULL COLLATE NOCASE,
  title         TEXT,
  visit_count   INTEGER NOT NULL DEFAULT 0,
  last_visit_at INTEGER,
  is_bookmarked INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX place_url_idx ON place (url COLLATE NOCASE);
CREATE INDEX place_recent_idx ON place (last_visit_at DESC);
CREATE INDEX place_bookmarked_idx ON place (id) WHERE is_bookmarked = 1;

-- One visit to a place. The foreign key cascades, which a migration has to
-- carry as text and as behaviour.
CREATE TABLE visit (
  id        INTEGER PRIMARY KEY,
  place_id  INTEGER NOT NULL REFERENCES place (id) ON DELETE CASCADE,
  visited_at INTEGER NOT NULL,
  referrer_id INTEGER REFERENCES visit (id) ON DELETE SET NULL,
  transition TEXT NOT NULL DEFAULT 'link'
);
CREATE INDEX visit_place_idx ON visit (place_id, visited_at DESC);

-- The count `place.visit_count` holds is kept by these, not by the writer.
CREATE TRIGGER visit_counts_up AFTER INSERT ON visit FOR EACH ROW
BEGIN
  UPDATE place
     SET visit_count = visit_count + 1,
         last_visit_at = max(coalesce(last_visit_at, 0), NEW.visited_at)
   WHERE id = NEW.place_id;
END;
CREATE TRIGGER visit_counts_down AFTER DELETE ON visit FOR EACH ROW
BEGIN
  UPDATE place SET visit_count = visit_count - 1 WHERE id = OLD.place_id;
END;

-- A tag on a place, keyed by both, with no rowid of its own.
CREATE TABLE place_tag (
  place_id INTEGER NOT NULL REFERENCES place (id) ON DELETE CASCADE,
  tag      TEXT NOT NULL COLLATE NOCASE,
  PRIMARY KEY (place_id, tag)
) WITHOUT ROWID;

-- A downloaded file. `AUTOINCREMENT` rather than a plain `INTEGER PRIMARY KEY`
-- because the two differ in exactly one place: SQLite keeps the high-water mark
-- in `sqlite_sequence`, and a migration that did not carry that row would hand
-- the next insert a key a deleted row already had. The deletes below are what
-- make the two answers different - after them `max(id)` is 3 and the sequence
-- says 9 - so a test can tell which happened.
CREATE TABLE download (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  place_id   INTEGER REFERENCES place (id) ON DELETE SET NULL,
  filename   TEXT NOT NULL,
  bytes      INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX download_place_idx ON download (place_id);

CREATE VIEW most_visited AS
  SELECT p.url, p.title, p.visit_count, count(t.tag) AS tags
    FROM place p
    LEFT JOIN place_tag t ON t.place_id = p.id
   GROUP BY p.id, p.url, p.title, p.visit_count;

-- ---------------------------------------------------------------------------
-- The rows. Enough of them to matter and few enough to check in: the file is
-- under two megabytes, which is what makes it something a reviewer can read
-- the diff of.
-- ---------------------------------------------------------------------------

INSERT INTO place (id, url, title, is_bookmarked) VALUES
  (1, 'https://example.com/', 'Example', 1),
  (2, 'https://EXAMPLE.com/two', 'Example two', 0),
  (3, 'https://exämple.de/über-uns', 'Über uns', 1),
  (4, 'https://例え.jp/ページ', '例えページ', 0),
  (5, 'https://example.com/cafe' || char(769), 'Café, decomposed', 0),
  (6, 'https://example.com/caf' || char(233), 'Café, composed', 0);

-- A few thousand more, generated, so the tables have leaves rather than one
-- page each.
INSERT INTO place (id, url, title, is_bookmarked)
  SELECT value + 100,
         'https://site' || (value % 97) || '.example/page/' || value,
         'Page ' || value,
         value % 11 = 0
    FROM generate_series(1, 4000);

INSERT INTO visit (place_id, visited_at, transition)
  SELECT (value % 4000) + 101, 1700000000 + value,
         CASE value % 3 WHEN 0 THEN 'link' WHEN 1 THEN 'typed' ELSE 'bookmark' END
    FROM generate_series(1, 8000);

INSERT INTO visit (place_id, visited_at, transition) VALUES
  (1, 1700001000, 'typed'),
  (1, 1700002000, 'link'),
  (3, 1700003000, 'bookmark'),
  (5, 1700004000, 'link');

INSERT INTO place_tag (place_id, tag) VALUES
  (1, 'reading'), (1, 'Reading list'), (3, 'Deutsch'), (4, '日本語'), (5, 'café');

INSERT INTO place_tag (place_id, tag)
  SELECT (value % 4000) + 101, 'tag-' || (value % 37)
    FROM generate_series(1, 3000);

INSERT INTO download (place_id, filename, bytes)
  SELECT (value % 4000) + 101, 'file-' || value || '.zip', value * 1024
    FROM generate_series(1, 9);

-- The rows that raised the high-water mark are removed, so `sqlite_sequence`
-- is the only thing left that remembers them.
DELETE FROM download WHERE id > 3;

ANALYZE;

-- A retrieval corpus, modelled on Nikaya's schema.
--
-- Built by `tools/build-realistic-fixtures.sh` with the pinned SQLite 3.53.4
-- shell.
--
-- ## What is deliberately in it
--
--   * **FTS5 with external content**, which is the shape an application uses
--     when it already owns the rows and wants an index over them rather than a
--     second copy. A migration has to carry the `content=` and
--     `content_rowid=` options, the shadow tables, and the triggers that keep
--     the index in step - and a migration that carried the index without the
--     options would produce a table that answers nothing.
--   * **an `INTEGER PRIMARY KEY` table of eight thousand short rows**, which
--     is a real b-tree with real interior pages rather than one leaf.
--   * **a wide key table**: a composite primary key of three text columns,
--     which is what an index entry looks like when it is most of the row.
--   * a `sqlite_sequence` row, from `AUTOINCREMENT`, which the migration
--     carries and which `migrate_sqlite.rs` already asserts on the small
--     fixtures.
--
-- ## Why eight thousand and not two hundred thousand
--
-- The design said 200,000. This file is checked in, and the ceiling on a
-- checked-in fixture is two megabytes - a file past that is one nobody reviews
-- and the repository carries for ever. 20,000 documents with an FTS5 index
-- over them came to 3.6 MB, which is where this number came from rather than
-- from taste. Eight thousand is still a b-tree with interior pages, which is
-- what the case is about; the large form is the `nightly` tier's
-- `migrate_realistic`, built into scratch rather than checked in.

PRAGMA foreign_keys = ON;

CREATE TABLE document (
  id               INTEGER PRIMARY KEY AUTOINCREMENT,
  external_id      TEXT NOT NULL UNIQUE,
  kind             TEXT NOT NULL CHECK (kind IN ('email', 'attachment', 'note')),
  title            TEXT NOT NULL DEFAULT '',
  body             TEXT NOT NULL DEFAULT '',
  source_timestamp INTEGER,
  indexed_at       INTEGER
);
CREATE INDEX document_kind_idx ON document (kind);
CREATE INDEX document_when_idx ON document (source_timestamp DESC);

-- The FTS5 index over `document`, not a copy of it. `content=` is what makes
-- it external, and `content_rowid=` names the column its rowid comes from.
CREATE VIRTUAL TABLE document_fts USING fts5(
  title, body, content='document', content_rowid='id'
);

CREATE TRIGGER document_fts_insert AFTER INSERT ON document BEGIN
  INSERT INTO document_fts (rowid, title, body) VALUES (NEW.id, NEW.title, NEW.body);
END;
CREATE TRIGGER document_fts_delete AFTER DELETE ON document BEGIN
  INSERT INTO document_fts (document_fts, rowid, title, body)
    VALUES ('delete', OLD.id, OLD.title, OLD.body);
END;
CREATE TRIGGER document_fts_update AFTER UPDATE ON document BEGIN
  INSERT INTO document_fts (document_fts, rowid, title, body)
    VALUES ('delete', OLD.id, OLD.title, OLD.body);
  INSERT INTO document_fts (rowid, title, body) VALUES (NEW.id, NEW.title, NEW.body);
END;

-- The wide key: three text columns are the key, and the row is little else.
CREATE TABLE term_position (
  term       TEXT NOT NULL,
  field      TEXT NOT NULL,
  locator    TEXT NOT NULL,
  weight     REAL NOT NULL DEFAULT 1.0,
  PRIMARY KEY (term, field, locator)
) WITHOUT ROWID;

CREATE TABLE chunk (
  id          INTEGER PRIMARY KEY,
  document_id INTEGER NOT NULL REFERENCES document (id) ON DELETE CASCADE,
  ordinal     INTEGER NOT NULL,
  content     TEXT NOT NULL,
  UNIQUE (document_id, ordinal)
);

-- ---------------------------------------------------------------------------

INSERT INTO document (external_id, kind, title, body, source_timestamp)
  SELECT 'doc-' || value,
         CASE value % 3 WHEN 0 THEN 'email' WHEN 1 THEN 'attachment' ELSE 'note' END,
         'Document ' || value,
         'ledger '
           || CASE value % 2 WHEN 0 THEN 'segment ' ELSE '' END
           || CASE value % 3 WHEN 0 THEN 'extent ' ELSE '' END
           || CASE value % 97 WHEN 0 THEN 'rarity ' ELSE '' END
           || 'number' || value,
         1700000000 + value
    FROM generate_series(1, 8000);

INSERT INTO chunk (document_id, ordinal, content)
  SELECT (value % 8000) + 1, value % 4, 'chunk ' || value || ' of the corpus'
    FROM generate_series(1, 8000)
   WHERE value % 4 = 0;

INSERT INTO term_position (term, field, locator, weight)
  SELECT 'term-' || (value % 500),
         CASE value % 2 WHEN 0 THEN 'title' ELSE 'body' END,
         'doc-' || value,
         1.0 + (value % 7) / 10.0
    FROM generate_series(1, 3000);

ANALYZE;

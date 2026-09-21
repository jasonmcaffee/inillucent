-- A messaging application's archive: wide rows, blobs, generated columns.
--
-- Built by `tools/build-realistic-fixtures.sh` with the pinned SQLite 3.53.4
-- shell.
--
-- ## What is deliberately in it
--
--   * a **200 column** attachment table, because a fixed column array anywhere
--     in a migration fails at its own limit and nowhere before it, and 200 is
--     the width Nikaya's attachments table has
--   * **blobs from one byte to two hundred kilobytes**, which cross the extent
--     threshold at 4,096 byte pages and at 32,768 byte pages both
--   * **generated columns**, both `VIRTUAL` and `STORED`, which a migration has
--     to carry as expressions rather than as values - copying a stored
--     generated column's *value* into a plain column is a migration that works
--     until somebody updates the row it was generated from
--   * foreign keys with `ON DELETE CASCADE` and `ON UPDATE CASCADE`
--   * a `CHECK` naming an enumeration, and a `DEFAULT` that is an expression
--
-- ## Deterministic, not random
--
-- The blobs are built with `replace(hex(zeroblob(n)), '0', 'x')` rather than
-- with `randomblob(n)`. A fixture is checked in, and a fixture whose bytes
-- differ on every build is one `tools/build-realistic-fixtures.sh --check` can
-- never call current - so the check would report it stale for ever and nobody
-- would look at it again. What the case needs from a blob is its size.
--
-- ## Why the blobs stop at 200 KB
--
-- The design said 1 B to 3 MB. This file is checked in, and a 3 MB blob makes
-- a fixture nobody can review the diff of and a repository that carries it for
-- ever. The megabyte case lives in `crates/inillucent/tests/story_edges.rs`,
-- in a scratch database, where it costs nothing to keep. What a *fixture* has
-- to carry is a value past the extent threshold, and 200 KB is fifty pages at
-- the larger page size.

PRAGMA foreign_keys = ON;

CREATE TABLE conversation (
  id         INTEGER PRIMARY KEY,
  slug       TEXT NOT NULL UNIQUE,
  kind       TEXT NOT NULL CHECK (kind IN ('direct', 'group', 'channel')),
  created_at INTEGER NOT NULL DEFAULT (strftime('%s', '2024-01-01')),
  archived   INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE message (
  id              INTEGER PRIMARY KEY,
  conversation_id INTEGER NOT NULL REFERENCES conversation (id)
                    ON DELETE CASCADE ON UPDATE CASCADE,
  sender          TEXT NOT NULL,
  sent_at         INTEGER NOT NULL,
  body            TEXT NOT NULL DEFAULT '',
  -- A virtual generated column: computed on read, stored nowhere.
  body_length     INTEGER GENERATED ALWAYS AS (length(body)) VIRTUAL,
  -- A stored one: computed on write, and a migration that copied the value
  -- rather than the expression would be right until the next update.
  sent_day        TEXT GENERATED ALWAYS AS (date(sent_at, 'unixepoch')) STORED,
  edited          INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX message_conversation_idx ON message (conversation_id, sent_at DESC);
CREATE INDEX message_day_idx ON message (sent_day);

CREATE TABLE attachment (
  id            INTEGER PRIMARY KEY,
  message_id    INTEGER NOT NULL REFERENCES message (id) ON DELETE CASCADE,
  filename      TEXT NOT NULL,
  mime          TEXT NOT NULL DEFAULT 'application/octet-stream',
  bytes         BLOB,
  byte_size     INTEGER GENERATED ALWAYS AS (length(bytes)) VIRTUAL,
  c000 INTEGER, c001 INTEGER, c002 INTEGER, c003 INTEGER, c004 INTEGER,
  c005 INTEGER, c006 INTEGER, c007 INTEGER, c008 INTEGER, c009 INTEGER,
  c010 INTEGER, c011 INTEGER, c012 INTEGER, c013 INTEGER, c014 INTEGER,
  c015 INTEGER, c016 INTEGER, c017 INTEGER, c018 INTEGER, c019 INTEGER,
  c020 INTEGER, c021 INTEGER, c022 INTEGER, c023 INTEGER, c024 INTEGER,
  c025 INTEGER, c026 INTEGER, c027 INTEGER, c028 INTEGER, c029 INTEGER,
  c030 INTEGER, c031 INTEGER, c032 INTEGER, c033 INTEGER, c034 INTEGER,
  c035 INTEGER, c036 INTEGER, c037 INTEGER, c038 INTEGER, c039 INTEGER,
  c040 INTEGER, c041 INTEGER, c042 INTEGER, c043 INTEGER, c044 INTEGER,
  c045 INTEGER, c046 INTEGER, c047 INTEGER, c048 INTEGER, c049 INTEGER,
  c050 INTEGER, c051 INTEGER, c052 INTEGER, c053 INTEGER, c054 INTEGER,
  c055 INTEGER, c056 INTEGER, c057 INTEGER, c058 INTEGER, c059 INTEGER,
  c060 INTEGER, c061 INTEGER, c062 INTEGER, c063 INTEGER, c064 INTEGER,
  c065 INTEGER, c066 INTEGER, c067 INTEGER, c068 INTEGER, c069 INTEGER,
  c070 INTEGER, c071 INTEGER, c072 INTEGER, c073 INTEGER, c074 INTEGER,
  c075 INTEGER, c076 INTEGER, c077 INTEGER, c078 INTEGER, c079 INTEGER,
  c080 INTEGER, c081 INTEGER, c082 INTEGER, c083 INTEGER, c084 INTEGER,
  c085 INTEGER, c086 INTEGER, c087 INTEGER, c088 INTEGER, c089 INTEGER,
  c090 INTEGER, c091 INTEGER, c092 INTEGER, c093 INTEGER, c094 INTEGER,
  c095 INTEGER, c096 INTEGER, c097 INTEGER, c098 INTEGER, c099 INTEGER,
  c100 INTEGER, c101 INTEGER, c102 INTEGER, c103 INTEGER, c104 INTEGER,
  c105 INTEGER, c106 INTEGER, c107 INTEGER, c108 INTEGER, c109 INTEGER,
  c110 INTEGER, c111 INTEGER, c112 INTEGER, c113 INTEGER, c114 INTEGER,
  c115 INTEGER, c116 INTEGER, c117 INTEGER, c118 INTEGER, c119 INTEGER,
  c120 INTEGER, c121 INTEGER, c122 INTEGER, c123 INTEGER, c124 INTEGER,
  c125 INTEGER, c126 INTEGER, c127 INTEGER, c128 INTEGER, c129 INTEGER,
  c130 INTEGER, c131 INTEGER, c132 INTEGER, c133 INTEGER, c134 INTEGER,
  c135 INTEGER, c136 INTEGER, c137 INTEGER, c138 INTEGER, c139 INTEGER,
  c140 INTEGER, c141 INTEGER, c142 INTEGER, c143 INTEGER, c144 INTEGER,
  c145 INTEGER, c146 INTEGER, c147 INTEGER, c148 INTEGER, c149 INTEGER,
  c150 INTEGER, c151 INTEGER, c152 INTEGER, c153 INTEGER, c154 INTEGER,
  c155 INTEGER, c156 INTEGER, c157 INTEGER, c158 INTEGER, c159 INTEGER,
  c160 INTEGER, c161 INTEGER, c162 INTEGER, c163 INTEGER, c164 INTEGER,
  c165 INTEGER, c166 INTEGER, c167 INTEGER, c168 INTEGER, c169 INTEGER,
  c170 INTEGER, c171 INTEGER, c172 INTEGER, c173 INTEGER, c174 INTEGER,
  c175 INTEGER, c176 INTEGER, c177 INTEGER, c178 INTEGER, c179 INTEGER,
  c180 INTEGER, c181 INTEGER, c182 INTEGER, c183 INTEGER, c184 INTEGER,
  c185 INTEGER, c186 INTEGER, c187 INTEGER, c188 INTEGER, c189 INTEGER,
  c190 INTEGER, c191 INTEGER, c192 INTEGER, c193 INTEGER
);
CREATE INDEX attachment_message_idx ON attachment (message_id);
CREATE INDEX attachment_mime_idx ON attachment (mime COLLATE NOCASE);

CREATE TABLE reaction (
  message_id INTEGER NOT NULL REFERENCES message (id) ON DELETE CASCADE,
  who        TEXT NOT NULL,
  emoji      TEXT NOT NULL,
  PRIMARY KEY (message_id, who, emoji)
) WITHOUT ROWID;

-- ---------------------------------------------------------------------------

INSERT INTO conversation (id, slug, kind) VALUES
  (1, 'general', 'channel'),
  (2, 'ada-and-grace', 'direct'),
  (3, 'über-uns', 'group');

INSERT INTO message (id, conversation_id, sender, sent_at, body)
  SELECT value,
         (value % 3) + 1,
         CASE value % 4 WHEN 0 THEN 'ada' WHEN 1 THEN 'grace'
                        WHEN 2 THEN 'alan' ELSE 'édith' END,
         1700000000 + value * 37,
         'message number ' || value || ' ' ||
           replace(hex(zeroblob((value % 23) * 8)), '0', 'x')
    FROM generate_series(1, 3000);

-- A message whose body crosses the extent threshold on its own.
INSERT INTO message (id, conversation_id, sender, sent_at, body)
  VALUES (9001, 1, 'ada', 1700900000, replace(hex(zeroblob(24000)), '0', 'y'));

-- Attachments: one byte, one page, and two hundred kilobytes.
INSERT INTO attachment (id, message_id, filename, mime, bytes, c000, c193) VALUES
  (1, 1, 'one-byte.bin', 'application/octet-stream', zeroblob(1), 1, 193),
  (2, 2, 'a-page.bin', 'image/png', CAST(replace(hex(zeroblob(2048)), '0', 'b') AS BLOB), 2, 386),
  (3, 3, 'lärge.pdf', 'application/pdf', CAST(replace(hex(zeroblob(102400)), '0', 'c') AS BLOB), 3, 579),
  (4, 4, 'empty.txt', 'text/plain', zeroblob(0), 4, 772),
  (5, 5, 'absent.bin', 'application/octet-stream', NULL, 5, 965);

INSERT INTO attachment (id, message_id, filename, mime, bytes, c000, c100, c193)
  SELECT value + 100,
         (value % 3000) + 1,
         'file-' || value || '.dat',
         CASE value % 3 WHEN 0 THEN 'text/plain' WHEN 1 THEN 'image/png'
                        ELSE 'APPLICATION/PDF' END,
         CAST(replace(hex(zeroblob((value % 256) + 1)), '0', 'd') AS BLOB),
         value, value * 2, value * 3
    FROM generate_series(1, 400);

INSERT INTO reaction (message_id, who, emoji)
  SELECT (value % 3000) + 1,
         CASE value % 3 WHEN 0 THEN 'ada' WHEN 1 THEN 'grace' ELSE 'édith' END,
         CASE value % 4 WHEN 0 THEN '👍' WHEN 1 THEN '🎉' WHEN 2 THEN '👀' ELSE '🚀' END
    FROM generate_series(1, 2000);

ANALYZE;

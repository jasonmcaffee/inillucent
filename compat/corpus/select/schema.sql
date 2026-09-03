-- The schema the foundational SELECT corpus runs against.
--
-- It is created by the pinned SQLite binary, so every database the corpus is
-- graded on was written by the reference engine. The shapes here are chosen for
-- what they expose rather than for realism: an INTEGER PRIMARY KEY that is the
-- rowid, a WITHOUT ROWID table that has none, a NOCASE column whose index can
-- only be used by a comparison with the same collation, NULLs in every
-- nullable column, and values on both sides of the integer/real boundary.
CREATE TABLE people (
  id INTEGER PRIMARY KEY,
  name TEXT,
  team TEXT COLLATE NOCASE,
  score REAL,
  joined INTEGER,
  note
);
CREATE INDEX people_by_team ON people (team);
CREATE INDEX people_by_score ON people (score, name);
CREATE UNIQUE INDEX people_by_name ON people (name);

INSERT INTO people VALUES (1, 'ada', 'blue', 10.5, 2001, 'first');
INSERT INTO people VALUES (2, 'bob', 'Blue', -2.0, 2002, NULL);
INSERT INTO people VALUES (3, 'cai', 'red', 0.0, 2001, 'third');
INSERT INTO people VALUES (4, 'dee', 'RED', 99.25, 2003, '');
INSERT INTO people VALUES (5, 'eve', NULL, NULL, 2002, x'00ff');
INSERT INTO people VALUES (6, 'fay', 'green', 10.5, 2004, -1);
INSERT INTO people VALUES (7, 'gus', 'blue', 1e300, 2001, 9223372036854775807);
INSERT INTO people VALUES (8, 'hal', 'red', -1e300, 2005, -9223372036854775808);
INSERT INTO people VALUES (100, 'ivy', 'blue', 3.5, 2002, 'hundred');
INSERT INTO people VALUES (-3, 'jon', 'red', 2.5, 2000, 'negative');

CREATE TABLE teams (
  team TEXT PRIMARY KEY,
  region TEXT,
  founded INTEGER
) WITHOUT ROWID;

INSERT INTO teams VALUES ('blue', 'north', 1990);
INSERT INTO teams VALUES ('red', 'south', 1991);
INSERT INTO teams VALUES ('green', 'north', 1992);
INSERT INTO teams VALUES ('gone', 'east', 1993);

CREATE TABLE mixed (k INTEGER PRIMARY KEY, v);
INSERT INTO mixed VALUES (1, NULL);
INSERT INTO mixed VALUES (2, 0);
INSERT INTO mixed VALUES (3, 1);
INSERT INTO mixed VALUES (4, -1);
INSERT INTO mixed VALUES (5, 0.0);
INSERT INTO mixed VALUES (6, 1.5);
INSERT INTO mixed VALUES (7, '');
INSERT INTO mixed VALUES (8, '0');
INSERT INTO mixed VALUES (9, 'abc');
INSERT INTO mixed VALUES (10, x'');
INSERT INTO mixed VALUES (11, x'00');
INSERT INTO mixed VALUES (12, 9223372036854775807);
INSERT INTO mixed VALUES (13, -9223372036854775808);
INSERT INTO mixed VALUES (14, 1e308);
INSERT INTO mixed VALUES (15, -0.0);

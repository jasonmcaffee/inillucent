-- The foundational SELECT corpus: one statement per line.
--
-- Every line is run against the same SQLite-created database by both engines
-- and the tagged rows are compared exactly - storage class included, so an
-- integer that came back as a real is a failure rather than a rounding detail.
--
-- The statements are ordered by what they exercise, and each one is here
-- because it can be wrong in a way that a simpler statement cannot.

-- Scans and projection
SELECT * FROM people
SELECT id FROM people
SELECT id, name, team, score, joined, note FROM people
SELECT name, id FROM people
SELECT 1, 'x', 2.5, NULL, x'0a'
SELECT people.id, people.name FROM people
SELECT p.id FROM people AS p
SELECT * FROM teams
SELECT * FROM mixed

-- The rowid and its three spellings
SELECT rowid, id FROM people
SELECT _rowid_ FROM people
SELECT oid FROM people
SELECT rowid FROM people WHERE rowid = 4
SELECT rowid FROM people WHERE rowid > 3
SELECT rowid FROM people WHERE rowid >= 3 AND rowid < 7
SELECT id FROM people WHERE id = 100
SELECT id FROM people WHERE id = -3
SELECT id FROM people WHERE id = 4.0
SELECT id FROM people WHERE id = 4.5
SELECT id FROM people WHERE id = '4'

-- Comparison, affinity and collation
SELECT name FROM people WHERE name = 'ada'
SELECT name FROM people WHERE team = 'blue'
SELECT name FROM people WHERE team = 'BLUE'
SELECT name FROM people WHERE team = 'BLUE' COLLATE BINARY
SELECT name FROM people WHERE name > 'd'
SELECT name FROM people WHERE score > 0
SELECT name FROM people WHERE score >= 10.5
SELECT name FROM people WHERE joined <> 2001
SELECT k, v FROM mixed WHERE v = 0
SELECT k, v FROM mixed WHERE v = '0'
SELECT k, v FROM mixed WHERE v > 0
SELECT k, v FROM mixed WHERE v IS NULL
SELECT k, v FROM mixed WHERE v IS NOT NULL
SELECT k FROM mixed WHERE v IS 0
SELECT k FROM mixed WHERE v IS NOT 0

-- Three-valued logic
SELECT id FROM people WHERE note IS NULL
SELECT id FROM people WHERE note IS NOT NULL
SELECT id FROM people WHERE NOT (score > 0)
SELECT id FROM people WHERE score > 0 AND note IS NOT NULL
SELECT id FROM people WHERE score > 0 OR note IS NULL
SELECT id FROM people WHERE team IS NULL OR team = 'red'
SELECT NULL AND 0, NULL AND 1, NULL OR 0, NULL OR 1, NOT NULL

-- Arithmetic and its edges
SELECT 1 + 1, 1 - 2, 3 * 4, 7 / 2, 7 % 2
SELECT 7.0 / 2, 1 / 0, 1 % 0, -0.0, 0.0
SELECT 9223372036854775807 + 1
SELECT 'abc' + 1, '2' + 1, '2.5' + 1, x'31' + 1
SELECT 5.5 % 2.5, -7 / 2, -7 % 2
SELECT 1 << 3, 1 >> 1, 1 << 64, -1 >> 64, 5 & 3, 5 | 3, ~5
SELECT 'a' || 'b', 1 || 2, NULL || 'a'
SELECT -id, +id, ~id FROM people WHERE id = 4
SELECT score * 2, score + joined FROM people WHERE id = 1

-- CASE, CAST and COALESCE
SELECT CASE WHEN score > 0 THEN 'pos' WHEN score < 0 THEN 'neg' ELSE 'zero' END FROM people
SELECT CASE team WHEN 'blue' THEN 1 WHEN 'red' THEN 2 END FROM people
SELECT CASE WHEN 0 THEN 'a' END
SELECT CAST(v AS INTEGER), CAST(v AS REAL), CAST(v AS TEXT), CAST(v AS BLOB) FROM mixed
SELECT CAST('12abc' AS INTEGER), CAST('abc' AS INTEGER), CAST(1.9 AS INTEGER)
SELECT coalesce(note, 'none') FROM people
SELECT ifnull(team, 'none'), nullif(team, 'blue') FROM people

-- Built-in functions
SELECT typeof(v), v FROM mixed
SELECT length(name), upper(name), lower(team) FROM people
SELECT abs(score), round(score), round(score, 1) FROM people
SELECT substr(name, 2), substr(name, 2, 1), substr(name, -2) FROM people
SELECT hex(note), quote(note) FROM people
SELECT instr(name, 'a'), replace(name, 'a', 'A') FROM people
SELECT trim('  x  '), ltrim('  x  '), rtrim('  x  '), trim('xxaxx', 'x')
SELECT max(1, 2, 3), min(1, 2, 3), max(1, NULL), sign(score) FROM people WHERE id = 1
SELECT char(97, 98), unicode('a'), zeroblob(3)
SELECT iif(score > 0, 'yes', 'no') FROM people
SELECT concat('a', NULL, 'b'), concat_ws('-', 'a', NULL, 'b')

-- LIKE and GLOB
SELECT name FROM people WHERE name LIKE 'a%'
SELECT name FROM people WHERE name LIKE '_a_'
SELECT name FROM people WHERE name LIKE 'A%'
SELECT name FROM people WHERE name NOT LIKE 'a%'
SELECT name FROM people WHERE name GLOB 'a*'
SELECT name FROM people WHERE name GLOB '[a-c]*'
SELECT 'a%b' LIKE 'a\%b' ESCAPE '\'
SELECT like('a%', 'abc'), glob('a*', 'abc')

-- IN and BETWEEN
SELECT id FROM people WHERE id IN (1, 3, 5)
SELECT id FROM people WHERE id NOT IN (1, 3, 5)
SELECT id FROM people WHERE id IN ()
SELECT k FROM mixed WHERE v IN (0, 1)
SELECT k FROM mixed WHERE v IN (0, NULL)
SELECT k FROM mixed WHERE v NOT IN (0, NULL)
SELECT id FROM people WHERE score BETWEEN 0 AND 20
SELECT id FROM people WHERE score NOT BETWEEN 0 AND 20
SELECT id FROM people WHERE name BETWEEN 'b' AND 'e'

-- ORDER BY, LIMIT and OFFSET
SELECT name FROM people ORDER BY name
SELECT name FROM people ORDER BY name DESC
SELECT id, score FROM people ORDER BY score, id
SELECT id, score FROM people ORDER BY score DESC, id
SELECT id, score FROM people ORDER BY score NULLS LAST, id
SELECT id, score FROM people ORDER BY score DESC NULLS FIRST, id
SELECT id, team FROM people ORDER BY team, id
SELECT id, team FROM people ORDER BY team COLLATE BINARY, id
SELECT id FROM people ORDER BY id LIMIT 3
SELECT id FROM people ORDER BY id LIMIT 3 OFFSET 2
SELECT id FROM people ORDER BY id LIMIT 2, 3
SELECT id FROM people ORDER BY id LIMIT 0
SELECT id FROM people ORDER BY id LIMIT -1
SELECT id FROM people ORDER BY 1 DESC
SELECT name AS n FROM people ORDER BY n
SELECT id, name FROM people ORDER BY id DESC LIMIT 100 OFFSET 100

-- DISTINCT
SELECT DISTINCT team FROM people
SELECT DISTINCT joined FROM people
SELECT DISTINCT team, joined FROM people
SELECT DISTINCT v FROM mixed
SELECT DISTINCT score FROM people ORDER BY score

-- Aggregates over the whole table
SELECT count(*) FROM people
SELECT count(note) FROM people
SELECT count(DISTINCT team) FROM people
SELECT sum(id), total(id), avg(id) FROM people
SELECT sum(score), total(score), avg(score) FROM people
SELECT min(id), max(id), min(name), max(name) FROM people
SELECT min(score), max(score) FROM people
SELECT count(*) FROM people WHERE id > 1000
SELECT sum(id) FROM people WHERE id > 1000
SELECT total(id) FROM people WHERE id > 1000
SELECT avg(id) FROM people WHERE id > 1000
SELECT min(id) FROM people WHERE id > 1000
-- `group_concat` joins its values in an arbitrary order, which SQLite
-- documents, so only the single-row forms can be compared: anything wider
-- compares which access path each engine chose rather than what it computed.
SELECT group_concat(name) FROM people WHERE id = 1
SELECT group_concat(name, '-') FROM people WHERE id = 1
SELECT group_concat(name) FROM people WHERE id > 1000
SELECT sum(v), total(v), count(v) FROM mixed

-- GROUP BY and HAVING
SELECT team, count(*) FROM people GROUP BY team
SELECT team, count(*) FROM people GROUP BY team HAVING count(*) > 1
SELECT joined, count(*), sum(id) FROM people GROUP BY joined
SELECT joined, min(score), max(score) FROM people GROUP BY joined
SELECT note IS NULL, count(*) FROM people GROUP BY note IS NULL
SELECT team, length(group_concat(name)) FROM people GROUP BY team
SELECT id % 3, count(*) FROM people GROUP BY id % 3
SELECT team, count(*) FROM people GROUP BY team ORDER BY count(*) DESC, team
SELECT team, count(*) FROM people GROUP BY 1

-- Joins
SELECT people.name, teams.region FROM people, teams WHERE people.team = teams.team
SELECT people.name, teams.region FROM people JOIN teams ON people.team = teams.team
SELECT people.name, teams.region FROM people INNER JOIN teams ON people.team = teams.team
SELECT p.name, t.region FROM people AS p JOIN teams AS t ON p.team = t.team
SELECT name, region FROM people JOIN teams USING (team)
SELECT * FROM people JOIN teams USING (team)
SELECT name, region FROM people NATURAL JOIN teams
SELECT count(*) FROM people CROSS JOIN teams
SELECT p.name, t.region FROM people AS p JOIN teams AS t ON p.team = t.team WHERE t.region = 'north'
SELECT t.team, count(*) FROM teams AS t JOIN people AS p ON p.team = t.team GROUP BY t.team

-- VALUES
VALUES (1)
VALUES (1, 'a'), (2, 'b'), (3, NULL)
SELECT 1 WHERE 1
SELECT 1 WHERE 0
SELECT 1 WHERE NULL

-- The fixture the live PostgreSQL acceptance test migrates.
--
-- It is deliberately awkward: every family of the type map, a schema other than
-- `public`, an identifier that needs quoting, a composite primary key, a table
-- with no key at all, a table with no rows, a view and a trigger that must be
-- reported as not carried, and the values that a careless migration rounds -
-- a wide `numeric`, a float that is not exactly representable, an empty string
-- next to a NULL, and bytes that are not valid UTF-8.
--
-- Loaded by `psql`, which is the point: the rows this test compares against
-- were written by PostgreSQL's own client, so the oracle is not the code under
-- test.

CREATE SCHEMA IF NOT EXISTS sales;

CREATE TABLE note (
    id          bigint PRIMARY KEY,
    title       text NOT NULL,
    body        text,
    "odd Name"  varchar(64),
    published   boolean NOT NULL,
    rating      double precision,
    price       numeric(38, 10),
    tiny        smallint,
    payload     bytea,
    tags        text[],
    created     timestamp,
    created_at  timestamptz,
    day         date,
    identity    uuid,
    document    jsonb
);

INSERT INTO note VALUES
    (1, 'first', 'a body', 'quoted', true, 1.5, 12345678901234567890.1234567890, 7,
     '\x00ff10'::bytea, ARRAY['a','b'], '2026-01-02 03:04:05', '2026-01-02 03:04:05+00',
     '2026-01-02', '11111111-2222-3333-4444-555555555555', '{"a": 1}'),
    (2, 'second', NULL, '', false, 0.1, -0.0000000001, -32768,
     '\xdeadbeef'::bytea, ARRAY[]::text[], NULL, NULL, NULL, NULL, NULL),
    (3, 'third', 'unicode: caf' || chr(233) || ' 🛟', NULL, true, 'NaN'::float8, 0, 32767,
     ''::bytea, NULL, '1970-01-01 00:00:00', '1970-01-01 00:00:00+00',
     '1970-01-01', '00000000-0000-0000-0000-000000000000', '[]');

CREATE TABLE reading (
    note        bigint NOT NULL,
    reader      text NOT NULL,
    at          timestamptz NOT NULL,
    PRIMARY KEY (note, reader)
);

INSERT INTO reading VALUES
    (1, 'jason', '2026-01-02 03:04:05+00'),
    (1, 'someone else', '2026-01-03 03:04:05+00'),
    (2, 'jason', '2026-01-04 03:04:05+00');

-- No primary key at all, and a duplicated row: the digest is a multiset hash,
-- so two identical rows must not cancel.
CREATE TABLE event (
    kind    text,
    amount  integer
);

INSERT INTO event VALUES ('open', 1), ('open', 1), ('close', 2), (NULL, NULL);

-- Empty, which is the case a count check passes trivially and a digest still
-- has to agree on.
CREATE TABLE unused (
    id integer PRIMARY KEY
);

-- A schema other than `public`, whose table has the same name as one in it:
-- these must not collide into a single destination table.
CREATE TABLE sales."order" (
    id      integer PRIMARY KEY,
    total   numeric(12, 2) NOT NULL
);

INSERT INTO sales."order" VALUES (1, '10.50'), (2, '0.00');

CREATE TABLE sales.note (
    id      integer PRIMARY KEY,
    memo    text
);

INSERT INTO sales.note VALUES (1, 'not the public one');

-- Neither of these is carried, and both must be reported.
CREATE VIEW recent AS SELECT id, title FROM note WHERE published;

CREATE FUNCTION touch() RETURNS trigger AS $$ BEGIN RETURN NEW; END $$ LANGUAGE plpgsql;
CREATE TRIGGER note_touch BEFORE UPDATE ON note FOR EACH ROW EXECUTE FUNCTION touch();

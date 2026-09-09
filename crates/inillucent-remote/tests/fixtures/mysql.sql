-- The fixture the live MySQL acceptance test migrates.
--
-- Same shape as the PostgreSQL one and for the same reason: every family of the
-- type map, an identifier that needs quoting, a composite key, a table with no
-- key, an empty table, a view and a trigger that must be reported as not
-- carried, and the values a careless migration rounds - a wide DECIMAL, an
-- unsigned BIGINT past the signed range, an empty string next to a NULL, and
-- bytes that are not valid UTF-8.
--
-- Loaded by the `mysql` client, so the rows this test compares against were
-- written by MySQL's own tooling rather than by the code under test.

CREATE TABLE note (
    id          BIGINT NOT NULL,
    title       VARCHAR(128) NOT NULL,
    body        TEXT,
    `odd Name`  VARCHAR(64),
    published   TINYINT(1) NOT NULL,
    rating      DOUBLE,
    price       DECIMAL(38, 10),
    tiny        SMALLINT,
    huge        BIGINT UNSIGNED,
    payload     VARBINARY(64),
    created     DATETIME,
    day         DATE,
    document    JSON,
    mood        ENUM('good', 'bad'),
    PRIMARY KEY (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

INSERT INTO note VALUES
    (1, 'first', 'a body', 'quoted', 1, 1.5, '12345678901234567890.1234567890', 7,
     18446744073709551615, UNHEX('00FF10'), '2026-01-02 03:04:05', '2026-01-02',
     '{"a": 1}', 'good'),
    (2, 'second', NULL, '', 0, 0.1, '-0.0000000001', -32768,
     0, UNHEX('DEADBEEF'), NULL, NULL, NULL, NULL),
    (3, 'third', _utf8mb4'unicode: café 🛟', NULL, 1, -1.25, '0.0000000000', 32767,
     9223372036854775808, UNHEX(''), '1970-01-01 00:00:00', '1970-01-01', '[]', 'bad');

CREATE TABLE reading (
    note    BIGINT NOT NULL,
    reader  VARCHAR(64) NOT NULL,
    at      DATETIME NOT NULL,
    PRIMARY KEY (note, reader)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

INSERT INTO reading VALUES
    (1, 'jason', '2026-01-02 03:04:05'),
    (1, 'someone else', '2026-01-03 03:04:05'),
    (2, 'jason', '2026-01-04 03:04:05');

-- No primary key, and a duplicated row: the digest is a multiset hash, so two
-- identical rows must not cancel.
CREATE TABLE event (
    kind    VARCHAR(32),
    amount  INT
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4;

INSERT INTO event VALUES ('open', 1), ('open', 1), ('close', 2), (NULL, NULL);

CREATE TABLE unused (
    id INT NOT NULL PRIMARY KEY
) ENGINE = InnoDB;

-- Neither of these is carried, and both must be reported.
CREATE VIEW recent AS SELECT id, title FROM note WHERE published = 1;

CREATE TRIGGER note_touch BEFORE UPDATE ON note FOR EACH ROW SET NEW.title = NEW.title;

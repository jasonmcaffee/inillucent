// The Node wrapper against a real binary: write, read back, bind, and classify.
//
// **`resolve.test.mjs` reads this package's own source as text.** It checks the
// platform table and the shape of the error messages, which is worth having and
// is not a test of the wrapper: nothing in it calls `query` or `exec`, so a
// release could ship a wrapper that cannot run a statement and every test here
// would pass (task-1969, 5.7).
//
// So this file is the Go suite's three cases in Node - a round trip, a bound
// hostile string, and a classified status - because that suite is the one that
// was already doing the job.
//
// It needs a built binary, which `INILLUCENT_BIN` names. `tools/validate`'s
// `wrappers` stage sets it to `target/release/inillucent`; run by hand, set it
// yourself or put `inillucent` on `PATH`. When neither is there the suite skips
// with the command that provisions it, rather than failing on a machine that
// has never built the workspace.

import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';

import { inillucent, query, resolveBinary } from './index.mjs';

/** Whether a binary can be found at all, decided once. */
let available = true;
try {
  resolveBinary('inillucent');
} catch {
  available = false;
  console.error(
    'no inillucent binary: set INILLUCENT_BIN or put one on PATH ' +
      '(`cargo build --release -p inillucent-cli`); skipping',
  );
}

/**
 * Runs one case in a database of its own, and removes it afterwards.
 *
 * A directory per case rather than a file per case, because a database is a
 * file plus its log segments, and a case that left them behind would be a case
 * the next run reopened.
 *
 * @param name - the case's name
 * @param body - what to run, given the database's path
 */
function withDatabase(name, body) {
  test(name, { skip: !available }, async () => {
    const directory = mkdtempSync(join(tmpdir(), 'inillucent-roundtrip-'));
    try {
      await body(join(directory, 'probe.rdb'));
    } finally {
      rmSync(directory, { recursive: true, force: true });
    }
  });
}

withDatabase('a database can be created, written to and read back', async (db) => {
  const made = await inillucent('batch', {
    db,
    sql: `
      CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, age INTEGER);
      INSERT INTO people VALUES (1, 'Ada', 36), (2, 'Grace', 45), (3, 'Alan', 41)
    `,
  });
  assert.equal(made.ok, true, made.message);

  const result = await inillucent('query', {
    db,
    sql: 'SELECT name FROM people WHERE age > ?1 ORDER BY age',
    params: [40],
  });
  assert.equal(result.ok, true, result.message);
  assert.equal(result.total, 2, 'total is exact and is not the row count');

  const rows = await query('SELECT name FROM people WHERE age > ?1 ORDER BY age', {
    db,
    params: [40],
  });
  assert.deepEqual(
    rows.map((row) => row.name),
    ['Alan', 'Grace'],
    'the wrong rows, or the wrong order',
  );
});

withDatabase('a parameter is bound rather than pasted', async (db) => {
  // The quote is the point. Pasted into the statement it ends the string
  // literal and the rest is parsed as SQL; bound, it is one value with a quote
  // in it. This is the case that tells a wrapper apart from a string join.
  const hostile = "Robert'); DROP TABLE people; --";
  const made = await inillucent('batch', {
    db,
    sql: 'CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT)',
  });
  assert.equal(made.ok, true, made.message);

  const written = await inillucent('exec', {
    db,
    sql: 'INSERT INTO people (name) VALUES (?1)',
    params: [hostile],
  });
  assert.equal(written.ok, true, written.message);
  assert.equal(written.changes, 1);

  const rows = await query('SELECT name FROM people', { db });
  assert.equal(rows.length, 1, 'the table is gone, or holds something else');
  assert.equal(rows[0].name, hostile, 'the value did not survive as one string');
});

withDatabase('a failure comes back classified rather than thrown', async (db) => {
  const made = await inillucent('exec', {
    db,
    sql: 'CREATE TABLE people (id INTEGER PRIMARY KEY)',
  });
  assert.equal(made.ok, true, made.message);

  const missing = await inillucent('query', { db, sql: 'SELECT * FROM absent' });
  assert.equal(missing.ok, false);
  assert.equal(
    missing.status,
    'not_found',
    `a missing table came back as ${missing.status}`,
  );

  // `unsupported` is its own status and its own exit code, which is the thing
  // AGENTS.md asks a caller to branch on rather than reword their SQL over.
  const unbuilt = await inillucent('query', { db, sql: 'SELECT (SELECT 1, 2)' });
  assert.equal(unbuilt.ok, false);
  assert.equal(
    unbuilt.status,
    'unsupported',
    `a construct the engine has not built came back as ${unbuilt.status}`,
  );
});

withDatabase('a VECTOR column survives the wrapper', async (db) => {
  // The conformance suite gained a `vector` case for the driver and the C ABI;
  // this is the same question one layer out, because `vector` is one of the two
  // arguments the wrapper serialises as JSON rather than as a string.
  //
  // The value goes in as a blob literal of little-endian `f32` bits, which is
  // what a `VECTOR(N)` column is: three floats, twelve bytes. There is no
  // `vector_from_text`, and `'[1,0,0]'` is refused as "not a vector of 3
  // dimensions" - correctly, because it is a seven-character string.
  const made = await inillucent('batch', {
    db,
    sql: `
      CREATE TABLE point (id INTEGER PRIMARY KEY, at VECTOR(3));
      INSERT INTO point (id, at) VALUES (1, x'0000803f0000000000000000')
    `,
  });
  assert.equal(made.ok, true, made.message);

  const dimensions = await query('SELECT vector_dims(at) AS width FROM point', { db });
  assert.equal(dimensions[0].width, 3, 'the vector did not come back three wide');

  const found = await inillucent('vector-search', {
    db,
    table: 'point',
    column: 'at',
    vector: [1, 0, 0],
    k: 1,
  });
  assert.equal(found.ok, true, found.message);
  assert.ok(
    found.columns.some((column) => column.name === 'distance'),
    'vector-search answered without a distance column',
  );
});

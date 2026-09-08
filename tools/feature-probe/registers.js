// The completeness audit: does the *list* of features miss anything?
//
// `run.js` asks 416 questions somebody wrote down, and a feature nobody wrote a
// case for reads as "no gap". This asks a different question, and it asks
// SQLite rather than us: enumerate every function, pragma, module, collation
// and dot command **the reference itself reports**, then call every one of them
// in both engines and compare the whole answer.
//
// Three rules keep it meaningful, and each of them was learnt by getting it
// wrong on task-1861:
//
//   1. **The enumeration comes from SQLite, never from this repository.** A
//      list we maintain can only find what we already thought of.
//   2. **Every name is called, not just listed.** A register can under-report
//      in either direction: this engine omits lazily-registered modules from
//      `pragma_module_list` while answering them perfectly, and SQLite's shell
//      lists functions that its *library* does not have.
//   3. **A name SQLite only has in `shell.c` is not a library gap.** Before
//      calling anything missing, check the pinned amalgamation: `base64`,
//      `sha3`, `ieee754*`, `readfile` and friends are shell extensions, and an
//      application linking `sqlite3.h` never had them.
//
// A bare call is also the wrong way to ask about a context-scoped name. A
// window function and an FTS5 auxiliary function both refuse outside their
// context in *both* engines, so this script reports them as `context` rather
// than as missing, and `contextCases` below calls them properly.
//
// Usage:
//   node tools/feature-probe/registers.js

const { execFileSync } = require('child_process');
const fs = require('fs');
const path = require('path');
const { OURS, REF, OUT: PROBE_OUT } = require('./paths');

/** Where this run's databases and transcripts go. */
const OUT = path.join(PROBE_OUT, 'registers');

/** The enumerations to diff, each one a query SQLite answers about itself. */
const REGISTERS = [
  { id: 'function', sql: 'SELECT name FROM pragma_function_list ORDER BY name;' },
  { id: 'pragma', sql: 'SELECT name FROM pragma_pragma_list ORDER BY name;' },
  { id: 'module', sql: 'SELECT name FROM pragma_module_list ORDER BY name;' },
  { id: 'collation', sql: 'SELECT name FROM pragma_collation_list ORDER BY name;' },
];

/**
 * The names that legitimately refuse a bare call in both engines.
 *
 * A window function outside a frame and an FTS5 auxiliary function outside a
 * MATCH are errors in SQLite too, so the bare-call comparison says nothing
 * about whether they exist. `contextCases` is what actually decides them.
 */
const CONTEXT_ONLY = new Set([
  'row_number', 'rank', 'dense_rank', 'lag', 'lead', 'first_value', 'last_value',
  'nth_value', 'ntile', 'percent_rank', 'cume_dist',
  'bm25', 'highlight', 'snippet', 'matchinfo', 'offsets', 'optimize', 'match',
]);

/** The scripts that call the context-scoped names the way a caller would. */
const contextCases = [
  {
    id: 'window frames',
    sql: [
      'CREATE TABLE t(a INTEGER, b TEXT);',
      "INSERT INTO t VALUES (1,'x'),(2,'y'),(3,'z');",
      'SELECT row_number() OVER (ORDER BY a), rank() OVER (ORDER BY a), dense_rank() OVER (ORDER BY a) FROM t;',
      'SELECT lag(a) OVER (ORDER BY a), lead(a) OVER (ORDER BY a), first_value(a) OVER (ORDER BY a),',
      '       last_value(a) OVER (ORDER BY a), nth_value(a,2) OVER (ORDER BY a), ntile(2) OVER (ORDER BY a),',
      '       percent_rank() OVER (ORDER BY a), cume_dist() OVER (ORDER BY a) FROM t;',
    ].join('\n'),
  },
  {
    id: 'fts5 auxiliary functions',
    sql: [
      'CREATE VIRTUAL TABLE f USING fts5(body);',
      "INSERT INTO f(body) VALUES ('the quick brown fox'),('a slow brown dog');",
      "SELECT rowid, bm25(f) FROM f WHERE f MATCH 'brown' ORDER BY rowid;",
      "SELECT highlight(f,0,'[',']') FROM f WHERE f MATCH 'brown' ORDER BY rowid;",
      "SELECT snippet(f,0,'<','>','...',4) FROM f WHERE f MATCH 'fox';",
      "INSERT INTO f(f) VALUES ('optimize');",
    ].join('\n'),
  },
  {
    id: 'the modules the register omits',
    sql: [
      'CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT);',
      "INSERT INTO t VALUES (1,'hello world'),(2,'goodbye world');",
      "SELECT 'dbstat', count(*)>0 FROM dbstat;",
      "SELECT 'sqlite_dbpage', count(*)>0 FROM sqlite_dbpage;",
      "SELECT 'sqlite_stmt', count(*)>=0 FROM sqlite_stmt;",
      "SELECT 'bytecode', count(*)>0 FROM bytecode('SELECT 1');",
      "SELECT 'tables_used', count(*)>=0 FROM tables_used('SELECT a FROM t');",
      "SELECT 'completion', count(*)>0 FROM completion('SEL');",
      "SELECT 'generate_series', count(*) FROM generate_series(1,5);",
    ].join('\n'),
  },
  {
    id: 'fts3/4 modules and auxiliary functions',
    sql: [
      'CREATE VIRTUAL TABLE f4 USING fts4(body);',
      "INSERT INTO f4(body) VALUES ('alpha beta'),('beta gamma');",
      "SELECT 'matchinfo', typeof(matchinfo(f4)) FROM f4 WHERE f4 MATCH 'beta' LIMIT 1;",
      "SELECT 'offsets', typeof(offsets(f4)) FROM f4 WHERE f4 MATCH 'beta' LIMIT 1;",
      'CREATE VIRTUAL TABLE f4aux USING fts4aux(f4);',
      "SELECT 'fts4aux', count(*)>0 FROM f4aux;",
      "CREATE VIRTUAL TABLE tk USING fts3tokenize('simple');",
      "SELECT 'fts3tokenize', token FROM tk WHERE input='one two' ORDER BY token;",
    ].join('\n'),
  },
];

/**
 * Runs one SQL script through one shell over its own fresh database.
 *
 * Returns the whole of what the shell said, output and error together, because
 * a difference in an error is a difference.
 *
 * @param exe - the shell to run
 * @param db - the database path, created fresh by the caller
 * @param sql - the script
 */
function ask(exe, db, sql) {
  try {
    return execFileSync(exe, [db], { input: sql, encoding: 'utf8', stdio: ['pipe', 'pipe', 'pipe'] });
  } catch (e) {
    return (e.stdout || '') + (e.stderr || '');
  }
}

/** Normalises a transcript so the two shells' line endings are not a difference. */
const normalise = text => (text || '').replace(/\r\n/g, '\n').trim();

/** True when an answer says the name is not a function at all. */
const absent = text => /no such (function|module)/i.test(text);

/** Returns the sorted, de-duplicated names one register answers with. */
function namesFrom(exe, db, sql) {
  return [...new Set(normalise(ask(exe, db, sql)).split('\n').map(s => s.trim()).filter(Boolean))].sort();
}

/** Returns the dot commands a shell's own `.help` lists. */
function dotCommandsFrom(exe, db) {
  const help = normalise(ask(exe, db, '.help\n'));
  const found = help.split('\n').map(l => (l.match(/^(\.[a-zA-Z0-9_]+)/) || [])[1]).filter(Boolean);
  return [...new Set(found)].sort();
}

/** Prints one enumeration's diff and returns the two sides. */
function reportRegister(id, mine, theirs) {
  const missing = theirs.filter(n => !mine.includes(n));
  const extra = mine.filter(n => !theirs.includes(n));
  console.log(`\n## ${id}: SQLite ${theirs.length}, inillucent ${mine.length}`);
  if (missing.length) console.log(`  not listed here (${missing.length}): ${missing.join(' ')}`);
  if (extra.length) console.log(`  listed only here (${extra.length}): ${extra.join(' ')}`);
  if (!missing.length && !extra.length) console.log('  identical');
  return { missing, extra };
}

function main() {
  fs.mkdirSync(OUT, { recursive: true });
  const sqlite = REF;
  const shell = OURS;
  const db = n => path.join(OUT, n);

  const registers = {};
  for (const r of REGISTERS) {
    const theirs = namesFrom(sqlite, db(`s-${r.id}.db`), r.sql);
    const mine = namesFrom(shell, db(`i-${r.id}.rdb`), r.sql);
    registers[r.id] = reportRegister(r.id, mine, theirs);
    registers[r.id].sqlite = theirs;
    registers[r.id].inillucent = mine;
  }

  const theirDots = dotCommandsFrom(sqlite, db('s-dot.db'));
  const myDots = dotCommandsFrom(shell, db('i-dot.rdb'));
  registers.dot = reportRegister('dot commands', myDots, theirDots);

  // Every function name SQLite lists, called in both engines.
  console.log('\n## calling every function SQLite lists');
  const calls = [];
  for (const name of registers.function.sqlite) {
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(name)) continue; // `->` and `->>` are operators
    const sql = `SELECT ${name}();\n`;
    const theirs = normalise(ask(sqlite, db('call-s.db'), sql));
    const mine = normalise(ask(shell, db('call-i.rdb'), sql));
    calls.push({
      name,
      same: theirs === mine,
      context: CONTEXT_ONLY.has(name),
      missingHere: absent(mine) && !absent(theirs),
      sqlite: theirs,
      inillucent: mine,
    });
  }
  const same = calls.filter(c => c.same);
  const context = calls.filter(c => !c.same && c.context);
  const gaps = calls.filter(c => !c.same && !c.context && c.missingHere);
  const differ = calls.filter(c => !c.same && !c.context && !c.missingHere);
  console.log(`  called ${calls.length}`);
  console.log(`    identical                    ${same.length}`);
  console.log(`    context-scoped, see below    ${context.length}`);
  console.log(`    absent here                  ${gaps.length}  ${gaps.map(c => c.name).join(' ')}`);
  console.log(`    present, answering differently ${differ.length}  ${differ.map(c => c.name).join(' ')}`);
  console.log('  a name absent here may still be a shell-only extension: check whether');
  console.log('  .sqlite-ref/<v>/src/shell.c defines it and sqlite3.c does not before calling it a gap.');

  // The context-scoped names, called the way a caller would.
  console.log('\n## the context-scoped cases, called properly');
  const contexts = [];
  for (const c of contextCases) {
    const theirs = normalise(ask(sqlite, db(`ctx-s-${c.id.replace(/\W+/g, '-')}.db`), c.sql + '\n'));
    const mine = normalise(ask(shell, db(`ctx-i-${c.id.replace(/\W+/g, '-')}.rdb`), c.sql + '\n'));
    const verdict = theirs === mine ? 'same' : 'DIFFERS';
    console.log(`  ${verdict.padEnd(8)} ${c.id}`);
    if (verdict !== 'same') {
      console.log(`    sqlite:     ${theirs.split('\n').join('\n                ')}`);
      console.log(`    inillucent: ${mine.split('\n').join('\n                ')}`);
    }
    contexts.push({ id: c.id, same: theirs === mine, sqlite: theirs, inillucent: mine });
  }

  const results = { registers, calls, contexts };
  fs.writeFileSync(path.join(OUT, 'registers.json'), JSON.stringify(results, null, 1));
  console.log(`\nwritten to ${path.join(OUT, 'registers.json')}`);
}

main();

// Asks both engines every PRAGMA SQLite lists, and records what each answers.
//
// Three outcomes matter and they are different from each other: a value, an
// empty answer (the statement is accepted and says nothing, which an
// application reads as "no row" rather than as "unsupported"), and a refusal.
// The middle one is the one worth counting, because it is the only one a caller
// cannot tell from a legitimately empty result.

const { spawnSync } = require('child_process');
const fs = require('fs');
const path = require('path');
const { ROOT, OUT, OURS, REF } = require('./paths.js');

const AREA = path.join(OUT, 'pragma-area');

/**
 * Every pragma the reference lists, asked of the reference itself rather than
 * kept in a file here - a hand-maintained list would drift from the build it
 * claims to describe, and the whole point of this probe is that nothing in it
 * is written down twice.
 */
function referencePragmas() {
  fs.mkdirSync(OUT, { recursive: true });
  const result = spawnSync(REF, [':memory:'], {
    input: '.mode list\nSELECT name FROM pragma_pragma_list ORDER BY name;\n',
    encoding: 'utf8', timeout: 20000, windowsHide: true,
  });
  return (result.stdout || '').replace(/\r\n/g, '\n').split('\n').map((s) => s.trim()).filter(Boolean);
}

const NAMES = referencePragmas();

/** The setup every pragma is asked over, so table-shaped ones have something to describe. */
const SETUP = "CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER, b TEXT);\nINSERT INTO t VALUES (1,10,'p');\nCREATE INDEX ia ON t(a);\n";

/** The argument a pragma needs to answer at all, where it needs one. */
const ARGUMENT = {
  table_info: '(t)', table_xinfo: '(t)', index_list: '(t)', index_info: '(ia)', index_xinfo: '(ia)',
  foreign_key_list: '(t)', incremental_vacuum: '(1)', optimize: '', quick_check: '', integrity_check: '',
};

/**
 * Runs one pragma through one shell and returns its transcript.
 * @param program - the shell binary
 * @param dir - a directory of its own
 * @param name - the pragma name
 */
function ask(program, dir, name) {
  fs.mkdirSync(dir, { recursive: true });
  const argument = ARGUMENT[name] === undefined ? '' : ARGUMENT[name];
  const script = `${SETUP}PRAGMA ${name}${argument};\n`;
  const result = spawnSync(program, [path.join(dir, `${name}.db`)], {
    cwd: dir, input: script, encoding: 'utf8', timeout: 20000, windowsHide: true,
  });
  const text = ((result.stdout || '') + (result.stderr || '')).replace(/\r\n/g, '\n').trim();
  return text;
}

/** Classifies one transcript: a value, silence, or a refusal. */
function classify(text) {
  if (/(^|\n)(Parse error|Runtime error|Error)\b/.test(text)) return 'refused';
  if (text === '') return 'silent';
  return 'answers';
}

const rows = [];
const stamp = String(Date.now());
for (const name of NAMES) {
  const ours = ask(OURS, path.join(AREA, stamp, 'ours', name), name);
  const theirs = ask(REF, path.join(AREA, stamp, 'ref', name), name);
  rows.push({ name, ours: classify(ours), theirs: classify(theirs), oursText: ours.slice(0, 200), theirsText: theirs.slice(0, 200) });
}
fs.writeFileSync(path.join(OUT, 'pragmas.json'), JSON.stringify(rows, null, 2));

const tally = {};
for (const r of rows) tally[r.ours] = (tally[r.ours] || 0) + 1;
console.log(`${rows.length} pragmas SQLite lists`);
console.log(tally);
console.log('\nanswers in SQLite, silent here:');
console.log(rows.filter((r) => r.ours === 'silent' && r.theirs === 'answers').map((r) => r.name).join(' '));
console.log('\nanswers in SQLite, refused here:');
console.log(rows.filter((r) => r.ours === 'refused' && r.theirs === 'answers').map((r) => r.name).join(' '));
console.log('\nsilent in both (a pragma that answers nothing by design):');
console.log(rows.filter((r) => r.ours === 'silent' && r.theirs === 'silent').map((r) => r.name).join(' '));
console.log('\nanswered by both:');
console.log(rows.filter((r) => r.ours === 'answers' && r.theirs === 'answers').map((r) => r.name).join(' '));

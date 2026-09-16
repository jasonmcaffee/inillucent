// The side-by-side feature probe behind sqlite-feature-comparison.md (task-1858).
//
// Every case is a whole SQL script. It is run through `inillucent-shell` and
// through the pinned `sqlite3` 3.53.4, each over its own fresh database in its
// own directory, and every byte of stdout and stderr is compared. That is the
// same comparison `crates/inillucent-compat/tests/semantics.rs` makes, widened
// from the 110 constructs earlier reviews probed to the whole feature surface
// this document has to speak for.
//
// Usage: node run.js [--filter <substring>] [--out <path.json>]

const { spawnSync } = require('child_process');
const fs = require('fs');
const path = require('path');
const { ROOT, OUT, OURS, REF } = require('./paths.js');

const AREA = path.join(OUT, 'area');

const CASES = [...require('./cases.js'), ...require('./cases-extra.js')];

/**
 * Removes the noise that is about the shell rather than about the engine, so a
 * case where both engines refuse for the same reason counts as agreement.
 * SQLite's shell echoes the offending statement and underlines it after a parse
 * error; ours does not, and that difference is not a feature difference.
 * @param text - the combined stdout and stderr of one run
 */
function normalise(text) {
  const lines = text.replace(/\r\n/g, '\n').split('\n');
  const kept = [];
  for (const line of lines) {
    if (/^\s*\^-+\s*error here\s*$/.test(line)) {
      kept.pop();
      continue;
    }
    kept.push(line);
  }
  return kept.join('\n').trim();
}

/**
 * Runs one script through one shell over a database nothing else has touched.
 * @param program - the shell binary
 * @param dir - the directory the database and any ATTACHed file go in
 * @param script - the whole script, fed on standard input
 */
function run(program, dir, script, files) {
  fs.rmSync(dir, { recursive: true, force: true });
  fs.mkdirSync(dir, { recursive: true });
  for (const [name, body] of Object.entries(files || {})) fs.writeFileSync(path.join(dir, name), body);
  const result = spawnSync(program, [path.join(dir, 'probe.db')], {
    cwd: dir,
    input: script,
    encoding: 'utf8',
    timeout: 30000,
    windowsHide: true,
  });
  if (result.error && result.error.code === 'ETIMEDOUT') return { text: '<<TIMEOUT>>', code: -1 };
  return {
    text: withoutPath(normalise((result.stdout || '') + (result.stderr || '')), dir),
    code: result.status === null ? -1 : result.status,
  };
}

/**
 * Replaces the case directory with a placeholder. Each shell runs in its own
 * directory, so a transcript that prints the database path - PRAGMA
 * database_list does - would differ for a reason that is not a feature.
 * @param text - the transcript
 * @param dir - the directory this run used
 */
function withoutPath(text, dir) {
  const forward = dir.split('\\').join('/');
  const back = forward.split('/').join('\\');
  return text.split(forward).join('<dir>').split(back).join('<dir>');
}

/** True when the text carries an engine refusal rather than an answer. */
function isError(text) {
  return /(^|\n)(Parse error|Runtime error|Error)\b/.test(text) || text === '<<TIMEOUT>>';
}

/**
 * Decides what one case's two transcripts mean, in the vocabulary the
 * comparison document uses: agreement, a refusal only we make, an acceptance
 * only we make, or a different answer - which is the one that matters, because
 * it is the only outcome an application cannot see.
 * @param ours - our transcript
 * @param theirs - the reference transcript
 */
function verdict(ours, theirs) {
  if (ours.text === theirs.text) return 'same';
  const weFailed = isError(ours.text);
  const theyFailed = isError(theirs.text);
  if (weFailed && !theyFailed) return 'refused';
  if (!weFailed && theyFailed) return 'accepted';
  if (weFailed && theyFailed) return 'both-refuse-differently';
  return 'wrong-answer';
}

/**
 * Returns the commit this checkout is on, or null when that cannot be read.
 *
 * Null rather than a throw: a probe run inside an exported tarball with no
 * `.git` should still produce its numbers. What reads this decides what a
 * missing commit means, and `check.mjs` treats it as a result it cannot date.
 */
function headCommit() {
  const shown = spawnSync('git', ['-C', ROOT, 'rev-parse', 'HEAD'], { encoding: 'utf8' });
  const sha = (shown.stdout || '').trim();
  return /^[0-9a-f]{40}$/.test(sha) ? sha : null;
}

function main() {
  const args = process.argv.slice(2);
  const filterAt = args.indexOf('--filter');
  const filter = filterAt >= 0 ? args[filterAt + 1] : null;
  const outAt = args.indexOf('--out');
  const out = outAt >= 0 ? args[outAt + 1] : path.join(OUT, 'results.json');

  const selected = CASES.filter((c) => !filter || c.id.includes(filter) || c.area.includes(filter));
  const results = [];
  let n = 0;
  for (const c of selected) {
    const ours = run(OURS, path.join(AREA, c.id, 'ours'), c.sql, c.files);
    const theirs = c.oursOnly ? { text: ours.text, code: ours.code } : run(REF, path.join(AREA, c.id, 'ref'), c.sql, c.files);
    const v = c.oursOnly ? (isError(ours.text) ? 'refused' : 'ours-only') : verdict(ours, theirs);
    results.push({ id: c.id, area: c.area, feature: c.feature, sql: c.sql, ours: ours.text, theirs: theirs.text, verdict: v });
    n += 1;
    if (n % 20 === 0) process.stderr.write(`  ${n}/${selected.length}\n`);
  }
  fs.mkdirSync(path.dirname(out), { recursive: true });
  // **The result says which tree it measured (task-1969, 4.4).** It used to be a
  // bare array, and `tools/doc-facts/check.mjs` read it with no freshness check
  // of any kind - so a probe recorded a month and forty commits ago passed as
  // today's, and every published count that comes out of it (416 cases, 403 the
  // same) was being checked against a measurement of a different engine. The
  // file is gitignored, so nothing else could have noticed.
  fs.writeFileSync(out, JSON.stringify({ commit: headCommit(), recordedAt: new Date().toISOString(), cases: results }, null, 2));

  const tally = {};
  for (const r of results) tally[r.verdict] = (tally[r.verdict] || 0) + 1;
  console.log(`${results.length} cases`);
  for (const [k, v] of Object.entries(tally).sort((a, b) => b[1] - a[1])) console.log(`  ${k.padEnd(24)} ${v}`);
  console.log('');
  for (const r of results) {
    if (r.verdict !== 'same' && r.verdict !== 'ours-only') console.log(`${r.verdict.padEnd(24)} ${r.id}  (${r.feature})`);
  }
  console.log(`\nwritten to ${out}`);
}

main();

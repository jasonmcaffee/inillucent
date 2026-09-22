// The shared conformance suite, run through the npm wrapper.
//
// **One specification, four runners.** `drivers/conformance/suite.json` is what
// "the binding is correct" means, and until task-2036 only two things read it:
// the Rust driver and the Python binding. npm, Go and PHP each had a round trip
// of their own - three to eight cases, written by hand, agreeing with nothing.
// A binding graded against a suite it does not run is a binding graded against
// itself.
//
// **What this wrapper cannot do, and why that is written in the suite rather
// than here.** The npm package spawns the command line: every call is a
// process, so a transaction, a savepoint or a temporary table cannot outlive
// one step. That is the `session` capability, and `skipped_by.npm` in the suite
// names it with the reason. Five of the thirty-two cases need it; the other
// twenty-seven run.
//
// Rows *do* outlive a step - they are in the file - and so do bytes: the
// command line binds a blob as `{"blob":"<hex>"}` and renders one back as the
// text `x'<hex>'`. The first version of this runner assumed blobs were lost and
// skipped eleven more cases for a limitation that is not there.
//
// `tooling::every_binding_runs_the_whole_suite` reads
// `_agent_output/conformance/npm.json`, which this writes, and fails when a
// runner ran fewer cases than its capabilities allow.
//
// It needs a built binary, which `INILLUCENT_BIN` names. When there is none the
// suite skips with the command that provisions it.

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { test } from 'node:test';

import { resolveBinary } from './index.mjs';

/** This package's directory. */
const here = dirname(fileURLToPath(import.meta.url));

/** The workspace root, four directories up from this package. */
const root = resolve(here, '..', '..', '..');

/** Which runner this is, as the suite's `skipped_by` names it. */
const LANGUAGE = 'npm';

/** Whether a binary can be found at all, decided once. */
let binary = null;
try {
  binary = resolveBinary('inillucent');
} catch {
  console.error(
    'no inillucent binary: set INILLUCENT_BIN or put one on PATH ' +
      '(`cargo build --release -p inillucent-cli`); skipping',
  );
}

/** The suite, read once. */
const suite = JSON.parse(readFileSync(join(root, 'drivers', 'conformance', 'suite.json'), 'utf8'));

/** What this runner cannot do, out of the suite's own record of it. */
const lacks = new Set(suite.skipped_by?.[LANGUAGE]?.lacks ?? []);

/**
 * Reads a value out of the suite's one-key object form.
 *
 * @param described - the one-key object
 */
function valueOf(described) {
  if ('null' in described) return null;
  if ('int' in described) return described.int;
  if ('real' in described) return described.real;
  if ('text' in described) return described.text;
  if ('blob' in described) return { blob: hexOf(described.blob) };
  throw new Error(`${JSON.stringify(described)} names no value kind`);
}

/**
 * Renders a byte array as the lowercase hex the command line binds by.
 *
 * `--params` takes a blob as `{"blob":"<hex>"}`, which is how bytes reach a
 * wrapper that can only pass text on a command line.
 *
 * @param bytes - the byte array out of the suite
 */
function hexOf(bytes) {
  return bytes.map((byte) => byte.toString(16).padStart(2, '0')).join('');
}

/**
 * Compares an expected value to what the command line answered.
 *
 * The command line's JSON carries integers, reals, text and null directly, so
 * the comparison is on the value rather than on a rendering of it. An integer
 * past 2^53 arrives as a JavaScript number and loses precision, which is a
 * property of JSON rather than of this engine - so a large integer is compared
 * as text, which is exact.
 *
 * @param want - what the suite says
 * @param got - what came back
 */
function same(want, got) {
  if (want !== null && typeof want === 'object' && 'blob' in want) {
    // **The envelope, not the old `x'..'` string** (task-2066 §4.1.15). A blob
    // used to come back as text, so bytes could be written and not read, and
    // this comparison agreed with it - which is why the round-trip case passed
    // throughout. Both sides are compared as hexadecimal, which is exact and
    // does not depend on how either side spells a byte array.
    // The suite writes a blob as an array of bytes; a step that already wrote
    // it as hexadecimal is taken as it stands, so both spellings grade.
    const wanted = Array.isArray(want.blob) ? hexOf(want.blob) : String(want.blob).toLowerCase();
    if (got !== null && typeof got === 'object' && typeof got.blob === 'string') {
      return got.blob.toLowerCase() === wanted;
    }
    return false;
  }
  if (want === null || got === null) return want === got;
  if (typeof want === 'number' && Number.isInteger(want) && Math.abs(want) > Number.MAX_SAFE_INTEGER) {
    return String(want) === String(got);
  }
  return want === got;
}

/**
 * Runs one statement through the command line and reads its report.
 *
 * `exec` rather than `query`, because it answers both: a SELECT through it
 * carries rows and columns, and a write carries `changes`. One verb means one
 * shape to read.
 *
 * @param database - the file
 * @param sql - the statement
 * @param params - the values to bind, already converted
 * @param limit - how many rows to hand back, when the step names one
 */
function run(database, sql, params, limit) {
  // `query` rather than `exec` when the step names a limit, because `--limit`
  // is `query`'s and the exact `total` beside a cut result is what that case
  // is about. Everywhere else `exec` answers both reads and writes, which is
  // one shape to read rather than two.
  const verb = limit === undefined ? 'exec' : 'query';
  const args = ['--db', database, verb, sql, '--output', 'json'];
  if (limit !== undefined) {
    args.push('--limit', String(limit));
  }
  if (params.length > 0) {
    args.push('--params', JSON.stringify(params));
  }
  let printed;
  try {
    printed = execFileSync(binary, args, { encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 });
  } catch (failed) {
    // A refusal is an answer: the command prints its JSON report and exits
    // non-zero, and the report is what this reads.
    printed = failed.stdout ?? '';
    if (!printed.trim()) throw failed;
  }
  return JSON.parse(printed);
}

/**
 * Grades one case and returns what did not hold.
 *
 * @param database - the file
 * @param onecase - the case out of the suite
 */
function grade(database, onecase) {
  const wrong = [];
  for (const statement of onecase.setup ?? []) {
    const report = run(database, statement, [], undefined);
    if (!report.ok) {
      wrong.push(`${onecase.name}: the setup \`${statement}\` failed: ${report.message}`);
      return wrong;
    }
  }
  for (const step of onecase.steps ?? []) {
    const params = (step.params ?? []).map(valueOf);
    const report = run(database, step.sql, params, step.limit);

    if (step.status !== undefined) {
      if (report.ok) {
        wrong.push(`${onecase.name}: \`${step.sql}\` succeeded and should have failed with \`${step.status}\``);
        continue;
      }
      if (report.status !== step.status) {
        wrong.push(
          `${onecase.name}: \`${step.sql}\` failed with \`${report.status}\` and should have failed with \`${step.status}\``,
        );
      }
      if (step.message_contains && !(report.message ?? '').includes(step.message_contains)) {
        wrong.push(
          `${onecase.name}: \`${step.sql}\` said \`${report.message}\`, which does not contain \`${step.message_contains}\``,
        );
      }
      continue;
    }

    if (!report.ok) {
      wrong.push(`${onecase.name}: \`${step.sql}\` failed with \`${report.status}\`: ${report.message}`);
      continue;
    }
    if (step.columns !== undefined) {
      const names = (report.columns ?? []).map((column) => column.name);
      if (JSON.stringify(names) !== JSON.stringify(step.columns)) {
        wrong.push(`${onecase.name}: \`${step.sql}\` answered columns ${JSON.stringify(names)}, wanted ${JSON.stringify(step.columns)}`);
      }
    }
    if (step.rows !== undefined) {
      const rows = report.rows ?? [];
      if (rows.length !== step.rows.length) {
        wrong.push(`${onecase.name}: \`${step.sql}\` answered ${rows.length} rows, wanted ${step.rows.length}`);
      } else {
        step.rows.forEach((wantRow, at) => {
          wantRow.forEach((wantCell, column) => {
            const got = rows[at]?.[column];
            if (!same(valueOf(wantCell), got)) {
              wrong.push(
                `${onecase.name}: \`${step.sql}\`: row ${at} column ${column} is ${JSON.stringify(got)} and should be ${JSON.stringify(valueOf(wantCell))}`,
              );
            }
          });
        });
      }
    }
    if (step.more !== undefined && report.more !== step.more) {
      wrong.push(`${onecase.name}: \`${step.sql}\` reported more=${report.more}, wanted ${step.more}`);
    }
    if (step.total !== undefined && report.total !== step.total) {
      wrong.push(`${onecase.name}: \`${step.sql}\` reported total ${report.total}, wanted ${step.total}`);
    }
    if (step.affected !== undefined && step.affected !== null && report.changes !== step.affected) {
      wrong.push(`${onecase.name}: \`${step.sql}\` reported ${report.changes} changed, wanted ${step.affected}`);
    }
  }
  return wrong;
}

test('the shared conformance suite', { skip: binary === null }, () => {
  const ran = [];
  const skipped = [];
  const failures = [];

  for (const onecase of suite.cases) {
    const needs = onecase.needs ?? [];
    const missing = needs.filter((one) => lacks.has(one));
    if (missing.length > 0) {
      skipped.push({ name: onecase.name, group: onecase.group, needs: missing });
      continue;
    }
    const directory = mkdtempSync(join(tmpdir(), 'inillucent-conformance-'));
    try {
      // No `create`: the first setup statement makes the file, because a
      // write verb on a path that is not there creates it. One fewer process
      // per case, and one fewer difference between this runner and the others.
      const database = join(directory, 'case.rdb');
      failures.push(...grade(database, onecase));
      ran.push(onecase.name);
    } finally {
      rmSync(directory, { recursive: true, force: true });
    }
  }

  const record = join(root, '_agent_output', 'conformance');
  mkdirSync(record, { recursive: true });
  writeFileSync(
    join(record, `${LANGUAGE}.json`),
    `${JSON.stringify({ language: LANGUAGE, lacks: [...lacks], ran, skipped, failures }, null, 2)}\n`,
  );

  assert.ok(
    ran.length > 0,
    'no case ran, so this runner graded nothing at all',
  );
  assert.deepEqual(
    failures,
    [],
    `${failures.length} of the conformance suite's assertions did not hold through the npm wrapper:\n  ${failures.join('\n  ')}`,
  );
});

// inillucent, from Node.
//
// **This is not a database binding.** It is the four programs, plus one
// convenience for calling the command line and getting a typed result back.
// A real binding would go through the C ABI in
// `drivers/inillucent-driver-capi/include/inillucent_driver.h` with koffi or an
// N-API addon, and `drivers/README.md` says exactly how to write one - it is a
// worthwhile thing to have and it is not this.
//
// What this is for: a script or an agent harness that wants to run a query
// without shelling out by hand and parsing a table. Each call is a process, so
// it is the wrong tool for a loop over a million rows and the right one for the
// dozen calls a build script or a tool wrapper makes.

import { execFile } from 'node:child_process';
import { promisify } from 'node:util';

import { resolveBinary, PROGRAMS, platformPackage } from './resolve.mjs';

const run = promisify(execFile);

export { resolveBinary, PROGRAMS, platformPackage };

/**
 * Runs one inillucent command and returns its result object.
 *
 * The command line's `--output json` contract: `{ ok, command, columns, rows,
 * total, more, changes, last_insert_rowid, elapsed_ms, text }` on success, and
 * `{ ok: false, status, message, ... }` on failure. `status` is one of the
 * driver's fourteen names, and `unsupported` is its own - a construct the
 * engine has not built is not a syntax error and should not be handled as one.
 *
 * A failure is returned rather than thrown, because the interesting failures
 * here are answers: "no such table", "this construct is not built yet". Only a
 * broken invocation throws.
 *
 * @param command - the verb, such as "query" or "describe"
 * @param options - the named arguments the verb takes, plus `db`
 */
export async function inillucent(command, options = {}) {
  const { db, ...rest } = options;
  const args = [command, '--output', 'json'];
  if (db) {
    args.push('--db', db);
  }
  for (const [name, value] of Object.entries(rest)) {
    if (value === undefined || value === null) {
      continue;
    }
    if (value === true) {
      args.push(`--${name}`);
      continue;
    }
    if (value === false) {
      continue;
    }
    // An array is a JSON argument - `params` and `vector` are the two - and the
    // command line reads it as JSON, so it is serialised rather than joined.
    args.push(`--${name}`, Array.isArray(value) ? JSON.stringify(value) : String(value));
  }
  const binary = resolveBinary('inillucent');
  try {
    const { stdout } = await run(binary, args, { maxBuffer: 256 * 1024 * 1024 });
    return JSON.parse(stdout);
  } catch (why) {
    // A non-zero exit still prints the result object on standard output when
    // `--output json` was asked for, so the failure the caller wants is in
    // there. Only a truly broken invocation has nothing to parse.
    if (typeof why.stdout === 'string' && why.stdout.trim().startsWith('{')) {
      return JSON.parse(why.stdout);
    }
    throw new Error(
      `inillucent ${command} could not be run: ${why.stderr || why.message}`,
    );
  }
}

/**
 * Runs a query and returns its rows as objects keyed by column name.
 *
 * The shape most callers actually want. `inillucent()` is there for the ones
 * that need the counts, the timing or the failure class.
 *
 * @param sql - the statement
 * @param options - `db`, `params`, `limit`
 */
export async function query(sql, options = {}) {
  const result = await inillucent('query', { sql, ...options });
  if (!result.ok) {
    const error = new Error(result.message);
    error.status = result.status;
    error.feature = result.feature;
    throw error;
  }
  const names = result.columns.map((column) => column.name);
  return result.rows.map((row) => Object.fromEntries(names.map((name, at) => [name, row[at]])));
}

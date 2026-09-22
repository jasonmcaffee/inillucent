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

/**
 * Encodes one bound value as the JSON text the command line reads.
 *
 * **`JSON.stringify` loses three things a parameter can be (task-1979, D13 and
 * D16).** A NaN and an Infinity both become `null`, so a REAL column silently
 * stored NULL and nothing said so; `-0` becomes `0`, so the sign of negative
 * zero was gone before the value left Node; and a byte string has no JSON form
 * at all, so there was no way to bind a BLOB.
 *
 * What is written instead: a non-finite number throws here, where the caller
 * can see which value it was, rather than turning into a NULL nobody asked for.
 * Negative zero is written `-0.0`, which JSON's own grammar carries and the
 * command line's parser reads back as a negative zero. Bytes are written
 * `{"blob":"<hex>"}`.
 *
 * @param value - one element of `params`
 */
function encodeParam(value) {
  if (typeof value === 'number') {
    if (!Number.isFinite(value)) {
      throw new TypeError(
        `a parameter cannot be ${value}: SQL has no spelling for NaN or Infinity.`,
      );
    }
    return Object.is(value, -0) ? '-0.0' : JSON.stringify(value);
  }
  if (value instanceof Uint8Array) {
    const hex = Buffer.from(value).toString('hex');
    return JSON.stringify({ blob: hex });
  }
  if (Array.isArray(value)) {
    return `[${value.map(encodeParam).join(',')}]`;
  }
  return JSON.stringify(value === undefined ? null : value);
}

/**
 * Encodes the whole `params` array as JSON text.
 *
 * @param values - the bound values, in order
 */
function encodeParams(values) {
  return `[${values.map(encodeParam).join(',')}]`;
}

export { resolveBinary, PROGRAMS, platformPackage };

/**
 * Runs a program, writing `stdin` to it when there is any.
 *
 * `execFile` cannot write to standard input, so a call that has something to
 * write goes through `spawn` and the two are answered the same way: `{ stdout,
 * stderr }`, or a rejection carrying both plus the exit code, which is the
 * shape the caller below already handles.
 *
 * @param binary - the program to run
 * @param args - its command line
 * @param stdin - what to write to its standard input, or null
 */
async function spawnWith(binary, args, stdin) {
  if (stdin === null) {
    return run(binary, args, { maxBuffer: 256 * 1024 * 1024 });
  }
  const { spawn } = await import('node:child_process');
  return new Promise((resolve, reject) => {
    const child = spawn(binary, args, { stdio: ['pipe', 'pipe', 'pipe'] });
    let stdout = '';
    let stderr = '';
    child.stdout.on('data', (chunk) => {
      stdout += chunk;
    });
    child.stderr.on('data', (chunk) => {
      stderr += chunk;
    });
    child.on('error', reject);
    child.on('close', (code) => {
      if (code === 0) {
        resolve({ stdout, stderr });
        return;
      }
      const why = new Error(`inillucent exited ${code}`);
      why.stdout = stdout;
      why.stderr = stderr;
      why.code = code;
      reject(why);
    });
    child.stdin.on('error', () => {});
    child.stdin.end(stdin);
  });
}

/**
 * Runs one inillucent command and returns its result object.
 *
 * The command line's `--output json` contract: `{ ok, command, columns, rows,
 * total, more, changes, last_insert_rowid, elapsed_ms, text }` on success, and
 * `{ ok: false, status, message, ... }` on failure. `status` is one of the
 * driver's thirteen names, and `unsupported` is its own - a construct the
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
  let stdin = null;
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
    // **`params` travels on standard input (task-1979, D17).** A command line
    // has a length ceiling - about 32 KB on Windows - and a parameter past it
    // failed with an operating system error rather than with anything about
    // SQL. `--params-file -` has no such limit, and it is also what lets the
    // encoding above carry bytes and a negative zero.
    if (name === 'params' && Array.isArray(value)) {
      stdin = encodeParams(value);
      args.push('--params-file', '-');
      continue;
    }
    // Any other array is a JSON argument - `vector` is the one - and the
    // command line reads it as JSON, so it is serialised rather than joined.
    args.push(`--${name}`, Array.isArray(value) ? JSON.stringify(value) : String(value));
  }
  const binary = resolveBinary('inillucent');
  try {
    const { stdout } = await spawnWith(binary, args, stdin);
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
  return result.rows.map((row) =>
    Object.fromEntries(names.map((name, at) => [name, decodeValue(row[at])])),
  );
}

/**
 * Turns one cell of a result into the JavaScript value it stands for.
 *
 * **Bytes went in and could not come back** (task-2066 section 4.1.15). A blob
 * left as the string `"x'00ff'"`, typed `text` in the column list, so nothing
 * told it apart from a TEXT column holding that text - while `encodeParam`
 * above has always sent bytes as `{blob: "<hex>"}`. The two halves of the same
 * grammar now agree, and a `Uint8Array` read out of one query can be bound
 * straight into the next.
 *
 * Anything that is not an envelope is passed through, so a caller's own object
 * column - which arrives as text - is untouched.
 *
 * @param value - one cell, as `--output json` rendered it
 */
function decodeValue(value) {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    return value;
  }
  if (typeof value.blob === 'string') {
    return Uint8Array.from(Buffer.from(value.blob, 'hex'));
  }
  return value;
}

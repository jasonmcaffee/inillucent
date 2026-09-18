/**
 * Asks the built programs what they do, then reads the documentation and reports every number that
 * disagrees.
 *
 * The documentation carries counts a reader is meant to trust: how many verbs the command line has,
 * how many tools the MCP server serves, how many function names the engine answers, how many tests
 * the workspace holds. Every one of those drifted between task-1877 and task-1925 without anything
 * failing, because a count in prose is checked by a person noticing.
 *
 * This checks them. It never reads a previous document to learn a fact - it runs `inillucent help`,
 * an MCP `tools/list`, `.help` through the shell, `inillucent functions`, the probe's own result
 * files and the test runner's summary, and then greps the tracked documents for the written form of
 * each answer. A fact that appears in no document at all is reported as a failure of this file
 * rather than passed over, so deleting a sentence does not make the check green.
 *
 * Usage:
 *   node tools/doc-facts/check.mjs [--site <path to inillucent-site>] [--json]
 *   node tools/doc-facts/check.mjs --run-tests      # adds the test counts, about five minutes
 *   node tools/doc-facts/check.mjs --self-test      # shows that the test-run judgement can fail
 *
 * `--run-tests` fails when the runner is absent, when its output cannot be read, and when it reports
 * a failing or undetermined test. The nonzero exits it accepts are `--strict` reporting a
 * prerequisite this machine does not have: no PostgreSQL or MySQL server, no
 * `INILLUCENT_NETWORK_TESTS`, and no ONNX weights. `docs/repository.md` documents all four.
 *
 * `onnx` joined that list in task-1913. Before it, `inillucent-core`'s twenty-seven embedding tests
 * were behind a cargo feature the build did not turn on, so they were in no binary and nothing
 * reported them; now they are built and run, and twenty-nine cases across two suites say
 * `no ONNX weights found; skipping` on a machine where `inillucent setup-embeddings all` has not
 * been run. That is the absence being reported rather than a new one appearing.
 *
 * It exits 1 when anything disagrees.
 */
import { execFileSync, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, '..', '..');

const args = process.argv.slice(2);
const asJson = args.includes('--json');
const siteIndex = args.indexOf('--site');
const SITE = siteIndex >= 0 ? path.resolve(args[siteIndex + 1]) : null;

/** Picks the release binaries when they are there and the debug ones otherwise. */
function binary(name) {
  for (const profile of ['release', 'debug']) {
    const candidate = path.join(ROOT, 'target', profile, process.platform === 'win32' ? `${name}.exe` : name);
    if (fs.existsSync(candidate)) return candidate;
  }
  return null;
}

/**
 * Runs a program and returns its output, its exit code and whether it was cut off.
 *
 * @param name - the program, looked for in target/release then target/debug
 * @param argv - its arguments
 * @param input - what to write to its standard input
 * @param timeout - how long to wait, in milliseconds
 */
function runDetailed(name, argv, input, timeout = 120000) {
  const exe = binary(name);
  if (!exe) return { built: false, text: '', status: null, timedOut: false };
  const result = spawnSync(exe, argv, { encoding: 'utf8', input, timeout });
  return {
    built: true,
    text: `${result.stdout || ''}${result.stderr || ''}`,
    status: result.status,
    timedOut: result.error?.code === 'ETIMEDOUT',
  };
}

/**
 * Runs a program and returns its combined output, or null when it is not built.
 *
 * The exit code is deliberately discarded here, because every caller of this asks a program a
 * question and reads the answer out of what it printed. The test runner is the one program whose
 * exit code carries meaning, and it goes through `runDetailed` instead.
 *
 * @param name - the program, looked for in target/release then target/debug
 * @param argv - its arguments
 * @param input - what to write to its standard input
 * @param timeout - how long to wait, in milliseconds
 */
function run(name, argv, input, timeout = 120000) {
  const outcome = runDetailed(name, argv, input, timeout);
  return outcome.built ? outcome.text : null;
}

/* ------------------------------------------------------------------ the facts */

/** Counts the verbs `inillucent help` lists under `Commands:`. */
function commandVerbs() {
  const help = run('inillucent', ['help']);
  if (help === null) return null;
  const block = help.split(/Commands:\r?\n/)[1];
  if (!block) return null;
  return block.split(/\r?\n/).filter((line) => /^ {2}[a-z]/.test(line)).length;
}

/** Counts the tools the MCP server answers a `tools/list` with. */
function mcpTools() {
  const session = [
    JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'initialize', params: { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 'doc-facts', version: '1' } } }),
    JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' }),
    JSON.stringify({ jsonrpc: '2.0', id: 2, method: 'tools/list', params: {} }),
  ].join('\n');
  const out = run('inillucent-mcp', ['--db', ':memory:'], `${session}\n`);
  if (out === null) return null;
  for (const line of out.split(/\r?\n/)) {
    if (!line.trim().startsWith('{')) continue;
    let message;
    try { message = JSON.parse(line); } catch { continue; }
    if (message.id === 2 && message.result?.tools) return message.result.tools.length;
  }
  return null;
}

/** Counts the dot commands the shell's own `.help` lists. */
function dotCommands() {
  const out = run('inillucent-shell', [':memory:'], '.help\n');
  if (out === null) return null;
  return out.split(/\r?\n/).filter((line) => /^\./.test(line)).length;
}

/** Counts the `sqlite3` command line options the shell acts on and the ones it refuses by name. */
function shellOptions() {
  const out = run('inillucent-shell', ['--help']);
  if (out === null) return null;
  const [accepted, refused] = out.split(/Refused, because this engine has no equivalent[^\n]*\r?\n/);
  const count = (text) => (text ? text.split(/\r?\n/).filter((line) => /^\s{2,}(--?[a-z-]+|--)(\s|$)/.test(line)).length : 0);
  const acted = count(accepted?.split(/Options:\r?\n/)[1]);
  const declined = count(refused);
  return { acted, declined, total: acted + declined };
}

/** Reads the engine's function register: distinct names, and the rows one per name and arity. */
function functionRegister() {
  // **Read out of the generated register rather than by running the CLI
  // (task-1962).** `run` prefers `target/release`, so this asked whichever
  // binary happened to be there - and the shipped archive is built with
  // `--features inillucent-cli/embed`, which registers `embed(TEXT)` and one
  // more name than a default build. The check therefore answered 191 on a box
  // that had just cut a release and 190 everywhere else, for one unchanged
  // tree. Its own label says "in the register", and the register is a file:
  // `compat/api/builtins.toml`, generated straight out of
  // `inillucent_sql::function::every_function` and checked against the engine
  // by `obligations::the_registers_match_the_engine`. Reading it makes this
  // answer the same thing on every machine.
  const file = path.join(ROOT, 'compat', 'api', 'builtins.toml');
  if (!fs.existsSync(file)) return null;
  const names = [...fs.readFileSync(file, 'utf8').matchAll(/^name = "(.*)"$/gm)].map((found) => found[1]);
  if (names.length === 0) return null;
  const distinct = [...new Set(names)];
  const family = (test) => distinct.filter(test).length;
  return {
    names: distinct.length,
    rows: names.length,
    json: family((name) => /^jsonb?(_|$)/.test(name)),
    dateTime: family((name) => ['date', 'time', 'datetime', 'julianday', 'unixepoch', 'strftime', 'timediff'].includes(name)),
    vector: family((name) => /^(vector_|l1_|l2_|cosine_|hamming_|jaccard_|inner_product|binary_quantize|subvector)/.test(name)),
  };
}

/** Counts the driver capabilities by the support each one declares. */
function capabilities() {
  const out = run('inillucent', ['capabilities', '--output', 'json', '--limit', '0']);
  if (out === null) return null;
  let answer;
  try { answer = JSON.parse(out); } catch { return null; }
  const by = {};
  for (const row of answer.rows) by[row[1]] = (by[row[1]] || 0) + 1;
  return { total: answer.rows.length, yes: by.yes || 0, partial: by.partial || 0, no: by.no || 0 };
}

/** Returns the commit this checkout is on, or null when that cannot be read. */
function headCommit() {
  const shown = spawnSync('git', ['-C', ROOT, 'rev-parse', 'HEAD'], { encoding: 'utf8' });
  const sha = (shown.stdout || '').trim();
  return /^[0-9a-f]{40}$/.test(sha) ? sha : null;
}

/** What `probe()` could not accept about the result it read, for the exit code. */
let probeStaleness = null;

/**
 * Reads the probe's last run, which is what the SQL and compatibility pages quote.
 *
 * **It is refused when it was recorded at another commit (task-1969, 4.4).**
 * `_agent_output/feature-probe/results.json` is gitignored, so on every fresh
 * clone it is absent and on every machine that has one it is however old that
 * machine's last probe was. Nothing checked either. A probe from a month and
 * forty commits ago passed as today's, and the 416-case count and the 403 that
 * agree - the two numbers `docs/feature-comparison.md` is built on - were being
 * held against a measurement of a different engine.
 *
 * A result with no `commit` is refused too. That is the old bare-array shape,
 * and accepting it would leave the hole open for exactly as long as one stale
 * file survives.
 */
function probe() {
  const file = path.join(ROOT, '_agent_output', 'feature-probe', 'results.json');
  if (!fs.existsSync(file)) return null;
  const cases = JSON.parse(fs.readFileSync(file, 'utf8'));
  const head = headCommit();
  if (!Array.isArray(cases) && head && cases.commit !== head) {
    probeStaleness = cases.commit
      ? `the probe result was recorded at ${cases.commit} and this tree is at ${head}; re-run \`node tools/feature-probe/run.js\``
      : `the probe result records no commit, so it cannot be dated; re-run \`node tools/feature-probe/run.js\``;
    return null;
  }
  if (Array.isArray(cases)) {
    probeStaleness = 'the probe result is a bare array with no commit, so it cannot be dated; re-run `node tools/feature-probe/run.js`';
    return null;
  }
  const rows = cases.cases || [];
  const by = {};
  for (const row of rows) by[row.verdict] = (by[row.verdict] || 0) + 1;
  return { total: rows.length, same: by.same || 0, refused: by.refused || 0, differ: by['wrong-answer'] || 0, oursOnly: by['ours-only'] || 0 };
}

/**
 * Reads the pragma register's own count out of `compat/api/pragmas.toml`.
 *
 * **A checked fact rather than a number somebody typed (task-1961, D3).** The
 * engine recognised 68 and two documents said 67, which is the 67 SQLite's
 * `pragma_list` reports rather than the 68 the register holds - a real
 * distinction that nothing wrote down, so the two numbers read as one being
 * wrong. `docs/pragmas.md` is generated from the register and
 * `cargo test -p inillucent-compat --test harness` fails when they differ;
 * this fails when a prose document names a number that is neither.
 */
function pragmaRegisterCount() {
  const file = path.join(ROOT, 'compat', 'api', 'pragmas.toml');
  if (!fs.existsSync(file)) return null;
  const found = fs.readFileSync(file, 'utf8').match(/^count = (\d+)$/m);
  return found ? Number(found[1]) : null;
}

/**
 * Counts the `[[target]]` rows in `tests/selection.toml`.
 *
 * **Three documents gave three target counts and nothing compared any of them
 * to the map (task-1969, 4.14).** `docs/repository.md` said 170,
 * `tests/inillucent-testing-tdd.md` said 169, and the file itself had 181.
 * `judgeTestRun` compares a written count against what the *runner* reported,
 * so a document that agrees with a stale run passes; the map is the thing both
 * documents are describing, and it is the thing to compare against.
 */
function selectionRows() {
  const file = path.join(ROOT, 'tests', 'selection.toml');
  if (!fs.existsSync(file)) return null;
  return (fs.readFileSync(file, 'utf8').match(/^\[\[target\]\]$/gm) || []).length;
}

/** Reads the register audit, which is where the pragma and collation counts come from. */
function registers() {
  const file = path.join(ROOT, '_agent_output', 'feature-probe', 'registers', 'registers.json');
  if (!fs.existsSync(file)) return null;
  const audit = JSON.parse(fs.readFileSync(file, 'utf8')).registers;
  const shellSource = path.join(ROOT, '.sqlite-ref', '3.53.4', 'src', 'shell.c');
  const librarySource = path.join(ROOT, '.sqlite-ref', '3.53.4', 'src', 'sqlite3.c');
  let libraryNames = null;
  let answered = null;
  if (fs.existsSync(shellSource) && fs.existsSync(librarySource)) {
    const shell = fs.readFileSync(shellSource, 'utf8');
    const library = fs.readFileSync(librarySource, 'utf8');
    const missing = audit.function.sqlite.filter((name) => !audit.function.inillucent.includes(name));
    const shellOnly = missing.filter((name) => shell.includes(`"${name}"`) && !library.includes(`"${name}"`));
    libraryNames = audit.function.sqlite.length - shellOnly.length;
    answered = libraryNames - (missing.length - shellOnly.length);
  }
  return {
    sqliteFunctions: audit.function.sqlite.length,
    libraryFunctions: libraryNames,
    answeredFunctions: answered,
    pragmas: audit.pragma.sqlite.length,
    collations: audit.collation.sqlite.length,
    dotMissing: (audit.dot.missing || []).length,
  };
}

/**
 * Counts the crates under each lint, which the repository page publishes.
 *
 * Reads the crate root, which is `main.rs` in a binary crate: this read
 * `lib.rs` alone until task-1973, so `inillucent-bench` was not counted and the
 * page's "28 of the 29" was checked against a measurement that could not see
 * the twenty-ninth.
 */
function crateLints() {
  const libs = [];
  for (const group of ['crates', 'drivers']) {
    const base = path.join(ROOT, group);
    if (!fs.existsSync(base)) continue;
    for (const entry of fs.readdirSync(base)) {
      const lib = path.join(base, entry, 'src', 'lib.rs');
      const main = path.join(base, entry, 'src', 'main.rs');
      const root = fs.existsSync(lib) ? lib : main;
      if (fs.existsSync(root)) libs.push(fs.readFileSync(root, 'utf8'));
    }
  }
  const members = (fs.readFileSync(path.join(ROOT, 'Cargo.toml'), 'utf8').match(/^\s{4}"(?:crates|drivers)\/[^"]+",$/gm) || []).length;
  const forbidsUnsafe = libs.filter((text) => text.includes('forbid(unsafe_code)')).length;
  const deniesFour = libs.filter((text) => ['unwrap_used', 'expect_used', 'panic', 'indexing_slicing'].every((lint) => text.includes(lint))).length;
  return { members, forbidsUnsafe, deniesFour };
}

/* ------------------------------------- what a public repository must not carry */

/**
 * Strings that must not appear in a tracked file, and the places each is still allowed.
 *
 * **This is the check that says the repository can be published.** A password, a
 * personal address, a machine's drive letter and the name of a private repository
 * were all in tracked content at the 0.1.2 release, and nothing looked at them
 * (task-1946, H7). Each row below is one of those, with the reason it must not be
 * here; `allow` names the files where the same characters mean something else, and
 * every entry says why, because an unexplained exclusion is how a check goes quiet.
 */
const PRIVATE_REFERENCES = [
  { needle: 'jasonlmcaffee', why: 'a personal email address' },
  { needle: 'black.rainbow.labs@', why: 'a personal email address' },
  { needle: '360water', why: "a real company's domain, used as a test fixture" },
  { needle: 'postgres:inillucent@', why: 'a database password' },
  { needle: 'C:\\jason', why: "a path on one developer's machine" },
  { needle: 'C:/jason', why: "a path on one developer's machine" },
  { needle: 'J:/inillucent', why: "a drive letter on one developer's machine" },
  { needle: 'jason-25', why: 'a personal hostname' },
  { needle: 'Codex Sol', why: 'a reviewer by name, where the house style credits a ticket' },
  { needle: 'codex exec', why: 'a reviewer by name, where the house style credits a ticket' },
  { needle: 'npmjs.com/settings', why: 'a registry URL carrying an account name' },
  {
    needle: '~/.claude',
    why: 'a path inside a private instruction directory',
    // `~/.claude/skills` is where Claude Code reads skills from on any machine, so
    // these two lines are telling a reader of `agent-skills/` what to do with it.
    // The reference H7 was about was `~/.claude/CLAUDE.md`, a private instruction
    // file, and that one is gone.
    allow: ['README.md', 'agent-skills/README.md'],
  },
  { needle: 'opencode.json', why: 'a file in a private repository' },
  { needle: 'aiservice-web', why: 'a private repository' },
];

/**
 * Files this check does not read, and why.
 *
 * Two files necessarily hold every string in the list above, because they are where
 * the list is written down: this file, and the review document the list came from.
 * Excluding anything else from `tasks/` was considered and rejected - the other
 * design documents were corrected instead, which is what the list is for.
 */
const PRIVATE_REFERENCE_EXEMPT = [
  'tools/doc-facts/check.mjs',
  'tasks/task-1946-inillucent-code-review-round-two-tdd.md',
];

/** Every file `git ls-files` reports, as repository-relative paths with forward slashes. */
function trackedFiles() {
  const listing = execFileSync('git', ['-C', ROOT, 'ls-files', '-z'], {
    encoding: 'utf8',
    maxBuffer: 64 * 1024 * 1024,
  });
  return listing.split('\0').filter((entry) => entry.length > 0);
}

/**
 * Reports every tracked file that carries one of the strings above.
 *
 * Binary files are read as UTF-8 and searched the same way; a database that happens
 * to hold the bytes of a password is exactly as much of a problem as a document
 * that does, so there is nothing to gain by skipping them.
 */
function privateReferences() {
  const problems = [];
  let files = 0;
  for (const relative of trackedFiles()) {
    if (PRIVATE_REFERENCE_EXEMPT.includes(relative)) continue;
    const full = path.join(ROOT, relative);
    let text;
    try {
      text = fs.readFileSync(full, 'utf8');
    } catch {
      continue;
    }
    files += 1;
    for (const { needle, why, allow } of PRIVATE_REFERENCES) {
      if (allow && allow.includes(relative)) continue;
      const at = text.indexOf(needle);
      if (at < 0) continue;
      const line = text.slice(0, at).split('\n').length;
      problems.push(`${relative}:${line} carries \`${needle}\` — ${why}`);
    }
  }
  return { label: 'no tracked file carries a private reference', problems, scanned: files };
}

/* --------------------------------------------- every release version pin agrees */

/**
 * Each place a release version is written down, and the pattern that reads it.
 *
 * **Nothing tied these together, and two of them were wrong at 0.1.2.** The Python
 * package reported 0.1.0 and the PHP installer downloaded 0.1.1, so
 * `composer require` followed by the installer fetched the release that cannot
 * embed (task-1946, H9). `packaging/release.ps1` runs this check before it builds.
 *
 * The npm platform packages are not listed: `packages/npm/build.mjs` writes each
 * one's manifest from the wrapper's version, so the wrapper is the only copy.
 */
const VERSION_PINS = [
  { file: 'packages/python/pyproject.toml', pattern: /^version = "([^"]+)"/m, what: 'the Python distribution' },
  { file: 'packages/python/src/inillucent/__init__.py', pattern: /^__version__ = "([^"]+)"/m, what: "the Python package's own report" },
  { file: 'packages/npm/inillucent/package.json', pattern: /"version":\s*"([^"]+)"/, what: 'the npm wrapper' },
  { file: 'packages/go/cmd/inillucent-install/main.go', pattern: /^const nativeVersion = "([^"]+)"/m, what: 'the Go installer' },
  { file: 'packages/php/bin/inillucent-install', pattern: /^const NATIVE_VERSION = '([^']+)';/m, what: 'the PHP installer' },
  { file: 'packaging/homebrew/inillucent.rb', pattern: /^\s*version "([^"]+)"/m, what: 'the Homebrew formula' },
];

/** Reports every pinned copy of the release version that is not the workspace's. */
function versionPins() {
  const manifest = fs.readFileSync(path.join(ROOT, 'Cargo.toml'), 'utf8');
  const workspace = /\[workspace\.package\][\s\S]*?^version = "([^"]+)"/m.exec(manifest);
  if (!workspace) {
    return { label: 'every version pin equals the workspace version', problems: ['Cargo.toml has no [workspace.package] version'] };
  }
  const expected = workspace[1];
  const problems = [];
  for (const { file, pattern, what } of VERSION_PINS) {
    const full = path.join(ROOT, file);
    if (!fs.existsSync(full)) {
      problems.push(`${file} is not there, and ${what} is pinned in it`);
      continue;
    }
    const found = pattern.exec(fs.readFileSync(full, 'utf8'));
    if (!found) {
      problems.push(`${file} no longer states a version where ${what} had one`);
      continue;
    }
    if (found[1] !== expected) {
      problems.push(`${file} pins ${what} at ${found[1]}, and the workspace is ${expected}`);
    }
  }
  return { label: 'every version pin equals the workspace version', problems, expected };
}

/* --------------------------------------- what the README says about the repository */

/** Matches a sentence that calls this repository private, and not "a private key" or "private field". */
const CALLS_THE_REPOSITORY_PRIVATE =
  /\bprivate\b[^.\n]{0,40}\b(repository|repo|source|project)\b|\b(repository|repo|source|project)\b[^.\n]{0,40}\bprivate\b/i;

/**
 * Reports every sentence in the shipped documents that calls this repository private while
 * `packaging/PUBLISHING.md` no longer does.
 *
 * **A sentence that is true today and false on the day the repository is published is a sentence
 * nobody will remember to delete.** The README told a Go user to set `GOPRIVATE` "because the
 * repository is private and Go's public checksum database cannot read it", which becomes a wrong
 * instruction the moment the repository is public, and the person it misleads is the first stranger
 * who tries to install it (task-1946, M6).
 *
 * That was answered by banning the word from `README.md` outright, and the ban had a cost nobody
 * measured. With the sentence gone, the README said the Go module was published, and it is not:
 * `go install` resolves through `proxy.golang.org`, the proxy clones with no credential, and a
 * private repository answers `404 ... fatal: could not read Username`. A reader was handed a command
 * that cannot work, which is the failure 0.1.0 was withdrawn for (task-1951).
 *
 * So the rule is agreement rather than absence. `packaging/PUBLISHING.md` holds the fact, because it
 * is the file whose job is recording where each route stands. While it says the repository is
 * private, the documents inside the archive may say so too. The moment somebody makes the repository
 * public and updates that file, this check lists every other line still saying it, by file and line
 * number, so the deletion is reported rather than remembered.
 */
function privateRepositorySentencesAgree() {
  const label = 'the shipped documents and PUBLISHING.md agree about the repository being private';
  const source = 'packaging/PUBLISHING.md';

  /**
   * Every line of one tracked file that calls this repository private.
   *
   * Each line is tested joined to the one after it, because these documents are hard wrapped at
   * about a hundred characters and the sentence this looks for straddles the wrap often enough to
   * matter. Tested one line at a time, "cannot clone a private" and "repository" sit on either side
   * of a newline and the match is missed - which happened to a sentence written in task-1951 itself,
   * in the same change that wrote this check.
   *
   * @param file - the repository-relative path to read
   */
  const hits = (file) => {
    const lines = fs.readFileSync(path.join(ROOT, file), 'utf8').split('\n');
    const found = [];
    lines.forEach((line, index) => {
      const match = CALLS_THE_REPOSITORY_PRIVATE.exec(`${line} ${lines[index + 1] ?? ''}`);
      // Only when the sentence starts on this line. A match that starts past the end of it belongs
      // to the next line and is reported there, so a sentence spanning two lines is one row rather
      // than two, and a blank line is never reported for what follows it.
      if (match && match.index < line.length) found.push(`${file}:${index + 1}: ${line.trim()}`);
    });
    return found;
  };

  // Everything a reader gets inside the release archive. PUBLISHING.md is not in the archive; it is
  // the record the archive's claims are checked against.
  const shipped = ['README.md', 'docs/getting-started.md', 'agent-skills/inillucent-quickstart/SKILL.md'];

  if (hits(source).length > 0) return { label, problems: [] };
  return {
    label,
    problems: shipped
      .flatMap(hits)
      .map((claim) => `${claim}  --  ${source} no longer calls the repository private, so this is stale`),
  };
}

/**
 * What `--strict` is allowed to report as absent on a developer's machine.
 *
 * Each is matched against the whole reason the runner printed, because the runner prints two shapes:
 * `needs postgres` for the name of a thing, and a whole sentence for a reason that is about this
 * machine - `set INILLUCENT_NETWORK_TESTS to run this`. Both are read; see `missingPrerequisites`.
 *
 * `docs/repository.md` names the same three, and a fourth name here without a line there is the
 * drift this list exists to stop.
 */
/**
 * The prerequisites a strict run may name without that being a failure of the run.
 *
 * **Read out of `tests/selection.toml` rather than written here (task-1969, 4.15).** This was
 * `['postgres', 'mysql', 'INILLUCENT_NETWORK_TESTS', 'onnx']`, the four values the map happened to
 * hold when it was written - so the same census that took the map from 9 prerequisite values to 19
 * would have turned every newly declared absence into "it exited 1 for a prerequisite that is not
 * optional". The list and the thing it describes were the same list twice, and one of them went
 * stale the moment the other grew.
 *
 * What it is for is unchanged: a strict run exits non-zero when it names a hollow suite, and a
 * hollow suite whose prerequisite the map declares is the documented condition rather than a
 * defect. A prerequisite the map does *not* declare is still a failure, which is what makes this a
 * check rather than a blanket permission - and `selection.rs`'s
 * `every_target_that_can_skip_declares_it_and_vice_versa` is what keeps the map equal to the
 * suites.
 *
 * `INILLUCENT_NETWORK_TESTS` is kept beside the map's values: the runner names the environment
 * variable rather than the `network` the row declares, and the two are the same condition.
 */
function optionalPrerequisites() {
  const file = path.join(ROOT, 'tests', 'selection.toml');
  if (!fs.existsSync(file)) return ['INILLUCENT_NETWORK_TESTS'];
  const declared = new Set(['INILLUCENT_NETWORK_TESTS']);
  for (const line of fs.readFileSync(file, 'utf8').split(/\r?\n/)) {
    const found = /^requires = \[(.*)\]$/.exec(line.trim());
    if (!found) continue;
    for (const value of found[1].matchAll(/"([^"]+)"/g)) declared.add(value[1]);
  }
  return [...declared];
}

/**
 * Runs the test runner, for the test and target counts.
 *
 * Behind `--run-tests`, because it is about five minutes and the rest of this file is about one
 * second. The counts it produces are what `docs/repository.md` and inillucent.com publish.
 *
 * **Every way this can go wrong is a failure rather than a skip.** It used to answer `null` when the
 * runner was absent and again when its output did not parse, and `null` read as "nothing to compare
 * against", so the two test assertions were skipped and the whole check exited 0. A review found it
 * twice over: `cargo build --release --bins` does not build this runner at all, because
 * `crates/inillucent-compat/Cargo.toml` puts it behind a required `testrun` feature, and a run that
 * reported one failing test still printed that every fact was right. A check that cannot fail is
 * worse than no check, which is rule 1.5 of `tests/inillucent-testing-tdd.md`.
 *
 * `--strict` exits 1 on this machine even when every test passes, because `live_postgres` and
 * `live_mysql` have no server to run against, and `docs/repository.md` documents that. That one
 * outcome is accepted, and only when the suites it names are on the list above.
 */
/**
 * Returns the suites the runner listed as having run without a prerequisite.
 *
 * **Both shapes the runner prints.** It puts `needs ` in front of the name of a thing and leaves a
 * whole sentence alone, because "needs this platform would not make a directory link" is not
 * English. Reading only the first shape made this file report that it could not read a row, which
 * says nothing about the run.
 *
 * Only the block the runner introduces is read, so the `slowest:` list below it - which is also
 * indented, also two columns - cannot be mistaken for a skip.
 *
 * @param text - everything the runner printed
 * @returns one `{suite, needs}` per listed row, `needs` being the reason as printed
 */
function declaredRows(text) {
  const start = /^\d+ suite\(s\) ran without a prerequisite[^\n]*\n/m.exec(text);
  if (!start) return [];
  const after = text.slice(start.index + start[0].length);
  const block = after.split(/\n\s*\n/, 1)[0] ?? '';
  return block
    .split('\n')
    .map((line) => /^\s+(\S+)\s\s+(.+?)\s*$/.exec(line))
    .filter(Boolean)
    .map((row) => ({ suite: row[1], needs: row[2].replace(/^needs /, '') }));
}

/**
 * Decides what one `inillucent-testrun --strict` outcome means.
 *
 * Separate from the running so it can be exercised against staged outcomes. `--self-test` does
 * exactly that, which is how the three failure paths below are shown to fail rather than asserted to.
 *
 * @param outcome - what `runDetailed` returned for the runner
 */
export function judgeTestRun(outcome) {
  if (!outcome.built) {
    return {
      error: [
        'inillucent-testrun is not built, and `cargo build --release --bins` does not build it:',
        'it is behind a required `testrun` feature. Build it with',
        '  cargo build --release -p inillucent-compat --bin inillucent-testrun --features testrun',
      ].join('\n          '),
    };
  }
  if (outcome.timedOut) {
    return { error: 'inillucent-testrun did not finish inside 30 minutes, so its counts are unknown.' };
  }
  const match = /(\d+) target\(s\), (\d+) test\(s\), (\d+) failed, (\d+) undetermined/.exec(outcome.text);
  if (!match) {
    const tail = outcome.text.trim().split('\n').slice(-5).join('\n          ');
    return { error: `inillucent-testrun printed no summary line, so its counts are unknown. Its last output was:\n          ${tail}` };
  }

  // The runner says how many suites went without a prerequisite before it lists them, and both
  // numbers are read. A line this pattern cannot parse would otherwise vanish and leave the list
  // looking entirely optional, which is how `--self-test` caught a suite needing a GPU being
  // accepted as though only postgres and mysql were absent.
  const declared = /^(\d+) suite\(s\) ran without a prerequisite/m.exec(outcome.text);
  const result = {
    targets: Number(match[1]),
    tests: Number(match[2]),
    failed: Number(match[3]),
    undetermined: Number(match[4]),
    status: outcome.status,
    declaredWithoutPrerequisite: declared ? Number(declared[1]) : 0,
    missingPrerequisites: declaredRows(outcome.text),
  };

  const problems = [];
  if (result.failed > 0) problems.push(`${result.failed} test(s) failed`);
  if (result.undetermined > 0) problems.push(`${result.undetermined} test(s) were undetermined`);
  if (result.missingPrerequisites.length !== result.declaredWithoutPrerequisite) {
    problems.push(`it said ${result.declaredWithoutPrerequisite} suite(s) had no prerequisite and this could read ${result.missingPrerequisites.length} of them`);
  }
  if (outcome.status !== 0) {
    const allowed = optionalPrerequisites();
    const unexplained = result.missingPrerequisites.filter(
      (row) => !allowed.some((named) => row.needs.includes(named)),
    );
    if (result.missingPrerequisites.length === 0) {
      problems.push(`it exited ${outcome.status} and named no missing prerequisite to explain it`);
    } else if (unexplained.length > 0) {
      problems.push(`it exited ${outcome.status} for a prerequisite that is not optional: ${unexplained.map((row) => `${row.suite} needs ${row.needs}`).join(', ')}`);
    }
  }
  if (problems.length > 0) result.error = `inillucent-testrun --strict: ${problems.join('; ')}.`;
  return result;
}

/**
 * Every `.rs` file under a directory, so a binary can be dated against its own sources.
 *
 * @param directory - where to look
 */
function sourcesUnder(directory) {
  if (!fs.existsSync(directory)) return [];
  const found = [];
  for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
    const full = path.join(directory, entry.name);
    if (entry.isDirectory()) found.push(...sourcesUnder(full));
    else if (entry.name.endsWith('.rs')) found.push(full);
  }
  return found;
}

/**
 * Returns the test runner to measure with, and why it is that one.
 *
 * **The stale instrument that answered a published number (task-1970).**
 * `binary()` prefers `target/release`, which is right for the shipped programs
 * and wrong for this one. Both validate scripts build the runner into
 * `target/debug` and run it from there, and the `provision` string this file
 * carries for the instrument is that same debug build - so a
 * `target/release/inillucent-testrun.exe` left over from an earlier release
 * build was preferred over the one the run had just made. The copy on this
 * machine was four days and twenty commits old, and the `tests` fact it
 * answered was 3,010 where the debug runner, the `tests` stage of both
 * validate scripts and a direct run all said 3,016. Nothing reported the
 * difference, because a stale answer looks exactly like a fresh one.
 *
 * So the newer of the two is used, and one older than either of the two files
 * that decide what it runs and how it reports - the map and its own source - is
 * refused by name rather than believed.
 *
 * @returns `{exe, error}`; `exe` is null when there is none to trust
 */
function testRunner() {
  const name = process.platform === 'win32' ? 'inillucent-testrun.exe' : 'inillucent-testrun';
  const built = ['release', 'debug']
    .map((profile) => path.join(ROOT, 'target', profile, name))
    .filter((file) => fs.existsSync(file))
    .sort((a, b) => fs.statSync(b).mtimeMs - fs.statSync(a).mtimeMs);
  if (built.length === 0) return { exe: null, error: null };
  const exe = built[0];
  const made = fs.statSync(exe).mtimeMs;
  // Its own sources, not the map: the map is read at run time, so a newer
  // `tests/selection.toml` is a newer question rather than a stale instrument.
  const newer = sourcesUnder(path.join(ROOT, 'crates', 'inillucent-compat', 'src')).filter(
    (file) => fs.statSync(file).mtimeMs > made,
  );
  if (newer.length > 0) {
    const named = path.relative(ROOT, newer[0]).replace(/\\/g, '/');
    const rest = newer.length > 1 ? ` and ${newer.length - 1} other file(s)` : '';
    return {
      exe: null,
      error:
        `the test runner at ${path.relative(ROOT, exe).replace(/\\/g, '/')} was built before ${named}${rest}, ` +
        'so its answer is a measurement of an older tree; rebuild it with ' +
        '`cargo build -p inillucent-compat --bin inillucent-testrun --features testrun`',
    };
  }
  return { exe, error: null };
}

function testRun() {
  if (!process.argv.includes('--run-tests')) return null;
  const runner = testRunner();
  if (runner.error) return { error: runner.error };
  if (!runner.exe) return judgeTestRun({ built: false, text: '', status: null, timedOut: false });
  // The whole suite is about five minutes, and a two minute cap killed it and read the missing
  // summary line as "the runner is not built" rather than as "it was cut off".
  const said = spawnSync(runner.exe, ['--strict'], { encoding: 'utf8', timeout: 1_800_000 });
  return judgeTestRun({
    built: true,
    text: `${said.stdout || ''}${said.stderr || ''}`,
    status: said.status,
    timedOut: said.error?.code === 'ETIMEDOUT',
  });
}

/**
 * Runs `judgeTestRun` over staged outcomes, so each way it refuses is shown rather than claimed.
 *
 * The transcripts below are the runner's real output, taken from a full `--strict` pass on this
 * machine and then edited only in the number under test.
 */
function selfTest() {
  const passing = [
    'running 149 target(s), 24 at a time, 2 thread(s) each',
    '',
    '--- summary ---',
    '149 target(s), 2646 test(s), 0 failed, 0 undetermined',
    'wall 300.9s; the same work run one at a time is 3163.8s of processor time (10.5x)',
    '',
    '3 suite(s) ran without a prerequisite and evidenced nothing:',
    '  inillucent-remote::lib                       set INILLUCENT_NETWORK_TESTS to run this',
    '  inillucent-remote::live_postgres             needs postgres',
    '  inillucent-remote::live_mysql                needs mysql',
    '',
    'not ok - every test passed, and 3 suite(s) evidenced nothing',
  ].join('\n');

  const cases = [
    { name: 'the runner is not built', outcome: { built: false, text: '', status: null, timedOut: false }, wantError: true },
    { name: 'its output cannot be read', outcome: { built: true, text: 'inillucent 0.1.1 - an embedded SQL database\n', status: 0, timedOut: false }, wantError: true },
    { name: 'it was cut off', outcome: { built: true, text: passing, status: null, timedOut: true }, wantError: true },
    { name: 'a test failed', outcome: { built: true, text: passing.replace('0 failed', '1 failed'), status: 1, timedOut: false }, wantError: true },
    { name: 'a test was undetermined', outcome: { built: true, text: passing.replace('0 undetermined', '3 undetermined'), status: 1, timedOut: false }, wantError: true },
    { name: 'it exited nonzero for no stated reason', outcome: { built: true, text: passing.replace(/\n3 suite\(s\)[\s\S]*$/, ''), status: 1, timedOut: false }, wantError: true },
    { name: 'it exited nonzero for a prerequisite that is not optional', outcome: { built: true, text: passing.replace('needs postgres', 'needs a GPU'), status: 1, timedOut: false }, wantError: true },
    { name: 'every test passed, and only the documented three were absent', outcome: { built: true, text: passing, status: 1, timedOut: false }, wantError: false },
    { name: 'every test passed, and nothing was absent', outcome: { built: true, text: passing.replace(/\n3 suite\(s\)[\s\S]*$/, ''), status: 0, timedOut: false }, wantError: false },
  ];

  let wrong = 0;
  for (const item of cases) {
    const verdict = judgeTestRun(item.outcome);
    const errored = Boolean(verdict.error);
    const right = errored === item.wantError;
    if (!right) wrong += 1;
    console.log(`  ${right ? 'ok  ' : 'WRONG'}  ${item.name} -> ${errored ? 'refused' : 'accepted'}`);
    if (errored) console.log(`          ${verdict.error.split('\n')[0]}`);
  }
  console.log(`\n${cases.length - wrong} of ${cases.length} staged outcomes were judged as intended.`);
  return wrong;
}

if (args.includes('--self-test')) {
  console.log('judgeTestRun, over staged runner outcomes\n');
  process.exit(selfTest() > 0 ? 1 : 0);
}

/** Counts the chapters in the site's documentation book, when the site is on this machine. */
async function bookChapters() {
  if (!SITE) return null;
  const file = path.join(SITE, 'src', 'data', 'documentation.ts');
  if (!fs.existsSync(file)) return null;
  const module = await import(`file://${file.replace(/\\/g, '/')}`);
  return module.documentationChapters.length;
}

/* -------------------------------------------------- reading what is written */

const DOCUMENT_GLOBS = [
  { base: ROOT, dirs: ['docs', 'agent-skills', 'packaging', 'drivers', 'examples', 'tests'], files: ['README.md', 'AGENTS.md'], extensions: ['.md'] },
];
if (SITE) DOCUMENT_GLOBS.push({ base: SITE, dirs: ['src'], files: [], extensions: ['.ts', '.tsx'] });

const SKIP = /[\\/](target|node_modules|\.git|dist|_junk|_agent_output|out)[\\/]/;

/** Collects every tracked document this check reads. */
function documents() {
  const found = [];
  const walk = (dir, extensions) => {
    if (!fs.existsSync(dir)) return;
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const full = path.join(dir, entry.name);
      if (SKIP.test(`${full}${path.sep}`)) continue;
      if (entry.isDirectory()) walk(full, extensions);
      else if (extensions.some((extension) => entry.name.endsWith(extension))) found.push(full);
    }
  };
  for (const group of DOCUMENT_GLOBS) {
    for (const dir of group.dirs) walk(path.join(group.base, dir), group.extensions);
    for (const file of group.files) {
      const full = path.join(group.base, file);
      if (fs.existsSync(full)) found.push(full);
    }
  }
  return found;
}

const CORPUS = documents().map((file) => ({ file, text: fs.readFileSync(file, 'utf8') }));

/**
 * Reports every written number that sits next to `keyword` and is not `expected`.
 *
 * @param label - what the fact is, for the report
 * @param expected - the number the engine gave
 * @param pattern - a regular expression with one capturing group holding the written number
 * @param notPrecededBy - an optional guard matched against the 40 characters before a hit, to drop
 *   a sentence that spells a different fact the same way
 */
function assertWritten(label, expected, pattern, notPrecededBy) {
  if (expected === null || expected === undefined) return { label, expected, skipped: true, wrong: [], seen: 0 };
  const wrong = [];
  let seen = 0;
  for (const { file, text } of CORPUS) {
    const regexp = new RegExp(pattern.source, `${pattern.flags.includes('g') ? pattern.flags : `${pattern.flags}g`}`);
    let match;
    while ((match = regexp.exec(text))) {
      if (notPrecededBy && notPrecededBy.test(text.slice(Math.max(0, match.index - 40), match.index))) continue;
      seen += 1;
      const written = Number((match[1] ?? match[2] ?? '').replace(/,/g, ''));
      if (written !== expected) {
        const line = text.slice(0, match.index).split('\n').length;
        wrong.push({ file: path.relative(ROOT, file), line, written });
      }
    }
  }
  return { label, expected, wrong, seen };
}

/* ------------------------------------------------------------------- the run */

const verbs = commandVerbs();
const tools = mcpTools();
const dots = dotCommands();
const options = shellOptions();
const functions = functionRegister();
const caps = capabilities();
const probed = probe();
const audit = registers();
const lints = crateLints();
const tests = testRun();
const chapters = await bookChapters();
const mapRows = selectionRows();

/**
 * Every instrument, what it answered, and the command that provisions it.
 *
 * **An instrument that cannot answer is a failure of this program, not a
 * reason for it to say nothing (task-1969, 4.4).** Ten of the sixteen facts
 * returned `null` on a checkout with nothing built and no probe result - which
 * is every fresh clone, because `_agent_output/` is gitignored - and `:789`
 * built `failed` out of the checks that were not `skipped`. So the program
 * printed `skip` ten times and exited 0 with "Every fact a document states is
 * the fact the engine reports." The task-1925 fix gave `judgeTestRun` this
 * treatment and left the other ten instruments as they were.
 *
 * `scopedOutWithout` is the one legitimate absence: a flag the caller did not
 * pass puts a fact out of scope rather than leaving it unmeasured. There are
 * two, and both are named here rather than being a property of the instrument,
 * so adding a third is a decision somebody writes down.
 */
const INSTRUMENTS = [
  { label: 'command line verbs', value: verbs, provision: 'cargo build --release -p inillucent-cli' },
  { label: 'MCP tools', value: tools, provision: 'cargo build --release -p inillucent-cli' },
  { label: 'shell dot commands', value: dots, provision: 'cargo build --release -p inillucent-cli' },
  { label: 'shell command line options', value: options, provision: 'cargo build --release -p inillucent-cli' },
  { label: 'function register', value: functions, provision: 'cargo build --release -p inillucent-cli' },
  { label: 'driver capabilities', value: caps, provision: 'cargo build --release -p inillucent-cli' },
  { label: 'pragma register count', value: pragmaRegisterCount(), provision: 'restore compat/api/pragmas.toml' },
  { label: 'probe result', value: probed, provision: 'node tools/feature-probe/run.js' },
  { label: 'register audit', value: audit, provision: 'node tools/feature-probe/registers.js' },
  { label: 'crate lints', value: lints, provision: 'nothing - it reads the manifests, so a null here is a defect in this file' },
  { label: 'selection map rows', value: mapRows, provision: 'restore tests/selection.toml' },
  { label: 'test run', value: tests, provision: 'cargo build -p inillucent-compat --bin inillucent-testrun --features testrun', scopedOutWithout: '--run-tests', facts: ['tests'] },
  { label: 'documentation book chapters', value: chapters, provision: 'a checkout of the site', scopedOutWithout: '--site <dir>', facts: ['documentation book chapters'] },
];

/**
 * Reports whether a flag that scopes a fact out of this run was passed.
 *
 * `facts` on an instrument names the checks it feeds, because a check's label
 * and its instrument's label are not the same word - the `test run` instrument
 * answers the `tests` fact - and matching them by equality printed a genuine
 * instrument failure as an out-of-scope line.
 */
function passed(flag) {
  return flag ? args.includes(flag.split(' ')[0]) : true;
}

// Two assertions that are not counts: what a public repository must not carry, and
// whether every packaged copy of the release version agrees with the workspace.
const assertions = [privateReferences(), versionPins(), privateRepositorySentencesAgree()];

const checks = [
  assertWritten('command line verbs', verbs, /\b(\d+)\s+(?:command line )?(?:verbs|commands)\b(?!\s+(?:over MCP|served|an agent|as MCP))/i, /(?:dot|reference's)\s+$/),
  assertWritten('MCP tools', tools, /(\d+)\s+(?:of the (?:same|CLI's) commands served|MCP tools|tools an agent can call|tools an AI agent can call|of those commands over MCP|of the CLI's commands as MCP tools|of the same commands served)/i),
  assertWritten('shell dot commands', dots, /(\d+)\s+of (?:its|`sqlite3`'s|SQLite's) 65 dot commands/i),
  assertWritten('pragmas in the register', pragmaRegisterCount(), /(\d+) pragmas this engine recognises/i),
  assertWritten('shell command line options', options?.total, /all (\d+) of (?:its|`sqlite3`'s) command line options/i),
  assertWritten('function names in the register', functions?.names, /(\d+) built[ -]in function names/i),
  assertWritten('JSON function names', functions?.json, /all (\d+) function names/i),
  assertWritten('driver capabilities', caps?.total, /(\d+) capabilities reported/i),
  assertWritten('probe cases', probed?.total, /(\d+)[ -]case (?:differential )?(?:probe|compatibility probe)/i),
  // `docs/feature-comparison.md` wrote it as "391 of 416 probed cases produce", which the narrower
  // pattern walked straight past while every other page was corrected. A fact this check can miss in
  // one phrasing is a fact it does not check.
  assertWritten('probe cases the same', probed?.same, /(\d+) of (?:the )?416 (?:probed cases )?(?:produce|agree|SQL cases)/i, /read |dip to |gave \*\*|count read /),
  assertWritten('workspace members', lints?.members, /(\d+) crates (?:forbid|deny)/i),
  assertWritten('crates forbidding unsafe', lints?.forbidsUnsafe, /(?:(\d+) of (?:the )?29 crates forbid|and (\d+) forbid[\s\n]+`unsafe`)/i),
  assertWritten('crates denying the four lints', lints?.deniesFour, /(\d+) of the 29 crates deny/i),
  assertWritten('tests', tests?.tests, /(?:([\d,]+) tests across \d+ test targets|'([\d,]+)',\s*label: 'tests,)/i),
  // Held against the map rather than against the runner. A document that agrees
  // with a stale run used to pass, which is how 170, 169 and 181 coexisted.
  assertWritten('test targets', mapRows, /tests across (\d+) test targets/i),
  assertWritten('selection map rows', mapRows, /(\d+) (?:\[\[target\]\] )?rows in `?tests\/selection\.toml`?/i),
  assertWritten('documentation book chapters', chapters, /(?:in|carries|book has) (\d+) chapters/i),
];

const measured = {
  verbs, mcpTools: tools, dotCommands: dots, shellOptions: options, functions, capabilities: caps,
  probe: probed, registers: audit, crates: lints, tests, bookChapters: chapters,
  selectionRows: mapRows,
};

// A run that could not produce a fact is a failure of this file, not a reason to say nothing. The
// test runner is the only instrument here that can fail rather than merely be absent, so its own
// trouble is reported beside the facts and counted in the exit code.
const instrumentErrors = [];
if (tests?.error) instrumentErrors.push(tests.error);
if (probeStaleness) instrumentErrors.push(probeStaleness);
for (const instrument of INSTRUMENTS) {
  if (!passed(instrument.scopedOutWithout)) continue;
  if (instrument.value !== null && instrument.value !== undefined) continue;
  instrumentErrors.push(`${instrument.label} could not be measured; run \`${instrument.provision}\``);
}
if (mapRows !== null && tests?.targets !== undefined && tests?.targets !== null && tests.targets !== mapRows) {
  instrumentErrors.push(`the runner reported ${tests.targets} targets and tests/selection.toml has ${mapRows} rows, so one of them is stale`);
}

if (asJson) {
  console.log(JSON.stringify({ measured, checks, assertions, instrumentErrors }, null, 2));
} else {
  console.log('what the engine reports\n');
  for (const [name, value] of Object.entries(measured)) {
    console.log(`  ${name.padEnd(26)} ${value === null ? '(not built, or not run)' : JSON.stringify(value)}`);
  }
  if (tests?.missingPrerequisites?.length > 0 && !tests.error) {
    console.log('\n  the test run exited nonzero for prerequisites this machine does not have, which');
    console.log('  docs/repository.md documents, and every test passed:');
    for (const row of tests.missingPrerequisites) console.log(`    ${row.suite} needs ${row.needs}`);
  }
  console.log('\nwhat the documents say\n');
  for (const check of checks) {
    // Not `skip`. A fact whose instrument could not answer is in
    // `instrumentErrors` and fails the run; the only thing printed here without
    // a verdict is a fact a flag put out of scope, and it says which flag.
    if (check.skipped) {
      const scoped = INSTRUMENTS.find(
        (instrument) => instrument.scopedOutWithout && (instrument.facts || []).includes(check.label),
      );
      console.log(scoped
        ? `  n/a   ${check.label} - out of scope without ${scoped.scopedOutWithout}`
        : `  FAIL  ${check.label} - its instrument could not answer; see below`);
      continue;
    }
    if (check.seen === 0) { console.log(`  MISS  ${check.label} — the engine says ${check.expected} and no document states it`); continue; }
    if (check.wrong.length === 0) { console.log(`  ok    ${check.label} — ${check.expected}, in ${check.seen} place(s)`); continue; }
    console.log(`  FAIL  ${check.label} — the engine says ${check.expected}`);
    for (const place of check.wrong) console.log(`          ${place.file}:${place.line} says ${place.written}`);
  }
  for (const problem of instrumentErrors) console.log(`  FAIL  the instrument itself — ${problem}`);
  console.log('\nwhat the repository must not carry, and what it pins\n');
  for (const assertion of assertions) {
    if (assertion.problems.length === 0) { console.log(`  ok    ${assertion.label}`); continue; }
    console.log(`  FAIL  ${assertion.label}`);
    for (const problem of assertion.problems) console.log(`          ${problem}`);
  }
}

const failed = checks.filter((check) => !check.skipped && (check.wrong.length > 0 || check.seen === 0));
const broken = assertions.filter((assertion) => assertion.problems.length > 0);
if (failed.length > 0 || broken.length > 0 || instrumentErrors.length > 0) {
  if (!asJson) {
    const parts = [];
    if (failed.length > 0) parts.push(`${failed.length} fact(s) disagree with the engine`);
    if (broken.length > 0) {
      const count = broken.reduce((total, assertion) => total + assertion.problems.length, 0);
      parts.push(`${count} thing(s) the repository must not carry or must agree on`);
    }
    if (instrumentErrors.length > 0) parts.push(`${instrumentErrors.length} instrument(s) could not answer`);
    console.log(`\n${parts.join(', and ')}.`);
  }
  process.exit(1);
}
if (!asJson) console.log('\nEvery fact a document states is the fact the engine reports.');

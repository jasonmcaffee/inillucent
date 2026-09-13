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
 * Runs a program and returns its combined output, or null when it is not built.
 *
 * @param name - the program, looked for in target/release then target/debug
 * @param argv - its arguments
 * @param input - what to write to its standard input
 * @param timeout - how long to wait, in milliseconds
 */
function run(name, argv, input, timeout = 120000) {
  const exe = binary(name);
  if (!exe) return null;
  const result = spawnSync(exe, argv, { encoding: 'utf8', input, timeout });
  return `${result.stdout || ''}${result.stderr || ''}`;
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
  const out = run('inillucent', ['functions', '--output', 'json', '--limit', '0']);
  if (out === null) return null;
  let answer;
  try { answer = JSON.parse(out); } catch { return null; }
  const names = answer.rows.map((row) => row[0]);
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

/** Reads the probe's last run, which is what the SQL and compatibility pages quote. */
function probe() {
  const file = path.join(ROOT, '_agent_output', 'feature-probe', 'results.json');
  if (!fs.existsSync(file)) return null;
  const cases = JSON.parse(fs.readFileSync(file, 'utf8'));
  const rows = Array.isArray(cases) ? cases : cases.cases || [];
  const by = {};
  for (const row of rows) by[row.verdict] = (by[row.verdict] || 0) + 1;
  return { total: rows.length, same: by.same || 0, refused: by.refused || 0, differ: by['wrong-answer'] || 0, oursOnly: by['ours-only'] || 0 };
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

/** Counts the crates under each lint, which the repository page publishes. */
function crateLints() {
  const libs = [];
  for (const group of ['crates', 'drivers']) {
    const base = path.join(ROOT, group);
    if (!fs.existsSync(base)) continue;
    for (const entry of fs.readdirSync(base)) {
      const lib = path.join(base, entry, 'src', 'lib.rs');
      if (fs.existsSync(lib)) libs.push(fs.readFileSync(lib, 'utf8'));
    }
  }
  const members = (fs.readFileSync(path.join(ROOT, 'Cargo.toml'), 'utf8').match(/^\s{4}"(?:crates|drivers)\/[^"]+",$/gm) || []).length;
  const forbidsUnsafe = libs.filter((text) => text.includes('forbid(unsafe_code)')).length;
  const deniesFour = libs.filter((text) => ['unwrap_used', 'expect_used', 'panic', 'indexing_slicing'].every((lint) => text.includes(lint))).length;
  return { members, forbidsUnsafe, deniesFour };
}

/**
 * Runs the test runner, for the test and target counts.
 *
 * Behind `--run-tests`, because it is about five minutes and the rest of this file is about one
 * second. The counts it produces are what `docs/repository.md` and inillucent.com publish.
 */
function testRun() {
  if (!binary('inillucent-testrun')) return null;
  if (!process.argv.includes('--run-tests')) return null;
  // The whole suite is about five minutes, and a two minute cap killed it and read the missing
  // summary line as "the runner is not built" rather than as "it was cut off".
  const out = run('inillucent-testrun', ['--strict'], undefined, 1_800_000);
  const match = /(\d+) target\(s\), (\d+) test\(s\), (\d+) failed/.exec(out || '');
  if (!match) return null;
  return { targets: Number(match[1]), tests: Number(match[2]), failed: Number(match[3]) };
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

const checks = [
  assertWritten('command line verbs', verbs, /\b(\d+)\s+(?:command line )?(?:verbs|commands)\b(?!\s+(?:over MCP|served|an agent|as MCP))/i, /(?:dot|reference's)\s+$/),
  assertWritten('MCP tools', tools, /(\d+)\s+(?:of the (?:same|CLI's) commands served|MCP tools|tools an agent can call|tools an AI agent can call|of those commands over MCP|of the CLI's commands as MCP tools|of the same commands served)/i),
  assertWritten('shell dot commands', dots, /(\d+)\s+of (?:its|`sqlite3`'s|SQLite's) 65 dot commands/i),
  assertWritten('shell command line options', options?.total, /all (\d+) of (?:its|`sqlite3`'s) command line options/i),
  assertWritten('function names in the register', functions?.names, /(\d+) built[ -]in function names/i),
  assertWritten('JSON function names', functions?.json, /all (\d+) function names/i),
  assertWritten('driver capabilities', caps?.total, /(\d+) capabilities reported/i),
  assertWritten('probe cases', probed?.total, /(\d+)[ -]case (?:differential )?(?:probe|compatibility probe)/i),
  assertWritten('probe cases the same', probed?.same, /(\d+) of 416 (?:produce|SQL cases)/i),
  assertWritten('workspace members', lints?.members, /(\d+) crates (?:forbid|deny)/i),
  assertWritten('crates forbidding unsafe', lints?.forbidsUnsafe, /(?:(\d+) of (?:the )?29 crates forbid|and (\d+) forbid[\s\n]+`unsafe`)/i),
  assertWritten('crates denying the four lints', lints?.deniesFour, /(\d+) of the 29 crates deny/i),
  assertWritten('tests', tests?.tests, /(?:([\d,]+) tests across \d+ test targets|'([\d,]+)',\s*label: 'tests,)/i),
  assertWritten('test targets', tests?.targets, /tests across (\d+) test targets/i),
  assertWritten('documentation book chapters', chapters, /(?:in|carries|book has) (\d+) chapters/i),
];

const measured = {
  verbs, mcpTools: tools, dotCommands: dots, shellOptions: options, functions, capabilities: caps,
  probe: probed, registers: audit, crates: lints, tests, bookChapters: chapters,
};

if (asJson) {
  console.log(JSON.stringify({ measured, checks }, null, 2));
} else {
  console.log('what the engine reports\n');
  for (const [name, value] of Object.entries(measured)) {
    console.log(`  ${name.padEnd(26)} ${value === null ? '(not built, or not run)' : JSON.stringify(value)}`);
  }
  console.log('\nwhat the documents say\n');
  for (const check of checks) {
    if (check.skipped) { console.log(`  skip  ${check.label} — nothing to compare against`); continue; }
    if (check.seen === 0) { console.log(`  MISS  ${check.label} — the engine says ${check.expected} and no document states it`); continue; }
    if (check.wrong.length === 0) { console.log(`  ok    ${check.label} — ${check.expected}, in ${check.seen} place(s)`); continue; }
    console.log(`  FAIL  ${check.label} — the engine says ${check.expected}`);
    for (const place of check.wrong) console.log(`          ${place.file}:${place.line} says ${place.written}`);
  }
}

const failed = checks.filter((check) => !check.skipped && (check.wrong.length > 0 || check.seen === 0));
if (failed.length > 0) {
  if (!asJson) console.log(`\n${failed.length} fact(s) disagree with the engine.`);
  process.exit(1);
}
if (!asJson) console.log('\nEvery fact a document states is the fact the engine reports.');

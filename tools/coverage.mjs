// Measures line and region coverage over the workspace.
//
// **A wrapper, because Windows caps a command line at about 32,000 characters
// (task-1962, T2).** `cargo llvm-cov` runs the tests, merges the profile, and
// then calls `llvm-cov report` with one `-object` argument per test binary.
// There are 176 of them and their paths are long, so the final call is about
// 40,000 characters and Windows refuses it with `os error 206`, "The filename
// or extension is too long". The tests all ran; the report is what could not
// start, so the run ends with no number at all after twenty minutes of work.
//
// LLVM's tools read a response file: `llvm-cov report @args.txt` takes one
// argument per line and has no length limit. So when the report fails that way,
// this re-runs the command `cargo llvm-cov` itself printed, through a response
// file. Nothing about what is measured changes - it is the same command with
// the same arguments, handed over differently.
//
// Usage: `node tools/coverage.mjs [--per-crate]`.
//
//   --per-crate   also print a table of one row per crate, which is what
//                 `docs/repository.md` publishes
//   --write       write that table into docs/repository.md between the two
//                 marker comments, rather than leaving it for somebody to paste
//
// **`--write` exists because the published table had no checker and contradicted
// itself (task-1969, 4.12).** The program printed the table to stdout and
// stopped; a person pasted it into the page, and the prose under it went on
// saying 40.9% and 47.9% where the table said 40.6% and 48.4%. Nothing read the
// page back. Writing it between markers makes the page a function of the
// measurement, and `cargo test -p inillucent-compat --test tooling documentation::` reads
// the block.

import { spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = dirname(dirname(fileURLToPath(import.meta.url)));

/** The crates the retrieval tier needs ONNX and a corpus for, which a machine without either measures as zero. */
const EXCLUDED = ['inillucent-bench', 'inillucent-core', 'inillucent-model'];

/**
 * Runs `cargo llvm-cov` over the workspace.
 *
 * @returns the process result, with stdout and stderr captured
 */
function measure() {
  const args = [
    'llvm-cov',
    '--manifest-path',
    join(root, 'Cargo.toml'),
    '--workspace',
    '--release',
    ...EXCLUDED.flatMap((name) => ['--exclude', name]),
  ];
  return spawnSync('cargo', args, { encoding: 'utf8', shell: process.platform === 'win32' });
}

/**
 * Returns the `llvm-cov` command a failed report named, or null.
 *
 * cargo-llvm-cov prints the whole command it could not start, quoting any
 * argument that holds a space. This reads it back so the same command can be
 * re-run through a response file.
 *
 * @param stderr - what the failed run wrote
 */
function refusedCommand(stderr) {
  if (!stderr.includes('too long')) {
    return null;
  }
  const opened = stderr.indexOf('could not execute process `');
  if (opened === -1) {
    return null;
  }
  const from = opened + 'could not execute process `'.length;
  const to = stderr.indexOf('` (never executed)', from);
  if (to === -1) {
    return null;
  }
  const text = stderr.slice(from, to);
  const tokens = [...text.matchAll(/'([^']*)'|(\S+)/g)].map((found) => found[1] ?? found[2]);
  return { program: tokens[0], args: tokens.slice(1) };
}

/**
 * Runs one command with its arguments in a response file.
 *
 * The first argument stays on the command line because it is the subcommand,
 * which `llvm-cov` reads before it expands anything.
 *
 * @param command - the program and its arguments
 */
function throughResponseFile(command) {
  const directory = mkdtempSync(join(tmpdir(), 'inillucent-coverage-'));
  const file = join(directory, 'llvm-cov-args.txt');
  writeFileSync(file, command.args.slice(1).join('\n'), 'utf8');
  return spawnSync(command.program, [command.args[0], `@${file}`], { encoding: 'utf8' });
}

/**
 * Prints one row per crate, summed from the per-file report.
 *
 * @param report - `llvm-cov report`'s own output
 */
function perCrate(report) {
  const crates = new Map();
  let total = null;
  for (const line of report.split('\n')) {
    const parts = line.trim().split(/\s+/);
    if (parts[0] === 'TOTAL') {
      total = parts;
      continue;
    }
    if (!parts[0]?.endsWith('.rs')) {
      continue;
    }
    const held = parts[0].replace(/\\/g, '/').split('/');
    if (held.length < 2) {
      continue;
    }
    const regions = Number(parts[1]);
    const missedRegions = Number(parts[2]);
    const lines = Number(parts[7]);
    const missedLines = Number(parts[8]);
    if ([regions, missedRegions, lines, missedLines].some(Number.isNaN)) {
      continue;
    }
    const row = crates.get(held[1]) ?? { regions: 0, missedRegions: 0, lines: 0, missedLines: 0 };
    row.regions += regions;
    row.missedRegions += missedRegions;
    row.lines += lines;
    row.missedLines += missedLines;
    crates.set(held[1], row);
  }
  const percent = (whole, missed) => (whole === 0 ? '-' : `${((100 * (whole - missed)) / whole).toFixed(1)}%`);
  const rows = ['| crate | regions | region coverage | lines | line coverage |', '|---|---:|---:|---:|---:|'];
  const ordered = [...crates.entries()].sort((one, two) => two[1].lines - one[1].lines);
  for (const [crate, row] of ordered) {
    rows.push(
      `| \`${crate}\` | ${row.regions.toLocaleString('en-US')} | ${percent(row.regions, row.missedRegions)} ` +
        `| ${row.lines.toLocaleString('en-US')} | ${percent(row.lines, row.missedLines)} |`,
    );
  }
  if (total) {
    const regions = Number(total[1]);
    const lines = Number(total[7]);
    rows.push(
      `| **total** | **${regions.toLocaleString('en-US')}** | **${percent(regions, Number(total[2]))}** ` +
        `| **${lines.toLocaleString('en-US')}** | **${percent(lines, Number(total[8]))}** |`,
    );
  }
  return rows.join('\n');
}

/** Where the generated table starts and ends in `docs/repository.md`. */
const BEGIN = '<!-- coverage:begin -->';
const END = '<!-- coverage:end -->';

/**
 * Writes the table into `docs/repository.md` between the two markers.
 *
 * It refuses rather than appending when a marker is missing: a page that has
 * lost its markers is one where the table has been edited by hand, and silently
 * putting a second copy at the end would leave two tables disagreeing, which is
 * the condition this replaces.
 *
 * @param table - the rendered markdown table
 */
function writeIntoThePage(table) {
  const page = join(root, 'docs', 'repository.md');
  const text = readFileSync(page, 'utf8');
  const opens = text.indexOf(BEGIN);
  const closes = text.indexOf(END);
  if (opens < 0 || closes < 0 || closes < opens) {
    console.error(`${page} has no ${BEGIN} ... ${END} block, so there is nowhere to write the table`);
    process.exit(1);
  }
  const updated = `${text.slice(0, opens + BEGIN.length)}\n\n${table}\n\n${text.slice(closes)}`;
  if (updated === text) {
    console.log('the table in docs/repository.md is already what this run measured');
    return;
  }
  writeFileSync(page, updated);
  console.log('wrote the table into docs/repository.md');
}

const first = measure();
process.stdout.write(first.stdout ?? '');
let report = first.stdout ?? '';
if (first.status !== 0) {
  const refused = refusedCommand(first.stderr ?? '');
  if (!refused) {
    process.stderr.write(first.stderr ?? '');
    process.exit(first.status ?? 1);
  }
  console.log('the report was too long a command line for this platform; re-running it through a response file');
  const again = throughResponseFile(refused);
  process.stdout.write(again.stdout ?? '');
  if (again.status !== 0) {
    process.stderr.write(again.stderr ?? '');
    process.exit(again.status ?? 1);
  }
  report = again.stdout ?? '';
}

if (process.argv.includes('--per-crate')) {
  const table = perCrate(report);
  console.log(table);
  if (process.argv.includes('--write')) writeIntoThePage(table);
}

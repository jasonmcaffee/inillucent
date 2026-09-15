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

import { spawnSync } from 'node:child_process';
import { mkdtempSync, writeFileSync } from 'node:fs';
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
  console.log('| crate | regions | region coverage | lines | line coverage |');
  console.log('|---|---:|---:|---:|---:|');
  const ordered = [...crates.entries()].sort((one, two) => two[1].lines - one[1].lines);
  for (const [crate, row] of ordered) {
    console.log(
      `| \`${crate}\` | ${row.regions.toLocaleString('en-US')} | ${percent(row.regions, row.missedRegions)} ` +
        `| ${row.lines.toLocaleString('en-US')} | ${percent(row.lines, row.missedLines)} |`,
    );
  }
  if (total) {
    const regions = Number(total[1]);
    const lines = Number(total[7]);
    console.log(
      `| **total** | **${regions.toLocaleString('en-US')}** | **${percent(regions, Number(total[2]))}** ` +
        `| **${lines.toLocaleString('en-US')}** | **${percent(lines, Number(total[8]))}** |`,
    );
  }
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
  perCrate(report);
}

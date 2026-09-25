#!/usr/bin/env node
/**
 * Reads the documentation and reports every line that breaks the writing rules in
 * `docs/writing-style.md`.
 *
 * The rules are the ones a reader notices: an em dash, a spaced hyphen used as a dash, a word made
 * by joining two words with a hyphen, and the phrases listed in `tools/doc-style/rules.txt`. Only
 * prose is read. Fenced code, inline code, link targets, URLs, paths, file names and HTML comments
 * are removed first, because a command, a file name or a program's output cannot be reworded.
 *
 * `crates/inillucent-compat/tests/tooling/documentation.rs` applies the same rules to the same files, read
 * from the same `rules.txt` and `scope.txt`, as part of the test suite. Neither uses a regular
 * expression for the rules themselves, so the two can be kept identical by reading them side by
 * side. This script exists so a writer can check a page in a second, and so the inillucent.com
 * documentation chapters, which live in another repository, can be checked too.
 *
 * Usage:
 *   node tools/doc-style/check.mjs                                   # every page in scope
 *   node tools/doc-style/check.mjs docs/sql.md README.md             # only these files
 *   node tools/doc-style/check.mjs --site ../black-rainbow-labs-sites/sites/inillucent         # only the site chapters
 *   node tools/doc-style/check.mjs --site ../black-rainbow-labs-sites/sites/inillucent --all   # the site and every page
 *
 * A page that has to quote a banned phrase, as `docs/writing-style.md` does, puts the quote between
 * `<!-- doc-style: off -->` and `<!-- doc-style: on -->`.
 *
 * It exits 1 when any rule is broken and prints one line per problem, as `file:line: rule: text`.
 */
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, '..', '..');

/**
 * Words joined by a hyphen that are allowed in prose, because they are names somebody else chose.
 *
 * Everything else joined by a hyphen is written as separate words: "read only", "built in",
 * "nearest neighbor". A program, crate or file name belongs in backticks, where it is not checked.
 */
export const ALLOWED_HYPHENATED = ['b-tree', 'b-trees', 'r-tree', 'r-trees', 'mach-o', 'p-value', 'p-values'];

/** File name endings that mark a word as a file name, which is not prose. */
export const FILE_EXTENSIONS = ['md', 'rs', 'toml', 'json', 'mjs', 'js', 'ts', 'tsx', 'ps1', 'sh', 'rdb', 'db', 'txt', 'yml', 'yaml', 'py', 'go', 'php', 'h', 'c', 'exe', 'dll', 'so', 'dylib', 'pkg', 'deb', 'rpm', 'zip', 'gz', 'onnx'];

/**
 * Reads a list file: one entry per line, blank lines and `#` comments ignored.
 *
 * @param name - the file name inside `tools/doc-style/`
 */
function readList(name) {
  return fs.readFileSync(path.join(HERE, name), 'utf8').split(/\r?\n/).map((line) => line.trim()).filter((line) => line && !line.startsWith('#'));
}

/** Returns the banned phrases from `rules.txt`, lower cased. */
export function phraseRules() {
  return readList('rules.txt').map((phrase) => phrase.toLowerCase());
}

/**
 * Turns one Markdown file into prose lines, one per line of the file.
 *
 * A line inside a fenced block becomes an empty string, and a line inside a
 * `doc-style: off` region becomes null, so line numbers still match the file.
 *
 * @param text - the whole file
 */
export function proseLines(text) {
  const out = [];
  let fence = null;
  let comment = false;
  let off = false;
  for (const line of text.split(/\r?\n/)) {
    if (line.includes('<!-- doc-style: off -->')) off = true;
    if (line.includes('<!-- doc-style: on -->')) { off = false; out.push(null); continue; }
    if (off) { out.push(null); continue; }
    const trimmed = line.trimStart();
    const marker = trimmed.startsWith('```') ? '```' : trimmed.startsWith('~~~') ? '~~~' : null;
    if (fence) {
      if (marker === fence) fence = null;
      out.push('');
      continue;
    }
    if (marker) { fence = marker; out.push(''); continue; }
    let prose = line;
    if (comment) {
      const end = prose.indexOf('-->');
      if (end < 0) { out.push(''); continue; }
      prose = prose.slice(end + 3);
      comment = false;
    }
    for (;;) {
      const open = prose.indexOf('<!--');
      if (open < 0) break;
      const close = prose.indexOf('-->', open + 4);
      if (close < 0) { prose = prose.slice(0, open); comment = true; break; }
      prose = `${prose.slice(0, open)} ${prose.slice(close + 3)}`;
    }
    out.push(stripInline(prose));
  }
  return out;
}

/**
 * Removes inline code: a run of backticks up to the next run of the same length.
 *
 * @param line - one line of Markdown
 */
function withoutInlineCode(line) {
  let out = '';
  let at = 0;
  while (at < line.length) {
    if (line[at] !== '`') { out += line[at]; at += 1; continue; }
    let run = 0;
    while (line[at + run] === '`') run += 1;
    const ticks = '`'.repeat(run);
    let close = line.indexOf(ticks, at + run);
    while (close >= 0 && line[close + run] === '`') close = line.indexOf(ticks, close + run + 1);
    if (close < 0) { out += line.slice(at); break; }
    out += ' CODE ';
    at = close + run;
  }
  return out;
}

/**
 * Removes everything between two markers, markers included, and puts `replacement` in its place.
 *
 * @param line - the text
 * @param open - where a removed part starts
 * @param close - where it ends
 * @param replacement - what is left behind
 */
function withoutBetween(line, open, close, replacement) {
  let out = line;
  for (let at = out.indexOf(open); at >= 0; at = out.indexOf(open, at + replacement.length)) {
    const end = out.indexOf(close, at + open.length);
    if (end < 0) break;
    out = out.slice(0, at) + replacement + out.slice(end + close.length);
  }
  return out;
}

/**
 * Removes the punctuation around a word.
 *
 * @param word - one whitespace separated word
 */
function trimWord(word) {
  let start = 0;
  let end = word.length;
  while (start < end && '([{"\'*_'.includes(word[start])) start += 1;
  while (end > start && ')]}"\'*_,.;:!?'.includes(word[end - 1])) end -= 1;
  return word.slice(start, end);
}

/**
 * Reports whether one word names a path, a URL or a file.
 *
 * @param word - one whitespace separated word
 */
function isPathOrFile(word) {
  if (word.includes('/') || word.includes('\\')) return true;
  const bare = trimWord(word);
  const dot = bare.lastIndexOf('.');
  return dot > 0 && FILE_EXTENSIONS.includes(bare.slice(dot + 1).toLowerCase());
}

/**
 * Removes the parts of one line of Markdown that are not prose, and joins the rest with single
 * spaces.
 *
 * @param line - one line, already outside any fenced block
 */
export function stripInline(line) {
  let prose = withoutInlineCode(line);
  if (/^\s*\[[^\]]+\]:\s/.test(prose)) return '';
  prose = withoutBetween(prose, '](', ')', '] ');
  prose = withoutBetween(prose, '<', '>', ' ');
  return prose.split(/\s+/).filter(Boolean).map((word) => (isPathOrFile(word) ? 'PATH' : word)).join(' ');
}

/**
 * Reports whether a word is two or more words joined by hyphens, and not a name the rules allow.
 *
 * @param word - one whitespace separated word
 */
export function isHyphenated(word) {
  const bare = trimWord(word);
  const parts = bare.split('-');
  if (parts.length < 2 || parts.some((part) => !/^[A-Za-z]+$/.test(part))) return false;
  if (ALLOWED_HYPHENATED.includes(bare.toLowerCase())) return false;
  if (bare === bare.toUpperCase()) return false;
  return !bare.toLowerCase().startsWith('inillucent-');
}

/**
 * Finds `phrase` in `text` where it stands as whole words.
 *
 * @param text - lower cased prose
 * @param phrase - a lower cased phrase from `rules.txt`
 */
export function containsPhrase(text, phrase) {
  const wordChar = (ch) => ch !== undefined && /[a-z0-9_-]/.test(ch);
  for (let at = text.indexOf(phrase); at >= 0; at = text.indexOf(phrase, at + 1)) {
    if (!wordChar(text[at - 1]) && !wordChar(text[at + phrase.length])) return true;
  }
  return false;
}

/**
 * Returns every rule one prose line breaks.
 *
 * @param prose - one line with code, links and paths removed
 * @param raw - the same line as written, for the em dash check, which applies to code as well
 * @param rules - the phrases from `rules.txt`
 */
export function problemsIn(prose, raw, rules) {
  const found = [];
  if (raw.includes('—')) found.push('em dash');
  if (prose.includes('–')) found.push('en dash');
  const words = prose.split(' ').filter(Boolean);
  let first = 0;
  if (words[0] === '>') first += 1;
  if (words[first] && /^(?:[-*+]|\d+[.)])$/.test(words[first])) first += 1;
  const body = words.slice(first);
  for (let index = 1; index < body.length - 1; index += 1) {
    if ((body[index] === '-' || body[index] === '--') && body[index - 1] !== '|' && body[index + 1] !== '|') {
      found.push('a spaced hyphen used as a dash');
      break;
    }
  }
  for (const word of body) if (isHyphenated(word)) found.push(`hyphenated word "${trimWord(word)}"`);
  const lower = body.join(' ').toLowerCase();
  for (const phrase of rules) if (containsPhrase(lower, phrase)) found.push(`banned "${phrase}"`);
  return found;
}

/**
 * Checks one Markdown file and returns its problems.
 *
 * @param file - an absolute path
 * @param rules - the phrases from `rules.txt`
 */
export function checkMarkdown(file, rules) {
  const text = fs.readFileSync(file, 'utf8');
  const raw = text.split(/\r?\n/);
  const problems = [];
  proseLines(text).forEach((line, index) => {
    if (line === null) return;
    for (const rule of problemsIn(line, raw[index], rules)) {
      problems.push({ file, line: index + 1, rule, text: raw[index].trim().slice(0, 140) });
    }
  });
  return problems;
}

/**
 * Checks the prose of the inillucent.com documentation chapters.
 *
 * The chapters are data in `src/data/documentation.ts`. The titles, summaries, paragraphs, points
 * and example explanations are prose. The example code and its recorded result are not.
 *
 * @param site - the site folder, black-rainbow-labs-sites/sites/inillucent
 * @param rules - the phrases from `rules.txt`
 */
export async function checkSite(site, rules) {
  const file = path.join(site, 'src', 'data', 'documentation.ts');
  const module = await import(`file://${file.replace(/\\/g, '/')}`);
  const problems = [];
  const check = (where, value) => {
    for (const rule of problemsIn(stripInline(value), value, rules)) problems.push({ file, line: where, rule, text: value.slice(0, 140) });
  };
  for (const group of module.documentationGroups) {
    check(`group ${group.id}`, group.title);
    check(`group ${group.id}`, group.description);
  }
  for (const chapter of module.documentationChapters) {
    const where = `chapter ${chapter.number} ${chapter.id}`;
    for (const value of [chapter.title, chapter.summary, ...chapter.paragraphs, ...(chapter.points ?? [])]) check(where, value);
    for (const example of chapter.examples) {
      check(`${where}, example "${example.title}"`, example.title);
      check(`${where}, example "${example.title}"`, example.explanation);
    }
  }
  return problems;
}

/**
 * Expands the scope list, or the files named on the command line, into absolute paths.
 *
 * @param named - file names given on the command line, relative to the repository root
 */
export function filesToCheck(named) {
  const entries = named.length > 0 ? named : readList('scope.txt');
  const files = [];
  const walk = (dir) => {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) walk(full);
      else if (entry.name.endsWith('.md')) files.push(full);
    }
  };
  for (const entry of entries) {
    const full = path.resolve(ROOT, entry);
    if (!fs.existsSync(full)) continue;
    if (fs.statSync(full).isDirectory()) walk(full); else files.push(full);
  }
  return files;
}

/** Runs the check over the repository, and over the site when `--site` names one. */
async function main() {
  const args = process.argv.slice(2);
  const siteAt = args.indexOf('--site');
  const site = siteAt >= 0 ? path.resolve(args[siteAt + 1]) : null;
  const named = args.filter((arg, index) => !arg.startsWith('--') && !(siteAt >= 0 && index === siteAt + 1));
  const rules = phraseRules();
  const siteOnly = site && named.length === 0 && !args.includes('--all');
  const files = siteOnly ? [] : filesToCheck(named);
  const problems = [];
  for (const file of files) problems.push(...checkMarkdown(file, rules));
  if (site) problems.push(...await checkSite(site, rules));
  for (const problem of problems) {
    const at = typeof problem.line === 'number' ? `${path.relative(ROOT, problem.file)}:${problem.line}` : `${path.relative(ROOT, problem.file)} (${problem.line})`;
    console.log(`${at}: ${problem.rule}: ${problem.text}`);
  }
  const checked = files.length + (site ? 1 : 0);
  console.log(problems.length === 0 ? `\n${checked} file(s) checked, no problems.` : `\n${problems.length} problem(s) in ${checked} file(s) checked.`);
  process.exit(problems.length === 0 ? 0 : 1);
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) await main();

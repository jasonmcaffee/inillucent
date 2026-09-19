/**
 * Rewrites the `downloads` array in the site's content file, and the two other fields beside it
 * that carry the release.
 *
 * **Three fields, not one, because the other two went stale.** `downloads` was all this rewrote, so
 * after publishing 0.1.3 the page listed five downloads at 0.1.3 while `version`, two dozen lines
 * below, still said `0.1.2`, and the macOS line still offered "build from source" under a comment
 * saying no macOS archive is built "because each archive is built on the platform it targets" -
 * which stopped being true on 2026-09-19. A page that contradicts its own download list is worse
 * than one that is merely behind.
 *
 * packaging/publish-site.ps1 knows which artifacts exist, how big each one is
 * and what its checksum is; this knows how the site's data file is shaped. The
 * two are separate because a regular expression over TypeScript is the part
 * most likely to go wrong, and it is easier to read in one file that does
 * nothing else.
 *
 * Usage: node update-downloads.mjs <content.ts> '<json array of entries>' [version]
 */
import { readFileSync, writeFileSync } from 'node:fs';

/**
 * Renders one download entry as the TypeScript object literal the site file uses.
 * @param entry - platform, detail, href and sha256 for one artifact
 */
function renderEntry(entry) {
  const lines = [
    '    {',
    `      platform: ${JSON.stringify(entry.platform)},`,
    `      detail: ${JSON.stringify(entry.detail)},`,
    `      href: ${JSON.stringify(entry.href)},`,
  ];
  if (entry.sha256) lines.push(`      sha256: ${JSON.stringify(entry.sha256)},`);
  lines.push('    },');
  return lines.join('\n');
}

/**
 * Replaces the downloads array between its opening line and its typed close,
 * failing loudly rather than writing a file that no longer parses.
 * @param source - the current content.ts text
 * @param entries - the entries to write
 */
function replaceDownloads(source, entries) {
  const open = source.indexOf('  downloads: [');
  if (open === -1) throw new Error('content.ts has no `downloads: [` to replace');
  const closeMarker = '] as readonly Download[],';
  const close = source.indexOf(closeMarker, open);
  if (close === -1) throw new Error('content.ts has no `] as readonly Download[],` after `downloads: [`');

  const body = entries.map(renderEntry).join('\n');
  const replacement = `  downloads: [\n${body}\n  ${closeMarker}`;
  return source.slice(0, open) + replacement + source.slice(close + closeMarker.length);
}

/**
 * Replaces one `key: 'value'` line in the content file.
 *
 * Absent is an error rather than a no-op: a field this is asked to write and cannot find has been
 * renamed, and carrying on would leave it stale with nothing said.
 * @param source - the current content.ts text
 * @param key - the field name
 * @param value - the new value
 */
function replaceField(source, key, value) {
  const pattern = new RegExp(`(\\n  ${key}: ')[^']*(')`);
  if (!pattern.test(source)) throw new Error(`content.ts has no ${key} to update`);
  return source.replace(pattern, `$1${value}$2`);
}

/**
 * Points the macOS line at the published package rather than at the documentation.
 *
 * The site carried `macOS: { label: 'macOS: build from source', href: '/docs' }` for as long as
 * there was no macOS archive to offer. There is one now, so that line would be contradicting the
 * download listed above it.
 * @param source - the current content.ts text
 * @param href - where the macOS package is
 */
function replaceMacosFallback(source, href) {
  const pattern = /\n  macOS: \{[^}]*\},/;
  if (!pattern.test(source)) return source;
  return source.replace(pattern, `\n  macOS: { label: 'macOS: download the .pkg', href: '${href}' },`);
}

const [contentPath, entriesJson, version] = process.argv.slice(2);
if (!contentPath || !entriesJson) {
  console.error('usage: node update-downloads.mjs <content.ts> <json> [version]');
  process.exit(2);
}

const entries = JSON.parse(entriesJson);
if (!Array.isArray(entries) || entries.length === 0) {
  console.error('the entries argument must be a non-empty JSON array');
  process.exit(2);
}

const source = readFileSync(contentPath, 'utf8');
let updated = replaceDownloads(source, entries);
if (version) {
  updated = replaceField(updated, 'version', version);
  const macos = entries.find((entry) => entry.platform === 'macOS');
  if (macos) updated = replaceMacosFallback(updated, macos.href);
}
writeFileSync(contentPath, updated);
console.log(`wrote ${entries.length} download entries into ${contentPath}`);

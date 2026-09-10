/**
 * Rewrites the `downloads` array in the site's content file.
 *
 * packaging/publish-site.ps1 knows which artifacts exist, how big each one is
 * and what its checksum is; this knows how the site's data file is shaped. The
 * two are separate because a regular expression over TypeScript is the part
 * most likely to go wrong, and it is easier to read in one file that does
 * nothing else.
 *
 * Usage: node update-downloads.mjs <content.ts> '<json array of entries>'
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

const [contentPath, entriesJson] = process.argv.slice(2);
if (!contentPath || !entriesJson) {
  console.error('usage: node update-downloads.mjs <content.ts> <json>');
  process.exit(2);
}

const entries = JSON.parse(entriesJson);
if (!Array.isArray(entries) || entries.length === 0) {
  console.error('the entries argument must be a non-empty JSON array');
  process.exit(2);
}

const source = readFileSync(contentPath, 'utf8');
const updated = replaceDownloads(source, entries);
writeFileSync(contentPath, updated);
console.log(`wrote ${entries.length} download entries into ${contentPath}`);

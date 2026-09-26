// Exports the feature probe's cases for the statement matrix.
//
// The matrix copies these cases into its Layer 1 corpus so they also run on
// every change (section 10 of tasks/task-2135-sql-statement-matrix-tdd.md).
// This writes one line per case: the id, a tab, the matrix family, a tab, and
// the script with `\n` for a newline. `inillucent-matrix convert-probe` reads
// it. Cases that drive the shell (a line starting with `.`) or that need a file
// beside the database are left out, because the matrix compares typed values
// through the engine rather than what the shell prints.
//
// Usage: node tools/feature-probe/export-matrix.js <output path>

const fs = require('fs');
const path = require('path');

/** The matrix family for a probe area. @param area - the probe's area name */
function familyOf(area) {
  const table = {
    select: 'select', join: 'join', compound: 'compound', subquery: 'subquery', cte: 'cte',
    window: 'window', dml: 'insert', upsert: 'insert', 'ddl-table': 'ddl_table',
    'ddl-index': 'ddl_index', 'ddl-view': 'ddl_view', 'ddl-trigger': 'trigger',
    alter: 'ddl_table', constraint: 'constraint', types: 'expression', operators: 'expression',
    collation: 'expression', 'fn-core': 'function', 'fn-agg': 'function', 'fn-time': 'function',
    'fn-math': 'function', 'fn-json': 'function', tvf: 'vtab', pragma: 'pragma',
    explain: 'maintenance', txn: 'transaction', attach: 'schema', temp: 'schema', fts5: 'vtab',
    rtree: 'vtab', ext: 'vtab', schema: 'schema', syntax: 'expression', vector: 'vector',
    params: 'expression', limits: 'expression', integrity: 'maintenance',
  };
  return table[area] || 'select';
}

/** Writes the export. @param output - where to write it */
function main(output) {
  const cases = [
    ...require(path.join(__dirname, 'cases.js')),
    ...require(path.join(__dirname, 'cases-extra.js')),
  ];
  const lines = [];
  let skipped = 0;
  for (const one of cases) {
    const usesShell = one.sql.split('\n').some((line) => line.trimStart().startsWith('.'));
    // The vector cases have no SQLite equivalent; the matrix grades vectors
    // against distances computed in Rust, in its own `vector` family.
    if (usesShell || one.files || one.area === 'vector') {
      skipped += 1;
      continue;
    }
    const script = one.sql.replace(/\\/g, '\\\\').replace(/\r?\n/g, '\\n');
    lines.push(`probe-${one.id}\t${familyOf(one.area)}\t${script}`);
  }
  fs.writeFileSync(output, lines.join('\n') + '\n');
  console.log(`exported ${lines.length} cases, left out ${skipped} that drive the shell or need files`);
}

main(process.argv[2] || 'probe-cases.tsv');

// What the vector side answers from SQL, asked one pgvector feature at a time.
//
// The reference here is not a running PostgreSQL - the graded comparison
// against pgvector is `inillucent-scorecard.md` and is about ranking quality,
// not about spelling. This asks the other question: of the things a pgvector
// application writes, which ones can be written here at all.

const { spawnSync } = require('child_process');
const fs = require('fs');
const path = require('path');
const { ROOT, OUT, OURS, REF } = require('./paths.js');

const AREA = path.join(OUT, 'vecfeat', String(Date.now()));

/** A four-float vector as the blob literal the engine stores. @param a - the components */
const lit = (a) => {
  const b = Buffer.alloc(a.length * 4);
  a.forEach((v, i) => b.writeFloatLE(v, i * 4));
  return `x'${b.toString('hex')}'`;
};
const E1 = lit([1, 0, 0, 0]);
const E2 = lit([0, 1, 0, 0]);
const SEED = `CREATE TABLE e(id INTEGER PRIMARY KEY, src TEXT, v VECTOR(4));\nINSERT INTO e VALUES (1,'a',${E1}),(2,'b',${E2});\n`;

const CASES = [
  ['type: VECTOR(n) column', `${SEED}SELECT typeof(v) FROM e LIMIT 1;`],
  ['type: a half-precision vector', `CREATE TABLE h(v HALFVEC(4));`],
  ['type: a bit vector', `CREATE TABLE h(v BIT(8));`],
  ['type: a sparse vector', `CREATE TABLE h(v SPARSEVEC(4));`],
  ['distance: cosine', `${SEED}SELECT round(vector_distance_cos(v,${E1}),4) FROM e ORDER BY id;`],
  ['distance: L2', `${SEED}SELECT round(vector_distance_l2(v,${E1}),4) FROM e ORDER BY id;`],
  ['distance: inner product', `${SEED}SELECT round(vector_dot(v,${E1}),4) FROM e ORDER BY id;`],
  ['distance: L1 / taxicab', `${SEED}SELECT vector_distance_l1(v,${E1}) FROM e;`],
  ['distance: Hamming', `${SEED}SELECT hamming_distance(v,${E1}) FROM e;`],
  ['distance: Jaccard', `${SEED}SELECT jaccard_distance(v,${E1}) FROM e;`],
  ["operator: <=> (cosine)", `${SEED}SELECT v <=> ${E1} FROM e;`],
  ["operator: <-> (L2)", `${SEED}SELECT v <-> ${E1} FROM e;`],
  ["operator: <#> (inner product)", `${SEED}SELECT v <#> ${E1} FROM e;`],
  ['function: vector_dims', `${SEED}SELECT vector_dims(v) FROM e LIMIT 1;`],
  ['function: vector_norm', `${SEED}SELECT vector_norm(v) FROM e LIMIT 1;`],
  ['function: l2_normalize', `${SEED}SELECT l2_normalize(v) FROM e LIMIT 1;`],
  ['function: binary_quantize', `${SEED}SELECT binary_quantize(v) FROM e LIMIT 1;`],
  ['function: subvector', `${SEED}SELECT subvector(v,1,2) FROM e LIMIT 1;`],
  ['arithmetic on vectors', `${SEED}SELECT v + v FROM e LIMIT 1;`],
  ['aggregate: avg over vectors', `${SEED}SELECT avg(v) FROM e;`],
  ['index: HNSW', `${SEED}CREATE INDEX ie ON e USING inillucent_hnsw (v);\nSELECT id FROM e ORDER BY vector_distance_cos(v,${E1}) LIMIT 1;`],
  ['index: HNSW with m and ef_construction', `${SEED}CREATE INDEX ie ON e USING inillucent_hnsw (v) WITH (m = 32, ef_construction = 128);`],
  ['index: IVFFlat', `${SEED}CREATE INDEX ie ON e USING ivfflat (v) WITH (lists = 4);`],
  ['index: an L2 ordering planned onto the index', `${SEED}CREATE INDEX ie ON e USING inillucent_hnsw (v);\nEXPLAIN QUERY PLAN SELECT id FROM e ORDER BY vector_distance_l2(v,${E1}) LIMIT 1;`],
  ['index: a cosine ordering planned onto the index', `${SEED}CREATE INDEX ie ON e USING inillucent_hnsw (v);\nEXPLAIN QUERY PLAN SELECT id FROM e ORDER BY vector_distance_cos(v,${E1}) LIMIT 1;`],
  ['runtime: ef_search equivalent', `${SEED}CREATE INDEX ie ON e USING inillucent_hnsw (v);\nPRAGMA hnsw_ef_search = 100;`],
  ['index: two vector columns on one table', `CREATE TABLE e(id INTEGER PRIMARY KEY, v VECTOR(4), w VECTOR(4));\nINSERT INTO e VALUES (1,${E1},${E2});\nCREATE INDEX iv ON e USING inillucent_hnsw (v);\nCREATE INDEX iw ON e USING inillucent_hnsw (w);\nSELECT id FROM e ORDER BY vector_distance_cos(w,${E2}) LIMIT 1;`],
  ['index: a partial vector index', `${SEED}CREATE INDEX ie ON e USING inillucent_hnsw (v) WHERE src='a';`],
  ['vector NULL handling', `CREATE TABLE e(id INTEGER PRIMARY KEY, v VECTOR(4));\nINSERT INTO e VALUES (1,NULL);\nSELECT typeof(v), vector_distance_cos(v,${E1}) FROM e;`],
  ['dimension mismatch is refused', `${SEED}SELECT vector_distance_cos(v, ${lit([1, 0, 0])}) FROM e LIMIT 1;`],
  ['a 1536 dimension column', `CREATE TABLE e(id INTEGER PRIMARY KEY, v VECTOR(1536));\nSELECT count(*) FROM e;`],
  ['lexical search beside the vector one (FTS5)', `CREATE VIRTUAL TABLE f USING fts5(body);\nINSERT INTO f VALUES ('the quick fox');\nSELECT count(*) FROM f WHERE f MATCH 'fox';`],
  ['hybrid: the inillucent_search virtual table', `CREATE VIRTUAL TABLE s USING inillucent_search(body, dims=4);\nSELECT count(*) FROM s;`],
  ['embedding generation in the database', `SELECT embed('hello');`],
];

/**
 * The cases whose *right* answer is a refusal.
 *
 * The check below reads any error as a failure, which is the right default -
 * a feature that errors is a feature that is not there. It is the wrong
 * reading for a case that exists to prove a refusal happens, and there is
 * one: comparing vectors of different widths has no answer, so pgvector
 * raises and so does this. Scoring that as a missing feature made the count
 * one lower than the measurement.
 */
const REFUSALS = new Set(['dimension mismatch is refused']);

const rows = [];
for (const [name, sql] of CASES) {
  const dir = path.join(AREA, name.replace(/[^a-z0-9]+/gi, '_'));
  fs.mkdirSync(dir, { recursive: true });
  const result = spawnSync(OURS, [path.join(dir, 'p.db')], { cwd: dir, input: sql, encoding: 'utf8', timeout: 30000, windowsHide: true });
  const text = ((result.stdout || '') + (result.stderr || '')).replace(/\r\n/g, '\n').trim();
  const refused = /(^|\n)(Parse error|Runtime error|Error)\b/.test(text);
  const ok = REFUSALS.has(name) ? refused : !refused;
  rows.push({ name, ok, text });
  console.log(`${ok ? 'YES' : 'NO '}  ${name}\n     ${text.split('\n').join('\n     ').slice(0, 300)}`);
}
fs.writeFileSync(path.join(OUT, 'vector-features.json'), JSON.stringify(rows, null, 2));

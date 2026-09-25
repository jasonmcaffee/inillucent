# inillucent performance scorecard

Label `nightly-20260925-072550`, platform `windows-x86_64`, 30 paired rounds per scale, bootstrap seed 17900001. Every optimization is on, which is the shipped engine.

Both engines read the same plan file. The ratio is SQLite over inillucent, so **above one means inillucent is faster**. A workload whose two engines returned different answers is reported as a correctness failure and is not timed.

## Fair configuration

| setting | value |
|---|---|
| journal mode | `delete` |
| synchronous | `full` |
| page size | 4096 |
| cache | -2000 in SQLite's units: positive is pages, negative is KiB |
| statement reuse | prepared once except the `open.prepare` family |
| database | on disk, cloned from one pristine image per round |

**On memory, which is the setting most easily got wrong.** This scorecard measures the bytecode engine, whose page cache and SQLite's are both governed by the `cache_size` above, so the two arms are given the same memory by construction.

That is *not* automatic for the vectorised engine, and the Phase 1 numbers were inflated because it was not: the prototype's trees were fully resident while SQLite ran at the plan's 2 MB cache. Phase 2's gate (`inillucent-readgate`) closes it by deriving SQLite's `cache_size` from the byte size of inillucent's own buffer pool, so `--frames` moves both sides together and neither engine can be given memory the other is not. The gate prints both figures before it times anything. A ratio measured without that is a ratio between two different machines.

## Scale `medium` - 100000 rows

Weighted geometric mean **0.759x**, 95% interval [0.750, 0.766]. The release bound is a lower bound of at least 3.00x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.332x | [0.329, 0.335] | loss | **below 1.00x** |
| `read.point` | 0.16 | 1.504x | [1.483, 1.524] | win | met |
| `read.range` | 0.12 | 0.907x | [0.897, 0.917] | loss | **below 1.00x** |
| `read.analytical` | 0.10 | 0.308x | [0.307, 0.310] | loss | **below 1.00x** |
| `read.join` | 0.08 | 0.636x | [0.628, 0.646] | loss | **below 1.00x** |
| `write` | 0.20 | 0.760x | [0.731, 0.791] | loss | **below 1.00x** |
| `transaction` | 0.10 | 0.765x | [0.742, 0.783] | loss | **below 1.00x** |
| `schema` | 0.04 | 1.443x | [1.419, 1.471] | win | met |
| `extension` | 0.08 | 0.568x | [0.556, 0.585] | loss | **below 1.00x** |
| `large.values` | 0.04 | 1.826x | [1.617, 1.961] | win | met |

### By workload

| workload | family | inillucent median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 18.40 ms | 1.61 ms | 0.087x | [0.086, 0.088] | 30 |
| `prepare.point` | `open.prepare` | 36.08 ms | 45.25 ms | 1.252x | [1.247, 1.279] | 30 |
| `point.rowid` | `read.point` | 19.68 ms | 38.70 ms | 1.977x | [1.954, 2.033] | 30 |
| `point.index` | `read.point` | 25.28 ms | 30.13 ms | 1.180x | [1.169, 1.204] | 30 |
| `point.miss` | `read.point` | 18.41 ms | 26.39 ms | 1.441x | [1.417, 1.460] | 30 |
| `range.covering` | `read.range` | 19.85 ms | 10.68 ms | 0.540x | [0.534, 0.546] | 30 |
| `range.lookaside` | `read.range` | 38.37 ms | 34.30 ms | 0.894x | [0.883, 0.913] | 30 |
| `range.reverse` | `read.range` | 9.41 ms | 14.37 ms | 1.529x | [1.514, 1.565] | 30 |
| `scan.aggregate` | `read.analytical` | 226.38 ms | 90.89 ms | 0.402x | [0.400, 0.405] | 30 |
| `scan.group` | `read.analytical` | 205.72 ms | 76.28 ms | 0.371x | [0.368, 0.374] | 30 |
| `scan.sort` | `read.analytical` | 29.02 ms | 279.28 ms | 9.611x | [9.495, 9.708] | 30 |
| `scan.distinct` | `read.analytical` | 156.61 ms | 982.75 us | 0.006x | [0.006, 0.006] | 30 |
| `join.selective` | `read.join` | 18.32 ms | 20.32 ms | 1.106x | [1.083, 1.120] | 30 |
| `join.range` | `read.join` | 61.48 ms | 22.60 ms | 0.365x | [0.362, 0.374] | 30 |
| `correlated.exists` | `read.correlated` | 2.26 ms | 169.70 us | 0.074x | [0.073, 0.102] | 30 |
| `correlated.in` | `read.correlated` | 1.50 ms | 91.10 us | 0.060x | [0.060, 0.066] | 30 |
| `correlated.exists.selective` | `read.correlated` | 55.15 us | 16.90 us | 0.308x | [0.284, 0.336] | 30 |
| `correlated.scalar.selective` | `read.correlated` | 44.35 us | 16.00 us | 0.356x | [0.353, 0.374] | 30 |
| `write.insert.batch` | `write` | 18.18 ms | 19.61 ms | 1.072x | [0.731, 0.988] | 30 |
| `write.insert.autocommit` | `write` | 132.95 ms | 424.48 ms | 3.202x | [3.124, 3.521] | 30 |
| `write.update.indexed` | `write` | 620.28 ms | 98.73 ms | 0.157x | [0.150, 0.160] | 30 |
| `write.delete` | `write` | 176.60 ms | 93.48 ms | 0.519x | [0.504, 0.535] | 30 |
| `write.upsert` | `write` | 5.83 ms | 6.57 ms | 1.104x | [1.064, 1.150] | 30 |
| `txn.autocommit` | `transaction` | 137.74 ms | 120.68 ms | 0.870x | [0.837, 0.898] | 30 |
| `txn.batched` | `transaction` | 270.31 ms | 866.50 ms | 3.205x | [3.036, 3.275] | 30 |
| `txn.large` | `transaction` | 54.86 ms | 8.91 ms | 0.162x | [0.157, 0.167] | 30 |
| `schema.index` | `schema` | 31.54 ms | 45.71 ms | 1.442x | [1.419, 1.471] | 30 |
| `extension.json` | `extension` | 19.06 ms | 1.13 ms | 0.059x | [0.059, 0.063] | 30 |
| `extension.fts.build` | `extension` | 5.30 ms | 5.53 ms | 1.058x | [1.029, 1.212] | 30 |
| `extension.fts.query` | `extension` | 18.36 ms | 9.08 ms | 0.496x | [0.488, 0.501] | 30 |
| `extension.rtree.insert` | `extension` | 2.73 ms | 5.56 ms | 2.002x | [1.833, 2.071] | 30 |
| `extension.rtree.query` | `extension` | 4.47 ms | 4.11 ms | 0.918x | [0.914, 0.947] | 30 |
| `large.read` | `large.values` | 11.27 ms | 13.62 ms | 1.211x | [1.191, 1.217] | 30 |
| `large.write` | `large.values` | 1.46 ms | 4.28 ms | 2.976x | [2.176, 3.171] | 30 |


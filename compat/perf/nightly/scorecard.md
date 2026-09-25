# inillucent performance scorecard

Label `nightly-20260925-174312`, platform `windows-x86_64`, 30 paired rounds per scale, bootstrap seed 17900001. Every optimization is on, which is the shipped engine.

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

Weighted geometric mean **0.754x**, 95% interval [0.745, 0.760]. The release bound is a lower bound of at least 3.00x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.330x | [0.327, 0.332] | loss | **below 1.00x** |
| `read.point` | 0.16 | 1.481x | [1.467, 1.495] | win | met |
| `read.range` | 0.12 | 0.917x | [0.905, 0.930] | loss | **below 1.00x** |
| `read.analytical` | 0.10 | 0.308x | [0.307, 0.310] | loss | **below 1.00x** |
| `read.join` | 0.08 | 0.635x | [0.624, 0.645] | loss | **below 1.00x** |
| `write` | 0.20 | 0.747x | [0.720, 0.773] | loss | **below 1.00x** |
| `transaction` | 0.10 | 0.774x | [0.755, 0.792] | loss | **below 1.00x** |
| `schema` | 0.04 | 1.425x | [1.404, 1.445] | win | met |
| `extension` | 0.08 | 0.532x | [0.496, 0.554] | loss | **below 1.00x** |
| `large.values` | 0.04 | 1.941x | [1.815, 2.093] | win | met |

### By workload

| workload | family | inillucent median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 18.46 ms | 1.61 ms | 0.087x | [0.086, 0.088] | 30 |
| `prepare.point` | `open.prepare` | 35.62 ms | 44.53 ms | 1.250x | [1.232, 1.263] | 30 |
| `point.rowid` | `read.point` | 20.16 ms | 38.19 ms | 1.909x | [1.891, 1.938] | 30 |
| `point.index` | `read.point` | 25.37 ms | 29.94 ms | 1.181x | [1.170, 1.197] | 30 |
| `point.miss` | `read.point` | 18.49 ms | 26.40 ms | 1.441x | [1.416, 1.453] | 30 |
| `range.covering` | `read.range` | 19.72 ms | 10.83 ms | 0.545x | [0.542, 0.555] | 30 |
| `range.lookaside` | `read.range` | 37.54 ms | 33.89 ms | 0.909x | [0.877, 0.922] | 30 |
| `range.reverse` | `read.range` | 9.29 ms | 14.49 ms | 1.561x | [1.545, 1.585] | 30 |
| `scan.aggregate` | `read.analytical` | 228.48 ms | 91.15 ms | 0.401x | [0.398, 0.403] | 30 |
| `scan.group` | `read.analytical` | 206.73 ms | 76.09 ms | 0.366x | [0.364, 0.369] | 30 |
| `scan.sort` | `read.analytical` | 29.26 ms | 282.33 ms | 9.658x | [9.578, 9.762] | 30 |
| `scan.distinct` | `read.analytical` | 156.53 ms | 972.65 us | 0.006x | [0.006, 0.006] | 30 |
| `join.selective` | `read.join` | 18.37 ms | 20.19 ms | 1.107x | [1.070, 1.117] | 30 |
| `join.range` | `read.join` | 61.45 ms | 22.44 ms | 0.365x | [0.363, 0.374] | 30 |
| `correlated.exists` | `read.correlated` | 2.26 ms | 167.75 us | 0.074x | [0.074, 0.079] | 30 |
| `correlated.in` | `read.correlated` | 1.49 ms | 90.95 us | 0.062x | [0.060, 0.064] | 30 |
| `correlated.exists.selective` | `read.correlated` | 52.60 us | 17.00 us | 0.322x | [0.307, 0.336] | 30 |
| `correlated.scalar.selective` | `read.correlated` | 43.45 us | 16.05 us | 0.374x | [0.362, 0.386] | 30 |
| `write.insert.batch` | `write` | 18.00 ms | 19.76 ms | 1.073x | [0.757, 0.995] | 30 |
| `write.insert.autocommit` | `write` | 133.13 ms | 424.52 ms | 3.190x | [2.879, 3.246] | 30 |
| `write.update.indexed` | `write` | 621.28 ms | 98.97 ms | 0.158x | [0.154, 0.160] | 30 |
| `write.delete` | `write` | 177.89 ms | 92.05 ms | 0.520x | [0.511, 0.526] | 30 |
| `write.upsert` | `write` | 5.88 ms | 6.48 ms | 1.067x | [1.031, 1.089] | 30 |
| `txn.autocommit` | `transaction` | 137.98 ms | 120.15 ms | 0.868x | [0.860, 0.880] | 30 |
| `txn.batched` | `transaction` | 270.30 ms | 866.78 ms | 3.227x | [3.013, 3.408] | 30 |
| `txn.large` | `transaction` | 53.87 ms | 8.96 ms | 0.166x | [0.162, 0.170] | 30 |
| `schema.index` | `schema` | 31.88 ms | 45.30 ms | 1.428x | [1.404, 1.445] | 30 |
| `extension.json` | `extension` | 19.18 ms | 1.13 ms | 0.059x | [0.058, 0.062] | 30 |
| `extension.fts.build` | `extension` | 5.29 ms | 5.49 ms | 1.047x | [0.794, 1.047] | 30 |
| `extension.fts.query` | `extension` | 18.29 ms | 9.13 ms | 0.498x | [0.493, 0.504] | 30 |
| `extension.rtree.insert` | `extension` | 2.80 ms | 5.43 ms | 1.923x | [1.360, 1.860] | 30 |
| `extension.rtree.query` | `extension` | 4.50 ms | 4.11 ms | 0.909x | [0.889, 0.931] | 30 |
| `large.read` | `large.values` | 11.15 ms | 13.55 ms | 1.209x | [1.200, 1.223] | 30 |
| `large.write` | `large.values` | 1.46 ms | 4.79 ms | 3.296x | [2.728, 3.619] | 30 |


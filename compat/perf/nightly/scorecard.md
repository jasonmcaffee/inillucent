# inillucent performance scorecard

Label `nightly-20260925-082330`, platform `windows-x86_64`, 30 paired rounds per scale, bootstrap seed 17900001. Every optimization is on, which is the shipped engine.

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

Weighted geometric mean **0.751x**, 95% interval [0.740, 0.759]. The release bound is a lower bound of at least 3.00x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.330x | [0.326, 0.334] | loss | **below 1.00x** |
| `read.point` | 0.16 | 1.478x | [1.461, 1.494] | win | met |
| `read.range` | 0.12 | 0.903x | [0.893, 0.912] | loss | **below 1.00x** |
| `read.analytical` | 0.10 | 0.308x | [0.306, 0.311] | loss | **below 1.00x** |
| `read.join` | 0.08 | 0.636x | [0.627, 0.646] | loss | **below 1.00x** |
| `write` | 0.20 | 0.734x | [0.707, 0.762] | loss | **below 1.00x** |
| `transaction` | 0.10 | 0.739x | [0.712, 0.763] | loss | **below 1.00x** |
| `schema` | 0.04 | 1.572x | [1.457, 1.809] | win | met |
| `extension` | 0.08 | 0.546x | [0.538, 0.554] | loss | **below 1.00x** |
| `large.values` | 0.04 | 1.919x | [1.868, 1.972] | win | met |

### By workload

| workload | family | inillucent median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 18.50 ms | 1.62 ms | 0.087x | [0.086, 0.088] | 30 |
| `prepare.point` | `open.prepare` | 35.11 ms | 44.23 ms | 1.253x | [1.233, 1.268] | 30 |
| `point.rowid` | `read.point` | 19.57 ms | 38.78 ms | 1.947x | [1.929, 1.981] | 30 |
| `point.index` | `read.point` | 25.25 ms | 29.78 ms | 1.173x | [1.138, 1.184] | 30 |
| `point.miss` | `read.point` | 18.41 ms | 26.03 ms | 1.426x | [1.403, 1.435] | 30 |
| `range.covering` | `read.range` | 20.03 ms | 10.62 ms | 0.531x | [0.516, 0.537] | 30 |
| `range.lookaside` | `read.range` | 37.62 ms | 33.92 ms | 0.894x | [0.884, 0.910] | 30 |
| `range.reverse` | `read.range` | 9.27 ms | 14.44 ms | 1.553x | [1.532, 1.585] | 30 |
| `scan.aggregate` | `read.analytical` | 224.41 ms | 90.77 ms | 0.404x | [0.401, 0.408] | 30 |
| `scan.group` | `read.analytical` | 206.87 ms | 75.64 ms | 0.367x | [0.364, 0.370] | 30 |
| `scan.sort` | `read.analytical` | 29.02 ms | 282.22 ms | 9.681x | [9.577, 9.727] | 30 |
| `scan.distinct` | `read.analytical` | 157.88 ms | 986.00 us | 0.006x | [0.006, 0.006] | 30 |
| `join.selective` | `read.join` | 18.32 ms | 20.26 ms | 1.103x | [1.076, 1.128] | 30 |
| `join.range` | `read.join` | 61.62 ms | 22.45 ms | 0.363x | [0.362, 0.372] | 30 |
| `correlated.exists` | `read.correlated` | 2.28 ms | 169.45 us | 0.074x | [0.073, 0.081] | 30 |
| `correlated.in` | `read.correlated` | 1.74 ms | 89.70 us | 0.051x | [0.050, 0.054] | 30 |
| `correlated.exists.selective` | `read.correlated` | 56.00 us | 16.80 us | 0.302x | [0.286, 0.311] | 30 |
| `correlated.scalar.selective` | `read.correlated` | 44.55 us | 15.90 us | 0.360x | [0.354, 0.370] | 30 |
| `write.insert.batch` | `write` | 17.95 ms | 19.58 ms | 1.065x | [0.771, 1.024] | 30 |
| `write.insert.autocommit` | `write` | 134.25 ms | 428.48 ms | 3.160x | [2.875, 3.182] | 30 |
| `write.update.indexed` | `write` | 623.98 ms | 97.00 ms | 0.153x | [0.138, 0.155] | 30 |
| `write.delete` | `write` | 177.79 ms | 91.44 ms | 0.514x | [0.489, 0.524] | 30 |
| `write.upsert` | `write` | 6.03 ms | 6.51 ms | 1.082x | [1.007, 1.086] | 30 |
| `txn.autocommit` | `transaction` | 139.00 ms | 119.41 ms | 0.857x | [0.781, 0.854] | 30 |
| `txn.batched` | `transaction` | 275.46 ms | 862.85 ms | 3.177x | [2.918, 3.289] | 30 |
| `txn.large` | `transaction` | 53.84 ms | 8.65 ms | 0.160x | [0.154, 0.163] | 30 |
| `schema.index` | `schema` | 31.34 ms | 46.17 ms | 1.473x | [1.457, 1.809] | 30 |
| `extension.json` | `extension` | 19.08 ms | 1.13 ms | 0.059x | [0.059, 0.060] | 30 |
| `extension.fts.build` | `extension` | 5.38 ms | 5.56 ms | 1.038x | [0.992, 1.057] | 30 |
| `extension.fts.query` | `extension` | 18.51 ms | 9.14 ms | 0.493x | [0.488, 0.501] | 30 |
| `extension.rtree.insert` | `extension` | 2.80 ms | 5.50 ms | 1.948x | [1.677, 1.902] | 30 |
| `extension.rtree.query` | `extension` | 4.49 ms | 4.05 ms | 0.904x | [0.887, 0.912] | 30 |
| `large.read` | `large.values` | 11.23 ms | 13.44 ms | 1.195x | [1.172, 1.197] | 30 |
| `large.write` | `large.values` | 1.51 ms | 5.06 ms | 3.243x | [2.939, 3.279] | 30 |


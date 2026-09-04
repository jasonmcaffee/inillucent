# rust-db performance scorecard

Label `release-candidate (no ordered-walk)`, platform `windows-x86_64`, 30 paired rounds per scale, bootstrap seed 17900001. **Arm: `ordered-walk` switched off.** This is one side of an A/B pair and not the shipped engine; compare it with the run whose arm is empty.

Both engines read the same plan file. The ratio is SQLite over rust-db, so **above one means rust-db is faster**. A workload whose two engines returned different answers is reported as a correctness failure and is not timed.

## Fair configuration

| setting | value |
|---|---|
| journal mode | `delete` |
| synchronous | `full` |
| page size | 4096 |
| cache | -2000 pages-or-KiB (SQLite units) |
| statement reuse | prepared once except the `open.prepare` family |
| database | on disk, cloned from one pristine image per round |

## Scale `small` - 5000 rows

Weighted geometric mean **0.213x**, 95% interval [0.209, 0.221]. The release bound is a lower bound of at least 1.50x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.211x | [0.143, 0.314] | loss | **below 0.90x** |
| `read.point` | 0.16 | 0.874x | [0.764, 0.998] | loss | **below 0.90x** |
| `read.range` | 0.12 | 0.065x | [0.053, 0.079] | loss | **below 0.90x** |
| `read.analytical` | 0.10 | 0.064x | [0.057, 0.071] | loss | **below 0.90x** |
| `read.join` | 0.08 | 0.281x | [0.203, 0.388] | loss | **below 0.90x** |
| `write` | 0.20 | 0.210x | [0.179, 0.248] | loss | **below 0.90x** |
| `transaction` | 0.10 | 0.302x | [0.221, 0.411] | loss | **below 0.90x** |
| `schema` | 0.04 | 0.216x | [0.205, 0.228] | loss | **below 0.90x** |
| `extension` | 0.08 | 0.100x | [0.080, 0.126] | loss | **below 0.90x** |
| `large.values` | 0.04 | 0.777x | [0.681, 0.859] | loss | **below 0.90x** |

### By workload

| workload | family | rust-db median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 38.28 ms | 1.93 ms | 0.050x | [0.043, 0.051] | 30 |
| `prepare.point` | `open.prepare` | 81.51 ms | 76.65 ms | 0.939x | [0.888, 1.010] | 30 |
| `point.rowid` | `read.point` | 35.74 ms | 65.26 ms | 1.744x | [1.639, 1.956] | 30 |
| `point.index` | `read.point` | 67.93 ms | 61.88 ms | 0.873x | [0.847, 1.005] | 30 |
| `point.miss` | `read.point` | 140.89 ms | 58.26 ms | 0.404x | [0.378, 0.433] | 30 |
| `range.covering` | `read.range` | 108.30 ms | 17.78 ms | 0.163x | [0.166, 0.195] | 30 |
| `range.lookaside` | `read.range` | 272.50 ms | 21.99 ms | 0.080x | [0.078, 0.087] | 30 |
| `range.reverse` | `read.range` | 951.27 ms | 16.83 ms | 0.016x | [0.017, 0.020] | 30 |
| `scan.aggregate` | `read.analytical` | 639.14 ms | 55.18 ms | 0.084x | [0.083, 0.088] | 30 |
| `scan.group` | `read.analytical` | 452.71 ms | 51.16 ms | 0.111x | [0.108, 0.114] | 30 |
| `scan.sort` | `read.analytical` | 1.31 s | 101.99 ms | 0.077x | [0.073, 0.079] | 30 |
| `scan.distinct` | `read.analytical` | 472.26 ms | 11.27 ms | 0.024x | [0.021, 0.025] | 30 |
| `join.selective` | `read.join` | 33.49 ms | 29.26 ms | 0.916x | [0.867, 1.059] | 30 |
| `join.range` | `read.join` | 255.20 ms | 20.82 ms | 0.079x | [0.077, 0.088] | 30 |
| `write.insert.batch` | `write` | 94.17 ms | 7.04 ms | 0.070x | [0.065, 0.082] | 30 |
| `write.insert.autocommit` | `write` | 134.32 ms | 133.34 ms | 0.990x | [0.948, 1.058] | 30 |
| `write.update.indexed` | `write` | 80.37 ms | 7.56 ms | 0.095x | [0.092, 0.098] | 30 |
| `write.delete` | `write` | 48.52 ms | 6.43 ms | 0.131x | [0.127, 0.137] | 30 |
| `write.upsert` | `write` | 9.90 ms | 4.55 ms | 0.471x | [0.426, 0.494] | 30 |
| `txn.autocommit` | `transaction` | 42.15 ms | 39.29 ms | 0.909x | [0.890, 0.961] | 30 |
| `txn.batched` | `transaction` | 331.24 ms | 270.68 ms | 0.826x | [0.765, 0.834] | 30 |
| `txn.large` | `transaction` | 20.06 ms | 733.65 us | 0.036x | [0.036, 0.039] | 30 |
| `schema.index` | `schema` | 15.03 ms | 3.28 ms | 0.217x | [0.205, 0.228] | 30 |
| `extension.json` | `extension` | 30.06 ms | 1.18 ms | 0.040x | [0.036, 0.042] | 30 |
| `extension.fts.build` | `extension` | 135.03 ms | 2.59 ms | 0.018x | [0.018, 0.020] | 30 |
| `extension.fts.query` | `extension` | 268.45 ms | 13.22 ms | 0.052x | [0.051, 0.059] | 30 |
| `extension.rtree.insert` | `extension` | 9.80 ms | 2.47 ms | 0.243x | [0.238, 0.268] | 30 |
| `extension.rtree.query` | `extension` | 8.39 ms | 10.29 ms | 0.941x | [0.867, 1.106] | 30 |
| `large.read` | `large.values` | 35.12 ms | 31.68 ms | 0.896x | [0.838, 0.991] | 30 |
| `large.write` | `large.values` | 3.22 ms | 2.31 ms | 0.774x | [0.529, 0.766] | 30 |


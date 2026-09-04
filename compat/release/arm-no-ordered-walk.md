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

Weighted geometric mean **0.207x**, 95% interval [0.200, 0.216]. The release bound is a lower bound of at least 1.50x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.216x | [0.145, 0.327] | loss | **below 0.90x** |
| `read.point` | 0.16 | 0.902x | [0.788, 1.031] | inconclusive | **below 0.90x** |
| `read.range` | 0.12 | 0.063x | [0.051, 0.078] | loss | **below 0.90x** |
| `read.analytical` | 0.10 | 0.055x | [0.049, 0.062] | loss | **below 0.90x** |
| `read.join` | 0.08 | 0.295x | [0.210, 0.412] | loss | **below 0.90x** |
| `write` | 0.20 | 0.195x | [0.165, 0.230] | loss | **below 0.90x** |
| `transaction` | 0.10 | 0.287x | [0.209, 0.391] | loss | **below 0.90x** |
| `schema` | 0.04 | 0.190x | [0.173, 0.207] | loss | **below 0.90x** |
| `extension` | 0.08 | 0.096x | [0.077, 0.121] | loss | **below 0.90x** |
| `large.values` | 0.04 | 0.849x | [0.685, 1.064] | inconclusive | **below 0.90x** |

### By workload

| workload | family | rust-db median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 44.18 ms | 2.15 ms | 0.048x | [0.044, 0.054] | 30 |
| `prepare.point` | `open.prepare` | 93.39 ms | 102.19 ms | 1.074x | [0.847, 1.075] | 30 |
| `point.rowid` | `read.point` | 49.27 ms | 85.83 ms | 1.788x | [1.578, 1.987] | 30 |
| `point.index` | `read.point` | 86.89 ms | 94.63 ms | 0.879x | [0.868, 1.018] | 30 |
| `point.miss` | `read.point` | 170.59 ms | 84.48 ms | 0.421x | [0.396, 0.486] | 30 |
| `range.covering` | `read.range` | 145.02 ms | 29.34 ms | 0.183x | [0.167, 0.199] | 30 |
| `range.lookaside` | `read.range` | 371.92 ms | 30.21 ms | 0.073x | [0.070, 0.084] | 30 |
| `range.reverse` | `read.range` | 1.24 s | 26.71 ms | 0.016x | [0.016, 0.020] | 30 |
| `scan.aggregate` | `read.analytical` | 876.17 ms | 70.95 ms | 0.079x | [0.073, 0.086] | 30 |
| `scan.group` | `read.analytical` | 765.59 ms | 61.68 ms | 0.083x | [0.075, 0.090] | 30 |
| `scan.sort` | `read.analytical` | 1.67 s | 129.02 ms | 0.075x | [0.067, 0.079] | 30 |
| `scan.distinct` | `read.analytical` | 663.32 ms | 13.83 ms | 0.018x | [0.017, 0.021] | 30 |
| `join.selective` | `read.join` | 40.83 ms | 42.62 ms | 0.959x | [0.905, 1.178] | 30 |
| `join.range` | `read.join` | 304.18 ms | 27.21 ms | 0.080x | [0.078, 0.093] | 30 |
| `write.insert.batch` | `write` | 112.79 ms | 7.97 ms | 0.071x | [0.064, 0.090] | 30 |
| `write.insert.autocommit` | `write` | 148.47 ms | 147.14 ms | 0.977x | [0.955, 1.078] | 30 |
| `write.update.indexed` | `write` | 91.58 ms | 8.38 ms | 0.087x | [0.074, 0.087] | 30 |
| `write.delete` | `write` | 59.23 ms | 7.18 ms | 0.112x | [0.103, 0.121] | 30 |
| `write.upsert` | `write` | 10.79 ms | 4.45 ms | 0.432x | [0.400, 0.435] | 30 |
| `txn.autocommit` | `transaction` | 47.47 ms | 41.96 ms | 0.882x | [0.826, 0.913] | 30 |
| `txn.batched` | `transaction` | 372.43 ms | 302.57 ms | 0.800x | [0.732, 0.835] | 30 |
| `txn.large` | `transaction` | 22.29 ms | 820.90 us | 0.036x | [0.032, 0.037] | 30 |
| `schema.index` | `schema` | 17.45 ms | 3.71 ms | 0.199x | [0.173, 0.207] | 30 |
| `extension.json` | `extension` | 38.96 ms | 1.39 ms | 0.037x | [0.034, 0.040] | 30 |
| `extension.fts.build` | `extension` | 166.08 ms | 2.93 ms | 0.018x | [0.016, 0.019] | 30 |
| `extension.fts.query` | `extension` | 318.06 ms | 19.63 ms | 0.051x | [0.051, 0.063] | 30 |
| `extension.rtree.insert` | `extension` | 10.82 ms | 2.82 ms | 0.249x | [0.233, 0.264] | 30 |
| `extension.rtree.query` | `extension` | 12.46 ms | 11.77 ms | 0.878x | [0.845, 1.050] | 30 |
| `large.read` | `large.values` | 44.73 ms | 40.07 ms | 0.860x | [0.819, 0.999] | 30 |
| `large.write` | `large.values` | 3.76 ms | 2.58 ms | 0.711x | [0.527, 1.229] | 30 |


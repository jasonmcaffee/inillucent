# rust-db performance scorecard

Label `release-candidate (no covering-index)`, platform `windows-x86_64`, 30 paired rounds per scale, bootstrap seed 17900001. **Arm: `covering-index` switched off.** This is one side of an A/B pair and not the shipped engine; compare it with the run whose arm is empty.

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

Weighted geometric mean **0.170x**, 95% interval [0.167, 0.174]. The release bound is a lower bound of at least 1.50x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.192x | [0.131, 0.284] | loss | **below 0.90x** |
| `read.point` | 0.16 | 0.820x | [0.713, 0.942] | loss | **below 0.90x** |
| `read.range` | 0.12 | 0.035x | [0.030, 0.040] | loss | **below 0.90x** |
| `read.analytical` | 0.10 | 0.024x | [0.021, 0.028] | loss | **below 0.90x** |
| `read.join` | 0.08 | 0.192x | [0.130, 0.279] | loss | **below 0.90x** |
| `write` | 0.20 | 0.219x | [0.189, 0.255] | loss | **below 0.90x** |
| `transaction` | 0.10 | 0.341x | [0.253, 0.454] | loss | **below 0.90x** |
| `schema` | 0.04 | 0.103x | [0.092, 0.124] | loss | **below 0.90x** |
| `extension` | 0.08 | 0.095x | [0.077, 0.118] | loss | **below 0.90x** |
| `large.values` | 0.04 | 0.773x | [0.656, 0.900] | loss | **below 0.90x** |

### By workload

| workload | family | rust-db median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 39.41 ms | 1.90 ms | 0.043x | [0.041, 0.046] | 30 |
| `prepare.point` | `open.prepare` | 75.56 ms | 63.27 ms | 0.813x | [0.774, 0.937] | 30 |
| `point.rowid` | `read.point` | 32.44 ms | 50.91 ms | 1.554x | [1.453, 1.835] | 30 |
| `point.index` | `read.point` | 66.46 ms | 51.55 ms | 0.788x | [0.795, 0.932] | 30 |
| `point.miss` | `read.point` | 137.26 ms | 50.35 ms | 0.367x | [0.364, 0.426] | 30 |
| `range.covering` | `read.range` | 538.74 ms | 16.12 ms | 0.031x | [0.031, 0.037] | 30 |
| `range.lookaside` | `read.range` | 278.47 ms | 20.98 ms | 0.078x | [0.075, 0.081] | 30 |
| `range.reverse` | `read.range` | 1.08 s | 14.77 ms | 0.014x | [0.015, 0.018] | 30 |
| `scan.aggregate` | `read.analytical` | 1.39 s | 53.47 ms | 0.039x | [0.038, 0.041] | 30 |
| `scan.group` | `read.analytical` | 1.55 s | 47.86 ms | 0.031x | [0.030, 0.033] | 30 |
| `scan.sort` | `read.analytical` | 1.93 s | 95.16 ms | 0.049x | [0.048, 0.051] | 30 |
| `scan.distinct` | `read.analytical` | 1.76 s | 8.86 ms | 0.005x | [0.005, 0.006] | 30 |
| `join.selective` | `read.join` | 34.85 ms | 26.01 ms | 0.787x | [0.756, 0.890] | 30 |
| `join.range` | `read.join` | 457.68 ms | 19.19 ms | 0.043x | [0.042, 0.048] | 30 |
| `write.insert.batch` | `write` | 74.22 ms | 7.06 ms | 0.097x | [0.086, 0.115] | 30 |
| `write.insert.autocommit` | `write` | 139.59 ms | 134.49 ms | 0.995x | [0.950, 1.106] | 30 |
| `write.update.indexed` | `write` | 79.51 ms | 7.44 ms | 0.094x | [0.092, 0.097] | 30 |
| `write.delete` | `write` | 49.13 ms | 6.52 ms | 0.134x | [0.128, 0.138] | 30 |
| `write.upsert` | `write` | 9.94 ms | 4.14 ms | 0.412x | [0.386, 0.432] | 30 |
| `txn.autocommit` | `transaction` | 40.32 ms | 39.61 ms | 0.982x | [0.916, 1.010] | 30 |
| `txn.batched` | `transaction` | 319.42 ms | 266.04 ms | 0.842x | [0.814, 0.930] | 30 |
| `txn.large` | `transaction` | 14.38 ms | 698.60 us | 0.049x | [0.042, 0.053] | 30 |
| `schema.index` | `schema` | 32.97 ms | 3.16 ms | 0.096x | [0.092, 0.124] | 30 |
| `extension.json` | `extension` | 27.95 ms | 1.13 ms | 0.040x | [0.037, 0.041] | 30 |
| `extension.fts.build` | `extension` | 128.96 ms | 2.54 ms | 0.020x | [0.019, 0.021] | 30 |
| `extension.fts.query` | `extension` | 258.96 ms | 12.59 ms | 0.050x | [0.051, 0.058] | 30 |
| `extension.rtree.insert` | `extension` | 9.76 ms | 2.32 ms | 0.239x | [0.183, 0.247] | 30 |
| `extension.rtree.query` | `extension` | 8.37 ms | 7.07 ms | 0.846x | [0.774, 0.962] | 30 |
| `large.read` | `large.values` | 32.22 ms | 24.49 ms | 0.809x | [0.763, 0.948] | 30 |
| `large.write` | `large.values` | 3.36 ms | 2.50 ms | 0.698x | [0.520, 0.931] | 30 |


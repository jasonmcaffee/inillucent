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

Weighted geometric mean **0.216x**, 95% interval [0.206, 0.227]. The release bound is a lower bound of at least 1.50x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.212x | [0.141, 0.322] | loss | **below 0.90x** |
| `read.point` | 0.16 | 0.891x | [0.780, 1.018] | inconclusive | **below 0.90x** |
| `read.range` | 0.12 | 0.144x | [0.109, 0.190] | loss | **below 0.90x** |
| `read.analytical` | 0.10 | 0.035x | [0.030, 0.041] | loss | **below 0.90x** |
| `read.join` | 0.08 | 0.208x | [0.140, 0.305] | loss | **below 0.90x** |
| `write` | 0.20 | 0.207x | [0.176, 0.242] | loss | **below 0.90x** |
| `transaction` | 0.10 | 0.297x | [0.217, 0.405] | loss | **below 0.90x** |
| `schema` | 0.04 | 0.207x | [0.196, 0.217] | loss | **below 0.90x** |
| `extension` | 0.08 | 0.098x | [0.078, 0.123] | loss | **below 0.90x** |
| `large.values` | 0.04 | 0.802x | [0.740, 0.872] | loss | **below 0.90x** |

### By workload

| workload | family | rust-db median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 49.21 ms | 1.95 ms | 0.044x | [0.041, 0.049] | 30 |
| `prepare.point` | `open.prepare` | 82.57 ms | 84.20 ms | 1.073x | [0.894, 1.125] | 30 |
| `point.rowid` | `read.point` | 45.29 ms | 65.35 ms | 1.546x | [1.402, 1.836] | 30 |
| `point.index` | `read.point` | 73.36 ms | 75.94 ms | 0.881x | [0.860, 1.108] | 30 |
| `point.miss` | `read.point` | 154.73 ms | 74.35 ms | 0.424x | [0.400, 0.509] | 30 |
| `range.covering` | `read.range` | 570.17 ms | 27.31 ms | 0.038x | [0.034, 0.044] | 30 |
| `range.lookaside` | `read.range` | 313.52 ms | 28.95 ms | 0.085x | [0.078, 0.097] | 30 |
| `range.reverse` | `read.range` | 24.59 ms | 25.17 ms | 0.834x | [0.778, 0.986] | 30 |
| `scan.aggregate` | `read.analytical` | 1.02 s | 62.83 ms | 0.055x | [0.053, 0.063] | 30 |
| `scan.group` | `read.analytical` | 1.16 s | 51.54 ms | 0.044x | [0.041, 0.049] | 30 |
| `scan.sort` | `read.analytical` | 1.50 s | 106.05 ms | 0.070x | [0.064, 0.078] | 30 |
| `scan.distinct` | `read.analytical` | 1.34 s | 13.45 ms | 0.009x | [0.008, 0.010] | 30 |
| `join.selective` | `read.join` | 38.46 ms | 40.44 ms | 0.903x | [0.845, 1.088] | 30 |
| `join.range` | `read.join` | 508.78 ms | 24.44 ms | 0.048x | [0.042, 0.049] | 30 |
| `write.insert.batch` | `write` | 98.38 ms | 7.61 ms | 0.079x | [0.067, 0.081] | 30 |
| `write.insert.autocommit` | `write` | 139.34 ms | 139.64 ms | 0.997x | [0.948, 1.014] | 30 |
| `write.update.indexed` | `write` | 83.96 ms | 8.05 ms | 0.095x | [0.088, 0.096] | 30 |
| `write.delete` | `write` | 50.88 ms | 6.99 ms | 0.135x | [0.122, 0.135] | 30 |
| `write.upsert` | `write` | 10.17 ms | 4.59 ms | 0.412x | [0.411, 0.475] | 30 |
| `txn.autocommit` | `transaction` | 43.93 ms | 41.96 ms | 0.911x | [0.872, 0.964] | 30 |
| `txn.batched` | `transaction` | 349.67 ms | 279.48 ms | 0.791x | [0.737, 0.844] | 30 |
| `txn.large` | `transaction` | 20.88 ms | 740.50 us | 0.035x | [0.035, 0.038] | 30 |
| `schema.index` | `schema` | 15.88 ms | 3.38 ms | 0.211x | [0.196, 0.217] | 30 |
| `extension.json` | `extension` | 37.58 ms | 1.21 ms | 0.039x | [0.035, 0.040] | 30 |
| `extension.fts.build` | `extension` | 140.25 ms | 2.79 ms | 0.019x | [0.014, 0.019] | 30 |
| `extension.fts.query` | `extension` | 272.60 ms | 18.42 ms | 0.058x | [0.053, 0.062] | 30 |
| `extension.rtree.insert` | `extension` | 10.30 ms | 2.53 ms | 0.249x | [0.234, 0.262] | 30 |
| `extension.rtree.query` | `extension` | 12.46 ms | 12.50 ms | 0.989x | [0.873, 1.072] | 30 |
| `large.read` | `large.values` | 43.32 ms | 39.87 ms | 0.950x | [0.800, 1.036] | 30 |
| `large.write` | `large.values` | 3.47 ms | 2.47 ms | 0.724x | [0.643, 0.781] | 30 |


# rust-db performance scorecard

Label `release-candidate (no indexed-write)`, platform `windows-x86_64`, 30 paired rounds per scale, bootstrap seed 17900001. **Arm: `indexed-write` switched off.** This is one side of an A/B pair and not the shipped engine; compare it with the run whose arm is empty.

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

Weighted geometric mean **0.166x**, 95% interval [0.161, 0.169]. The release bound is a lower bound of at least 1.50x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.221x | [0.150, 0.330] | loss | **below 0.90x** |
| `read.point` | 0.16 | 0.820x | [0.716, 0.948] | loss | **below 0.90x** |
| `read.range` | 0.12 | 0.210x | [0.173, 0.256] | loss | **below 0.90x** |
| `read.analytical` | 0.10 | 0.054x | [0.048, 0.061] | loss | **below 0.90x** |
| `read.join` | 0.08 | 0.276x | [0.197, 0.380] | loss | **below 0.90x** |
| `write` | 0.20 | 0.060x | [0.043, 0.085] | loss | **below 0.90x** |
| `transaction` | 0.10 | 0.092x | [0.054, 0.156] | loss | **below 0.90x** |
| `schema` | 0.04 | 0.201x | [0.189, 0.214] | loss | **below 0.90x** |
| `extension` | 0.08 | 0.103x | [0.083, 0.130] | loss | **below 0.90x** |
| `large.values` | 0.04 | 0.546x | [0.444, 0.659] | loss | **below 0.90x** |

### By workload

| workload | family | rust-db median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 36.35 ms | 1.91 ms | 0.050x | [0.046, 0.052] | 30 |
| `prepare.point` | `open.prepare` | 73.68 ms | 76.26 ms | 1.000x | [0.905, 1.096] | 30 |
| `point.rowid` | `read.point` | 33.03 ms | 56.34 ms | 1.584x | [1.513, 1.905] | 30 |
| `point.index` | `read.point` | 67.30 ms | 51.88 ms | 0.807x | [0.778, 0.930] | 30 |
| `point.miss` | `read.point` | 144.26 ms | 51.69 ms | 0.375x | [0.353, 0.410] | 30 |
| `range.covering` | `read.range` | 112.68 ms | 17.07 ms | 0.144x | [0.150, 0.173] | 30 |
| `range.lookaside` | `read.range` | 277.63 ms | 20.68 ms | 0.078x | [0.075, 0.082] | 30 |
| `range.reverse` | `read.range` | 22.22 ms | 14.65 ms | 0.727x | [0.671, 0.796] | 30 |
| `scan.aggregate` | `read.analytical` | 706.37 ms | 50.98 ms | 0.074x | [0.073, 0.077] | 30 |
| `scan.group` | `read.analytical` | 622.61 ms | 46.51 ms | 0.077x | [0.076, 0.081] | 30 |
| `scan.sort` | `read.analytical` | 1.13 s | 93.74 ms | 0.085x | [0.083, 0.089] | 30 |
| `scan.distinct` | `read.analytical` | 556.29 ms | 9.05 ms | 0.016x | [0.016, 0.019] | 30 |
| `join.selective` | `read.join` | 27.44 ms | 25.24 ms | 0.955x | [0.903, 1.069] | 30 |
| `join.range` | `read.join` | 252.43 ms | 18.18 ms | 0.073x | [0.073, 0.082] | 30 |
| `write.insert.batch` | `write` | 90.59 ms | 6.22 ms | 0.067x | [0.064, 0.085] | 30 |
| `write.insert.autocommit` | `write` | 134.74 ms | 128.94 ms | 0.976x | [0.890, 1.054] | 30 |
| `write.update.indexed` | `write` | 1.38 s | 7.63 ms | 0.006x | [0.005, 0.006] | 30 |
| `write.delete` | `write` | 1.17 s | 6.52 ms | 0.006x | [0.005, 0.006] | 30 |
| `write.upsert` | `write` | 10.12 ms | 3.95 ms | 0.390x | [0.359, 0.418] | 30 |
| `txn.autocommit` | `transaction` | 57.90 ms | 38.01 ms | 0.657x | [0.628, 0.681] | 30 |
| `txn.batched` | `transaction` | 570.57 ms | 265.97 ms | 0.451x | [0.435, 0.463] | 30 |
| `txn.large` | `transaction` | 266.41 ms | 716.00 us | 0.003x | [0.003, 0.003] | 30 |
| `schema.index` | `schema` | 15.44 ms | 3.12 ms | 0.203x | [0.189, 0.214] | 30 |
| `extension.json` | `extension` | 27.81 ms | 1.19 ms | 0.040x | [0.036, 0.042] | 30 |
| `extension.fts.build` | `extension` | 133.39 ms | 2.68 ms | 0.019x | [0.019, 0.021] | 30 |
| `extension.fts.query` | `extension` | 254.56 ms | 17.12 ms | 0.067x | [0.056, 0.065] | 30 |
| `extension.rtree.insert` | `extension` | 9.85 ms | 2.42 ms | 0.246x | [0.236, 0.265] | 30 |
| `extension.rtree.query` | `extension` | 8.73 ms | 12.19 ms | 0.975x | [0.877, 1.140] | 30 |
| `large.read` | `large.values` | 31.76 ms | 39.49 ms | 1.086x | [0.918, 1.180] | 30 |
| `large.write` | `large.values` | 7.98 ms | 2.43 ms | 0.289x | [0.239, 0.328] | 30 |


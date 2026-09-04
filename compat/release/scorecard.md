# rust-db performance scorecard

Label `release-candidate`, platform `windows-x86_64`, 30 paired rounds per scale, bootstrap seed 17900001. Every optimization is on, which is the shipped engine.

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

Weighted geometric mean **0.205x**, 95% interval [0.200, 0.208]. The release bound is a lower bound of at least 1.50x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.198x | [0.134, 0.294] | loss | **below 0.90x** |
| `read.point` | 0.16 | 0.833x | [0.727, 0.955] | loss | **below 0.90x** |
| `read.range` | 0.12 | 0.062x | [0.050, 0.076] | loss | **below 0.90x** |
| `read.analytical` | 0.10 | 0.052x | [0.047, 0.058] | loss | **below 0.90x** |
| `read.join` | 0.08 | 0.288x | [0.206, 0.401] | loss | **below 0.90x** |
| `write` | 0.20 | 0.218x | [0.189, 0.252] | loss | **below 0.90x** |
| `transaction` | 0.10 | 0.348x | [0.257, 0.468] | loss | **below 0.90x** |
| `schema` | 0.04 | 0.091x | [0.088, 0.094] | loss | **below 0.90x** |
| `extension` | 0.08 | 0.096x | [0.077, 0.120] | loss | **below 0.90x** |
| `large.values` | 0.04 | 0.820x | [0.723, 0.911] | loss | **below 0.90x** |

### By workload

| workload | family | rust-db median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 40.05 ms | 1.91 ms | 0.046x | [0.042, 0.046] | 30 |
| `prepare.point` | `open.prepare` | 75.85 ms | 71.54 ms | 0.895x | [0.819, 0.962] | 30 |
| `point.rowid` | `read.point` | 31.74 ms | 51.96 ms | 1.602x | [1.551, 1.849] | 30 |
| `point.index` | `read.point` | 66.44 ms | 56.64 ms | 0.853x | [0.851, 0.982] | 30 |
| `point.miss` | `read.point` | 137.44 ms | 49.30 ms | 0.367x | [0.351, 0.401] | 30 |
| `range.covering` | `read.range` | 103.95 ms | 16.35 ms | 0.158x | [0.163, 0.189] | 30 |
| `range.lookaside` | `read.range` | 295.87 ms | 22.05 ms | 0.074x | [0.075, 0.085] | 30 |
| `range.reverse` | `read.range` | 1.09 s | 18.00 ms | 0.016x | [0.015, 0.018] | 30 |
| `scan.aggregate` | `read.analytical` | 702.36 ms | 57.34 ms | 0.080x | [0.077, 0.082] | 30 |
| `scan.group` | `read.analytical` | 619.49 ms | 48.46 ms | 0.078x | [0.077, 0.082] | 30 |
| `scan.sort` | `read.analytical` | 1.47 s | 97.88 ms | 0.064x | [0.064, 0.068] | 30 |
| `scan.distinct` | `read.analytical` | 553.48 ms | 9.25 ms | 0.017x | [0.017, 0.019] | 30 |
| `join.selective` | `read.join` | 28.16 ms | 28.19 ms | 0.975x | [0.931, 1.129] | 30 |
| `join.range` | `read.join` | 245.18 ms | 18.81 ms | 0.078x | [0.078, 0.085] | 30 |
| `write.insert.batch` | `write` | 76.95 ms | 7.56 ms | 0.098x | [0.091, 0.120] | 30 |
| `write.insert.autocommit` | `write` | 136.23 ms | 131.00 ms | 0.971x | [0.905, 1.016] | 30 |
| `write.update.indexed` | `write` | 77.96 ms | 7.54 ms | 0.097x | [0.092, 0.098] | 30 |
| `write.delete` | `write` | 49.08 ms | 6.31 ms | 0.132x | [0.124, 0.137] | 30 |
| `write.upsert` | `write` | 9.56 ms | 3.99 ms | 0.407x | [0.383, 0.433] | 30 |
| `txn.autocommit` | `transaction` | 40.73 ms | 39.94 ms | 1.004x | [0.958, 1.160] | 30 |
| `txn.batched` | `transaction` | 314.50 ms | 262.64 ms | 0.839x | [0.790, 0.915] | 30 |
| `txn.large` | `transaction` | 14.20 ms | 667.30 us | 0.048x | [0.047, 0.049] | 30 |
| `schema.index` | `schema` | 34.67 ms | 3.18 ms | 0.091x | [0.088, 0.094] | 30 |
| `extension.json` | `extension` | 30.42 ms | 1.18 ms | 0.040x | [0.035, 0.041] | 30 |
| `extension.fts.build` | `extension` | 129.09 ms | 2.57 ms | 0.019x | [0.018, 0.020] | 30 |
| `extension.fts.query` | `extension` | 260.43 ms | 12.44 ms | 0.048x | [0.050, 0.058] | 30 |
| `extension.rtree.insert` | `extension` | 10.04 ms | 2.42 ms | 0.242x | [0.229, 0.254] | 30 |
| `extension.rtree.query` | `extension` | 9.13 ms | 6.99 ms | 0.854x | [0.746, 1.006] | 30 |
| `large.read` | `large.values` | 30.27 ms | 31.42 ms | 0.893x | [0.879, 1.100] | 30 |
| `large.write` | `large.values` | 3.26 ms | 2.37 ms | 0.741x | [0.552, 0.791] | 30 |

## Scale `medium` - 100000 rows

Weighted geometric mean **0.137x**, 95% interval [0.134, 0.144]. The release bound is a lower bound of at least 1.50x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.181x | [0.124, 0.265] | loss | **below 0.90x** |
| `read.point` | 0.16 | 0.664x | [0.579, 0.761] | loss | **below 0.90x** |
| `read.range` | 0.12 | 0.025x | [0.016, 0.040] | loss | **below 0.90x** |
| `read.analytical` | 0.10 | 0.029x | [0.021, 0.039] | loss | **below 0.90x** |
| `read.join` | 0.08 | 0.177x | [0.122, 0.254] | loss | **below 0.90x** |
| `write` | 0.20 | 0.165x | [0.136, 0.201] | loss | **below 0.90x** |
| `transaction` | 0.10 | 0.352x | [0.258, 0.484] | loss | **below 0.90x** |
| `schema` | 0.04 | 0.010x | [0.009, 0.011] | loss | **below 0.90x** |
| `extension` | 0.08 | 0.101x | [0.082, 0.126] | loss | **below 0.90x** |
| `large.values` | 0.04 | 0.832x | [0.708, 0.981] | loss | **below 0.90x** |

### By workload

| workload | family | rust-db median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 45.48 ms | 1.91 ms | 0.044x | [0.039, 0.044] | 30 |
| `prepare.point` | `open.prepare` | 119.56 ms | 90.01 ms | 0.775x | [0.713, 0.858] | 30 |
| `point.rowid` | `read.point` | 62.95 ms | 87.64 ms | 1.320x | [1.192, 1.441] | 30 |
| `point.index` | `read.point` | 93.04 ms | 70.58 ms | 0.723x | [0.678, 0.810] | 30 |
| `point.miss` | `read.point` | 200.52 ms | 58.21 ms | 0.289x | [0.276, 0.331] | 30 |
| `range.covering` | `read.range` | 115.09 ms | 17.55 ms | 0.149x | [0.153, 0.182] | 30 |
| `range.lookaside` | `read.range` | 532.59 ms | 40.36 ms | 0.078x | [0.074, 0.088] | 30 |
| `range.reverse` | `read.range` | 21.22 s | 23.83 ms | 0.001x | [0.001, 0.001] | 30 |
| `scan.aggregate` | `read.analytical` | 1.44 s | 101.47 ms | 0.069x | [0.064, 0.073] | 30 |
| `scan.group` | `read.analytical` | 1.27 s | 81.71 ms | 0.063x | [0.062, 0.069] | 30 |
| `scan.sort` | `read.analytical` | 2.83 s | 355.88 ms | 0.124x | [0.117, 0.134] | 30 |
| `scan.distinct` | `read.analytical` | 1.12 s | 1.30 ms | 0.001x | [0.001, 0.001] | 30 |
| `join.selective` | `read.join` | 53.41 ms | 34.85 ms | 0.668x | [0.645, 0.776] | 30 |
| `join.range` | `read.join` | 641.47 ms | 27.77 ms | 0.042x | [0.041, 0.048] | 30 |
| `write.insert.batch` | `write` | 95.85 ms | 17.08 ms | 0.146x | [0.098, 0.146] | 30 |
| `write.insert.autocommit` | `write` | 140.85 ms | 129.76 ms | 0.953x | [0.886, 1.034] | 30 |
| `write.update.indexed` | `write` | 1.82 s | 101.12 ms | 0.053x | [0.048, 0.055] | 30 |
| `write.delete` | `write` | 2.15 s | 99.32 ms | 0.043x | [0.041, 0.050] | 30 |
| `write.upsert` | `write` | 10.04 ms | 4.86 ms | 0.460x | [0.427, 0.501] | 30 |
| `txn.autocommit` | `transaction` | 36.66 ms | 42.84 ms | 1.154x | [1.107, 1.377] | 30 |
| `txn.batched` | `transaction` | 318.81 ms | 263.50 ms | 0.836x | [0.794, 0.881] | 30 |
| `txn.large` | `transaction` | 20.71 ms | 892.80 us | 0.043x | [0.040, 0.046] | 30 |
| `schema.index` | `schema` | 4.79 s | 50.46 ms | 0.010x | [0.009, 0.011] | 30 |
| `extension.json` | `extension` | 28.58 ms | 1.17 ms | 0.041x | [0.038, 0.044] | 30 |
| `extension.fts.build` | `extension` | 127.42 ms | 2.81 ms | 0.021x | [0.020, 0.022] | 30 |
| `extension.fts.query` | `extension` | 262.28 ms | 13.94 ms | 0.049x | [0.050, 0.059] | 30 |
| `extension.rtree.insert` | `extension` | 10.19 ms | 2.55 ms | 0.259x | [0.246, 0.270] | 30 |
| `extension.rtree.query` | `extension` | 11.60 ms | 8.25 ms | 0.899x | [0.792, 0.984] | 30 |
| `large.read` | `large.values` | 35.42 ms | 32.08 ms | 0.816x | [0.805, 1.032] | 30 |
| `large.write` | `large.values` | 3.25 ms | 2.49 ms | 0.762x | [0.568, 1.063] | 30 |

## Scale `large` - 600000 rows

Weighted geometric mean **0.143x**, 95% interval [0.139, 0.151]. The release bound is a lower bound of at least 1.50x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.180x | [0.122, 0.267] | loss | **below 0.90x** |
| `read.point` | 0.16 | 0.797x | [0.722, 0.882] | loss | **below 0.90x** |
| `read.range` | 0.12 | 0.015x | [0.008, 0.028] | loss | **below 0.90x** |
| `read.analytical` | 0.10 | 0.031x | [0.021, 0.046] | loss | **below 0.90x** |
| `read.join` | 0.08 | 0.178x | [0.119, 0.263] | loss | **below 0.90x** |
| `write` | 0.20 | 0.281x | [0.236, 0.334] | loss | **below 0.90x** |
| `transaction` | 0.10 | 0.281x | [0.194, 0.406] | loss | **below 0.90x** |
| `schema` | 0.04 | 0.002x | [0.002, 0.002] | loss | **below 0.90x** |
| `extension` | 0.08 | 0.161x | [0.131, 0.198] | loss | **below 0.90x** |
| `large.values` | 0.04 | 1.103x | [0.978, 1.232] | inconclusive | met |

### By workload

| workload | family | rust-db median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 25.08 ms | 983.15 us | 0.037x | [0.037, 0.044] | 30 |
| `prepare.point` | `open.prepare` | 69.25 ms | 53.50 ms | 0.835x | [0.732, 0.898] | 30 |
| `point.rowid` | `read.point` | 37.12 ms | 47.38 ms | 1.194x | [1.117, 1.378] | 30 |
| `point.index` | `read.point` | 46.57 ms | 37.59 ms | 0.777x | [0.751, 0.955] | 30 |
| `point.miss` | `read.point` | 69.06 ms | 33.87 ms | 0.479x | [0.439, 0.538] | 30 |
| `range.covering` | `read.range` | 58.34 ms | 12.96 ms | 0.228x | [0.208, 0.253] | 30 |
| `range.lookaside` | `read.range` | 374.70 ms | 25.90 ms | 0.067x | [0.063, 0.074] | 30 |
| `range.reverse` | `read.range` | 70.62 s | 15.17 ms | 0.000x | [0.000, 0.000] | 30 |
| `scan.aggregate` | `read.analytical` | 913.16 ms | 99.78 ms | 0.105x | [0.094, 0.114] | 30 |
| `scan.group` | `read.analytical` | 830.95 ms | 84.06 ms | 0.095x | [0.091, 0.109] | 30 |
| `scan.sort` | `read.analytical` | 1.82 s | 235.72 ms | 0.126x | [0.116, 0.138] | 30 |
| `scan.distinct` | `read.analytical` | 701.78 ms | 582.95 us | 0.001x | [0.001, 0.001] | 30 |
| `join.selective` | `read.join` | 30.19 ms | 24.22 ms | 0.703x | [0.653, 0.872] | 30 |
| `join.range` | `read.join` | 438.88 ms | 19.31 ms | 0.040x | [0.038, 0.046] | 30 |
| `write.insert.batch` | `write` | 53.20 ms | 38.83 ms | 0.389x | [0.214, 0.544] | 30 |
| `write.insert.autocommit` | `write` | 74.98 ms | 73.58 ms | 0.989x | [0.927, 1.007] | 30 |
| `write.update.indexed` | `write` | 1.53 s | 146.80 ms | 0.090x | [0.080, 0.093] | 30 |
| `write.delete` | `write` | 1.16 s | 142.85 ms | 0.118x | [0.104, 0.125] | 30 |
| `write.upsert` | `write` | 6.80 ms | 3.74 ms | 0.532x | [0.495, 0.584] | 30 |
| `txn.autocommit` | `transaction` | 19.75 ms | 22.61 ms | 1.140x | [1.073, 1.193] | 30 |
| `txn.batched` | `transaction` | 150.35 ms | 123.20 ms | 0.832x | [0.798, 0.897] | 30 |
| `txn.large` | `transaction` | 22.65 ms | 522.65 us | 0.023x | [0.022, 0.025] | 30 |
| `schema.index` | `schema` | 129.94 s | 317.10 ms | 0.002x | [0.002, 0.002] | 30 |
| `extension.json` | `extension` | 19.46 ms | 645.10 us | 0.036x | [0.035, 0.043] | 30 |
| `extension.fts.build` | `extension` | 45.49 ms | 2.44 ms | 0.055x | [0.051, 0.062] | 30 |
| `extension.fts.query` | `extension` | 64.36 ms | 8.01 ms | 0.116x | [0.098, 0.128] | 30 |
| `extension.rtree.insert` | `extension` | 5.52 ms | 2.17 ms | 0.385x | [0.377, 0.423] | 30 |
| `extension.rtree.query` | `extension` | 5.38 ms | 6.26 ms | 1.111x | [1.036, 1.264] | 30 |
| `large.read` | `large.values` | 16.16 ms | 21.47 ms | 1.418x | [1.241, 1.556] | 30 |
| `large.write` | `large.values` | 2.35 ms | 1.99 ms | 0.927x | [0.732, 1.010] | 30 |


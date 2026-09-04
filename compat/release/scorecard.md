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

Weighted geometric mean **0.256x**, 95% interval [0.247, 0.263]. The release bound is a lower bound of at least 1.50x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.199x | [0.133, 0.298] | loss | **below 0.90x** |
| `read.point` | 0.16 | 0.894x | [0.784, 1.019] | inconclusive | **below 0.90x** |
| `read.range` | 0.12 | 0.236x | [0.194, 0.289] | loss | **below 0.90x** |
| `read.analytical` | 0.10 | 0.068x | [0.060, 0.076] | loss | **below 0.90x** |
| `read.join` | 0.08 | 0.304x | [0.216, 0.425] | loss | **below 0.90x** |
| `write` | 0.20 | 0.212x | [0.180, 0.249] | loss | **below 0.90x** |
| `transaction` | 0.10 | 0.300x | [0.217, 0.413] | loss | **below 0.90x** |
| `schema` | 0.04 | 0.204x | [0.193, 0.216] | loss | **below 0.90x** |
| `extension` | 0.08 | 0.102x | [0.082, 0.127] | loss | **below 0.90x** |
| `large.values` | 0.04 | 0.853x | [0.758, 0.996] | loss | **below 0.90x** |

### By workload

| workload | family | rust-db median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 44.46 ms | 1.91 ms | 0.042x | [0.041, 0.047] | 30 |
| `prepare.point` | `open.prepare` | 81.49 ms | 78.43 ms | 0.911x | [0.837, 0.990] | 30 |
| `point.rowid` | `read.point` | 38.98 ms | 69.92 ms | 1.723x | [1.539, 1.893] | 30 |
| `point.index` | `read.point` | 70.57 ms | 68.42 ms | 0.953x | [0.889, 1.056] | 30 |
| `point.miss` | `read.point` | 148.52 ms | 67.00 ms | 0.426x | [0.398, 0.472] | 30 |
| `range.covering` | `read.range` | 110.19 ms | 20.47 ms | 0.197x | [0.180, 0.219] | 30 |
| `range.lookaside` | `read.range` | 292.72 ms | 24.68 ms | 0.081x | [0.078, 0.090] | 30 |
| `range.reverse` | `read.range` | 24.08 ms | 16.01 ms | 0.765x | [0.712, 0.902] | 30 |
| `scan.aggregate` | `read.analytical` | 669.85 ms | 59.08 ms | 0.088x | [0.084, 0.092] | 30 |
| `scan.group` | `read.analytical` | 459.07 ms | 53.18 ms | 0.113x | [0.110, 0.119] | 30 |
| `scan.sort` | `read.analytical` | 1.11 s | 105.68 ms | 0.098x | [0.091, 0.099] | 30 |
| `scan.distinct` | `read.analytical` | 475.22 ms | 10.07 ms | 0.021x | [0.020, 0.024] | 30 |
| `join.selective` | `read.join` | 31.87 ms | 39.04 ms | 1.172x | [0.942, 1.211] | 30 |
| `join.range` | `read.join` | 254.16 ms | 21.83 ms | 0.085x | [0.080, 0.093] | 30 |
| `write.insert.batch` | `write` | 97.55 ms | 6.98 ms | 0.072x | [0.066, 0.082] | 30 |
| `write.insert.autocommit` | `write` | 142.21 ms | 146.97 ms | 1.014x | [0.972, 1.108] | 30 |
| `write.update.indexed` | `write` | 82.30 ms | 7.83 ms | 0.095x | [0.088, 0.098] | 30 |
| `write.delete` | `write` | 50.29 ms | 6.92 ms | 0.140x | [0.132, 0.150] | 30 |
| `write.upsert` | `write` | 10.70 ms | 4.53 ms | 0.432x | [0.404, 0.456] | 30 |
| `txn.autocommit` | `transaction` | 46.82 ms | 43.89 ms | 0.993x | [0.804, 1.036] | 30 |
| `txn.batched` | `transaction` | 360.65 ms | 300.24 ms | 0.823x | [0.788, 0.880] | 30 |
| `txn.large` | `transaction` | 20.75 ms | 731.65 us | 0.036x | [0.034, 0.037] | 30 |
| `schema.index` | `schema` | 16.11 ms | 3.30 ms | 0.210x | [0.193, 0.216] | 30 |
| `extension.json` | `extension` | 34.95 ms | 1.20 ms | 0.037x | [0.034, 0.040] | 30 |
| `extension.fts.build` | `extension` | 139.22 ms | 2.87 ms | 0.021x | [0.020, 0.022] | 30 |
| `extension.fts.query` | `extension` | 276.64 ms | 17.94 ms | 0.057x | [0.053, 0.062] | 30 |
| `extension.rtree.insert` | `extension` | 9.90 ms | 2.51 ms | 0.246x | [0.242, 0.340] | 30 |
| `extension.rtree.query` | `extension` | 11.80 ms | 12.41 ms | 0.979x | [0.812, 1.048] | 30 |
| `large.read` | `large.values` | 37.99 ms | 33.99 ms | 0.936x | [0.816, 1.014] | 30 |
| `large.write` | `large.values` | 3.13 ms | 2.30 ms | 0.703x | [0.660, 1.053] | 30 |

## Scale `medium` - 100000 rows

Weighted geometric mean **0.198x**, 95% interval [0.194, 0.210]. The release bound is a lower bound of at least 1.50x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.187x | [0.129, 0.273] | loss | **below 0.90x** |
| `read.point` | 0.16 | 0.661x | [0.578, 0.752] | loss | **below 0.90x** |
| `read.range` | 0.12 | 0.230x | [0.188, 0.285] | loss | **below 0.90x** |
| `read.analytical` | 0.10 | 0.036x | [0.026, 0.049] | loss | **below 0.90x** |
| `read.join` | 0.08 | 0.180x | [0.126, 0.255] | loss | **below 0.90x** |
| `write` | 0.20 | 0.172x | [0.142, 0.210] | loss | **below 0.90x** |
| `transaction` | 0.10 | 0.318x | [0.235, 0.425] | loss | **below 0.90x** |
| `schema` | 0.04 | 0.073x | [0.068, 0.078] | loss | **below 0.90x** |
| `extension` | 0.08 | 0.107x | [0.086, 0.133] | loss | **below 0.90x** |
| `large.values` | 0.04 | 0.826x | [0.731, 0.917] | loss | **below 0.90x** |

### By workload

| workload | family | rust-db median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 40.11 ms | 1.88 ms | 0.046x | [0.041, 0.048] | 30 |
| `prepare.point` | `open.prepare` | 127.35 ms | 97.31 ms | 0.776x | [0.684, 0.922] | 30 |
| `point.rowid` | `read.point` | 75.86 ms | 83.71 ms | 1.095x | [0.977, 1.196] | 30 |
| `point.index` | `read.point` | 82.65 ms | 76.96 ms | 0.894x | [0.833, 1.018] | 30 |
| `point.miss` | `read.point` | 228.09 ms | 65.58 ms | 0.273x | [0.264, 0.328] | 30 |
| `range.covering` | `read.range` | 118.54 ms | 19.55 ms | 0.160x | [0.160, 0.192] | 30 |
| `range.lookaside` | `read.range` | 590.90 ms | 46.18 ms | 0.079x | [0.074, 0.092] | 30 |
| `range.reverse` | `read.range` | 31.53 ms | 27.56 ms | 0.799x | [0.767, 0.979] | 30 |
| `scan.aggregate` | `read.analytical` | 1.35 s | 108.12 ms | 0.078x | [0.075, 0.085] | 30 |
| `scan.group` | `read.analytical` | 896.93 ms | 85.15 ms | 0.094x | [0.090, 0.098] | 30 |
| `scan.sort` | `read.analytical` | 2.73 s | 396.78 ms | 0.143x | [0.139, 0.155] | 30 |
| `scan.distinct` | `read.analytical` | 957.23 ms | 1.28 ms | 0.001x | [0.001, 0.002] | 30 |
| `join.selective` | `read.join` | 64.95 ms | 40.30 ms | 0.650x | [0.639, 0.771] | 30 |
| `join.range` | `read.join` | 676.76 ms | 29.46 ms | 0.044x | [0.043, 0.050] | 30 |
| `write.insert.batch` | `write` | 110.39 ms | 16.83 ms | 0.149x | [0.094, 0.135] | 30 |
| `write.insert.autocommit` | `write` | 139.01 ms | 139.11 ms | 0.982x | [0.959, 1.123] | 30 |
| `write.update.indexed` | `write` | 2.04 s | 111.87 ms | 0.055x | [0.053, 0.060] | 30 |
| `write.delete` | `write` | 2.18 s | 111.56 ms | 0.050x | [0.046, 0.052] | 30 |
| `write.upsert` | `write` | 10.00 ms | 4.99 ms | 0.475x | [0.444, 0.511] | 30 |
| `txn.autocommit` | `transaction` | 49.65 ms | 45.25 ms | 0.879x | [0.859, 0.961] | 30 |
| `txn.batched` | `transaction` | 339.67 ms | 282.22 ms | 0.850x | [0.803, 0.910] | 30 |
| `txn.large` | `transaction` | 22.47 ms | 905.10 us | 0.041x | [0.039, 0.044] | 30 |
| `schema.index` | `schema` | 722.30 ms | 51.66 ms | 0.074x | [0.068, 0.078] | 30 |
| `extension.json` | `extension` | 32.77 ms | 1.20 ms | 0.040x | [0.036, 0.043] | 30 |
| `extension.fts.build` | `extension` | 138.16 ms | 2.93 ms | 0.021x | [0.021, 0.024] | 30 |
| `extension.fts.query` | `extension` | 269.06 ms | 18.96 ms | 0.068x | [0.058, 0.069] | 30 |
| `extension.rtree.insert` | `extension` | 10.73 ms | 2.65 ms | 0.254x | [0.245, 0.275] | 30 |
| `extension.rtree.query` | `extension` | 12.41 ms | 12.36 ms | 0.985x | [0.860, 1.079] | 30 |
| `large.read` | `large.values` | 36.62 ms | 37.23 ms | 0.999x | [0.880, 1.079] | 30 |
| `large.write` | `large.values` | 3.27 ms | 2.49 ms | 0.712x | [0.571, 0.822] | 30 |

## Scale `large` - 600000 rows

Weighted geometric mean **0.219x**, 95% interval [0.203, 0.235]. The release bound is a lower bound of at least 1.50x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.196x | [0.132, 0.292] | loss | **below 0.90x** |
| `read.point` | 0.16 | 0.812x | [0.729, 0.906] | loss | **below 0.90x** |
| `read.range` | 0.12 | 0.243x | [0.194, 0.305] | loss | **below 0.90x** |
| `read.analytical` | 0.10 | 0.040x | [0.027, 0.059] | loss | **below 0.90x** |
| `read.join` | 0.08 | 0.182x | [0.124, 0.265] | loss | **below 0.90x** |
| `write` | 0.20 | 0.247x | [0.210, 0.289] | loss | **below 0.90x** |
| `transaction` | 0.10 | 0.235x | [0.163, 0.334] | loss | **below 0.90x** |
| `schema` | 0.04 | 0.018x | [0.017, 0.019] | loss | **below 0.90x** |
| `extension` | 0.08 | 0.162x | [0.131, 0.200] | loss | **below 0.90x** |
| `large.values` | 0.04 | 1.026x | [0.937, 1.120] | inconclusive | met |

### By workload

| workload | family | rust-db median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 23.94 ms | 993.20 us | 0.043x | [0.039, 0.046] | 30 |
| `prepare.point` | `open.prepare` | 72.10 ms | 62.98 ms | 0.854x | [0.781, 1.057] | 30 |
| `point.rowid` | `read.point` | 43.02 ms | 53.05 ms | 1.263x | [1.086, 1.335] | 30 |
| `point.index` | `read.point` | 52.09 ms | 47.96 ms | 0.917x | [0.801, 1.108] | 30 |
| `point.miss` | `read.point` | 75.70 ms | 38.65 ms | 0.498x | [0.415, 0.537] | 30 |
| `range.covering` | `read.range` | 63.04 ms | 15.29 ms | 0.223x | [0.194, 0.254] | 30 |
| `range.lookaside` | `read.range` | 371.58 ms | 28.26 ms | 0.074x | [0.061, 0.089] | 30 |
| `range.reverse` | `read.range` | 19.56 ms | 17.85 ms | 0.844x | [0.725, 1.072] | 30 |
| `scan.aggregate` | `read.analytical` | 850.67 ms | 103.23 ms | 0.119x | [0.107, 0.137] | 30 |
| `scan.group` | `read.analytical` | 546.67 ms | 92.18 ms | 0.169x | [0.150, 0.173] | 30 |
| `scan.sort` | `read.analytical` | 1.71 s | 254.50 ms | 0.149x | [0.141, 0.177] | 30 |
| `scan.distinct` | `read.analytical` | 610.67 ms | 566.05 us | 0.001x | [0.001, 0.001] | 30 |
| `join.selective` | `read.join` | 34.69 ms | 25.43 ms | 0.692x | [0.596, 0.842] | 30 |
| `join.range` | `read.join` | 486.29 ms | 23.18 ms | 0.048x | [0.041, 0.053] | 30 |
| `write.insert.batch` | `write` | 141.65 ms | 30.22 ms | 0.221x | [0.119, 0.236] | 30 |
| `write.insert.autocommit` | `write` | 74.38 ms | 74.99 ms | 0.969x | [0.908, 1.044] | 30 |
| `write.update.indexed` | `write` | 1.73 s | 159.75 ms | 0.088x | [0.080, 0.099] | 30 |
| `write.delete` | `write` | 1.25 s | 150.70 ms | 0.116x | [0.109, 0.128] | 30 |
| `write.upsert` | `write` | 7.17 ms | 3.64 ms | 0.508x | [0.480, 0.567] | 30 |
| `txn.autocommit` | `transaction` | 27.95 ms | 23.58 ms | 0.869x | [0.766, 0.915] | 30 |
| `txn.batched` | `transaction` | 180.14 ms | 130.42 ms | 0.705x | [0.651, 0.795] | 30 |
| `txn.large` | `transaction` | 23.91 ms | 560.35 us | 0.023x | [0.018, 0.024] | 30 |
| `schema.index` | `schema` | 18.77 s | 336.26 ms | 0.018x | [0.017, 0.019] | 30 |
| `extension.json` | `extension` | 19.53 ms | 660.70 us | 0.037x | [0.034, 0.040] | 30 |
| `extension.fts.build` | `extension` | 47.67 ms | 2.46 ms | 0.051x | [0.048, 0.057] | 30 |
| `extension.fts.query` | `extension` | 68.61 ms | 8.19 ms | 0.123x | [0.100, 0.130] | 30 |
| `extension.rtree.insert` | `extension` | 5.42 ms | 2.23 ms | 0.445x | [0.409, 0.460] | 30 |
| `extension.rtree.query` | `extension` | 5.75 ms | 6.30 ms | 1.112x | [1.013, 1.320] | 30 |
| `large.read` | `large.values` | 17.33 ms | 24.59 ms | 1.417x | [1.088, 1.416] | 30 |
| `large.write` | `large.values` | 2.51 ms | 2.17 ms | 0.843x | [0.781, 0.909] | 30 |


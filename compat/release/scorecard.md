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

Weighted geometric mean **0.240x**, 95% interval [0.238, 0.250]. The release bound is a lower bound of at least 1.50x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.223x | [0.153, 0.330] | loss | **below 0.90x** |
| `read.point` | 0.16 | 0.854x | [0.747, 0.980] | loss | **below 0.90x** |
| `read.range` | 0.12 | 0.212x | [0.173, 0.263] | loss | **below 0.90x** |
| `read.analytical` | 0.10 | 0.055x | [0.049, 0.062] | loss | **below 0.90x** |
| `read.join` | 0.08 | 0.280x | [0.200, 0.389] | loss | **below 0.90x** |
| `write` | 0.20 | 0.206x | [0.175, 0.243] | loss | **below 0.90x** |
| `transaction` | 0.10 | 0.306x | [0.219, 0.420] | loss | **below 0.90x** |
| `schema` | 0.04 | 0.211x | [0.202, 0.222] | loss | **below 0.90x** |
| `extension` | 0.08 | 0.098x | [0.079, 0.123] | loss | **below 0.90x** |
| `large.values` | 0.04 | 0.805x | [0.693, 0.951] | loss | **below 0.90x** |

### By workload

| workload | family | rust-db median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 36.08 ms | 1.92 ms | 0.052x | [0.049, 0.054] | 30 |
| `prepare.point` | `open.prepare` | 76.05 ms | 75.42 ms | 0.993x | [0.873, 1.081] | 30 |
| `point.rowid` | `read.point` | 35.34 ms | 60.49 ms | 1.591x | [1.562, 1.831] | 30 |
| `point.index` | `read.point` | 68.80 ms | 62.76 ms | 0.893x | [0.860, 1.039] | 30 |
| `point.miss` | `read.point` | 145.68 ms | 54.29 ms | 0.373x | [0.355, 0.436] | 30 |
| `range.covering` | `read.range` | 115.88 ms | 16.24 ms | 0.143x | [0.143, 0.168] | 30 |
| `range.lookaside` | `read.range` | 300.87 ms | 22.11 ms | 0.075x | [0.072, 0.081] | 30 |
| `range.reverse` | `read.range` | 20.88 ms | 15.53 ms | 0.759x | [0.725, 0.898] | 30 |
| `scan.aggregate` | `read.analytical` | 718.90 ms | 53.45 ms | 0.073x | [0.071, 0.078] | 30 |
| `scan.group` | `read.analytical` | 610.06 ms | 50.20 ms | 0.080x | [0.079, 0.084] | 30 |
| `scan.sort` | `read.analytical` | 1.12 s | 97.33 ms | 0.086x | [0.083, 0.089] | 30 |
| `scan.distinct` | `read.analytical` | 566.60 ms | 8.92 ms | 0.016x | [0.016, 0.019] | 30 |
| `join.selective` | `read.join` | 30.29 ms | 30.04 ms | 0.949x | [0.903, 1.116] | 30 |
| `join.range` | `read.join` | 253.16 ms | 19.44 ms | 0.076x | [0.074, 0.084] | 30 |
| `write.insert.batch` | `write` | 92.66 ms | 5.82 ms | 0.061x | [0.060, 0.082] | 30 |
| `write.insert.autocommit` | `write` | 141.42 ms | 137.48 ms | 0.982x | [0.907, 1.038] | 30 |
| `write.update.indexed` | `write` | 77.99 ms | 7.80 ms | 0.101x | [0.091, 0.101] | 30 |
| `write.delete` | `write` | 49.72 ms | 6.52 ms | 0.134x | [0.126, 0.142] | 30 |
| `write.upsert` | `write` | 9.87 ms | 4.20 ms | 0.433x | [0.408, 0.463] | 30 |
| `txn.autocommit` | `transaction` | 43.51 ms | 40.43 ms | 0.934x | [0.887, 1.150] | 30 |
| `txn.batched` | `transaction` | 339.18 ms | 278.73 ms | 0.798x | [0.787, 0.876] | 30 |
| `txn.large` | `transaction` | 19.91 ms | 713.15 us | 0.035x | [0.034, 0.037] | 30 |
| `schema.index` | `schema` | 15.28 ms | 3.28 ms | 0.214x | [0.202, 0.222] | 30 |
| `extension.json` | `extension` | 30.94 ms | 1.18 ms | 0.038x | [0.035, 0.040] | 30 |
| `extension.fts.build` | `extension` | 135.62 ms | 2.74 ms | 0.020x | [0.018, 0.020] | 30 |
| `extension.fts.query` | `extension` | 267.21 ms | 14.73 ms | 0.051x | [0.050, 0.058] | 30 |
| `extension.rtree.insert` | `extension` | 9.80 ms | 2.54 ms | 0.249x | [0.239, 0.276] | 30 |
| `extension.rtree.query` | `extension` | 8.98 ms | 8.62 ms | 0.892x | [0.829, 1.028] | 30 |
| `large.read` | `large.values` | 35.27 ms | 31.85 ms | 0.830x | [0.809, 0.948] | 30 |
| `large.write` | `large.values` | 3.37 ms | 2.38 ms | 0.699x | [0.567, 1.039] | 30 |

## Scale `medium` - 100000 rows

Weighted geometric mean **0.192x**, 95% interval [0.181, 0.199]. The release bound is a lower bound of at least 1.50x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.186x | [0.130, 0.269] | loss | **below 0.90x** |
| `read.point` | 0.16 | 0.646x | [0.560, 0.743] | loss | **below 0.90x** |
| `read.range` | 0.12 | 0.217x | [0.177, 0.267] | loss | **below 0.90x** |
| `read.analytical` | 0.10 | 0.029x | [0.020, 0.039] | loss | **below 0.90x** |
| `read.join` | 0.08 | 0.163x | [0.113, 0.232] | loss | **below 0.90x** |
| `write` | 0.20 | 0.171x | [0.141, 0.208] | loss | **below 0.90x** |
| `transaction` | 0.10 | 0.313x | [0.230, 0.423] | loss | **below 0.90x** |
| `schema` | 0.04 | 0.073x | [0.067, 0.080] | loss | **below 0.90x** |
| `extension` | 0.08 | 0.099x | [0.080, 0.123] | loss | **below 0.90x** |
| `large.values` | 0.04 | 0.760x | [0.708, 0.814] | loss | **below 0.90x** |

### By workload

| workload | family | rust-db median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 39.59 ms | 1.91 ms | 0.047x | [0.043, 0.050] | 30 |
| `prepare.point` | `open.prepare` | 110.10 ms | 82.74 ms | 0.704x | [0.661, 0.827] | 30 |
| `point.rowid` | `read.point` | 64.16 ms | 71.00 ms | 1.125x | [1.039, 1.294] | 30 |
| `point.index` | `read.point` | 76.57 ms | 59.07 ms | 0.805x | [0.739, 0.924] | 30 |
| `point.miss` | `read.point` | 194.86 ms | 55.56 ms | 0.288x | [0.256, 0.311] | 30 |
| `range.covering` | `read.range` | 117.59 ms | 16.92 ms | 0.144x | [0.145, 0.173] | 30 |
| `range.lookaside` | `read.range` | 541.46 ms | 43.56 ms | 0.073x | [0.072, 0.085] | 30 |
| `range.reverse` | `read.range` | 28.68 ms | 20.80 ms | 0.751x | [0.736, 0.921] | 30 |
| `scan.aggregate` | `read.analytical` | 1.41 s | 100.14 ms | 0.070x | [0.068, 0.074] | 30 |
| `scan.group` | `read.analytical` | 1.27 s | 80.89 ms | 0.064x | [0.062, 0.068] | 30 |
| `scan.sort` | `read.analytical` | 2.65 s | 325.15 ms | 0.121x | [0.117, 0.131] | 30 |
| `scan.distinct` | `read.analytical` | 1.10 s | 1.25 ms | 0.001x | [0.001, 0.001] | 30 |
| `join.selective` | `read.join` | 51.96 ms | 31.60 ms | 0.618x | [0.579, 0.697] | 30 |
| `join.range` | `read.join` | 641.05 ms | 25.76 ms | 0.040x | [0.039, 0.045] | 30 |
| `write.insert.batch` | `write` | 99.72 ms | 16.05 ms | 0.150x | [0.093, 0.147] | 30 |
| `write.insert.autocommit` | `write` | 130.12 ms | 130.96 ms | 0.998x | [0.955, 1.128] | 30 |
| `write.update.indexed` | `write` | 1.79 s | 92.67 ms | 0.051x | [0.048, 0.057] | 30 |
| `write.delete` | `write` | 1.97 s | 91.83 ms | 0.047x | [0.043, 0.053] | 30 |
| `write.upsert` | `write` | 10.57 ms | 4.94 ms | 0.478x | [0.439, 0.547] | 30 |
| `txn.autocommit` | `transaction` | 47.43 ms | 45.30 ms | 0.925x | [0.807, 0.971] | 30 |
| `txn.batched` | `transaction` | 323.84 ms | 268.93 ms | 0.821x | [0.784, 0.908] | 30 |
| `txn.large` | `transaction` | 21.59 ms | 891.00 us | 0.041x | [0.038, 0.043] | 30 |
| `schema.index` | `schema` | 672.94 ms | 49.63 ms | 0.072x | [0.067, 0.080] | 30 |
| `extension.json` | `extension` | 27.70 ms | 1.19 ms | 0.041x | [0.037, 0.042] | 30 |
| `extension.fts.build` | `extension` | 133.54 ms | 2.79 ms | 0.021x | [0.019, 0.022] | 30 |
| `extension.fts.query` | `extension` | 254.48 ms | 14.41 ms | 0.052x | [0.051, 0.063] | 30 |
| `extension.rtree.insert` | `extension` | 10.53 ms | 2.53 ms | 0.242x | [0.203, 0.256] | 30 |
| `extension.rtree.query` | `extension` | 11.25 ms | 8.11 ms | 0.896x | [0.780, 1.009] | 30 |
| `large.read` | `large.values` | 33.08 ms | 25.64 ms | 0.866x | [0.800, 0.924] | 30 |
| `large.write` | `large.values` | 3.48 ms | 2.42 ms | 0.689x | [0.606, 0.742] | 30 |

## Scale `large` - 600000 rows

Weighted geometric mean **0.209x**, 95% interval [0.197, 0.212]. The release bound is a lower bound of at least 1.50x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.195x | [0.134, 0.287] | loss | **below 0.90x** |
| `read.point` | 0.16 | 0.694x | [0.625, 0.774] | loss | **below 0.90x** |
| `read.range` | 0.12 | 0.196x | [0.156, 0.247] | loss | **below 0.90x** |
| `read.analytical` | 0.10 | 0.031x | [0.021, 0.045] | loss | **below 0.90x** |
| `read.join` | 0.08 | 0.180x | [0.122, 0.260] | loss | **below 0.90x** |
| `write` | 0.20 | 0.263x | [0.224, 0.310] | loss | **below 0.90x** |
| `transaction` | 0.10 | 0.249x | [0.174, 0.352] | loss | **below 0.90x** |
| `schema` | 0.04 | 0.018x | [0.017, 0.019] | loss | **below 0.90x** |
| `extension` | 0.08 | 0.159x | [0.129, 0.194] | loss | **below 0.90x** |
| `large.values` | 0.04 | 1.013x | [0.918, 1.120] | inconclusive | met |

### By workload

| workload | family | rust-db median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 18.41 ms | 951.65 us | 0.050x | [0.043, 0.051] | 30 |
| `prepare.point` | `open.prepare` | 57.47 ms | 45.37 ms | 0.749x | [0.710, 0.936] | 30 |
| `point.rowid` | `read.point` | 30.89 ms | 36.28 ms | 1.094x | [1.041, 1.230] | 30 |
| `point.index` | `read.point` | 39.41 ms | 27.96 ms | 0.708x | [0.689, 0.839] | 30 |
| `point.miss` | `read.point` | 61.02 ms | 23.67 ms | 0.413x | [0.361, 0.416] | 30 |
| `range.covering` | `read.range` | 60.96 ms | 9.27 ms | 0.154x | [0.142, 0.173] | 30 |
| `range.lookaside` | `read.range` | 335.42 ms | 19.80 ms | 0.061x | [0.055, 0.070] | 30 |
| `range.reverse` | `read.range` | 14.31 ms | 10.57 ms | 0.747x | [0.659, 0.911] | 30 |
| `scan.aggregate` | `read.analytical` | 868.87 ms | 87.91 ms | 0.097x | [0.094, 0.110] | 30 |
| `scan.group` | `read.analytical` | 803.75 ms | 77.81 ms | 0.095x | [0.091, 0.098] | 30 |
| `scan.sort` | `read.analytical` | 1.63 s | 202.46 ms | 0.123x | [0.122, 0.133] | 30 |
| `scan.distinct` | `read.analytical` | 674.38 ms | 441.25 us | 0.001x | [0.001, 0.001] | 30 |
| `join.selective` | `read.join` | 27.80 ms | 22.36 ms | 0.756x | [0.695, 0.850] | 30 |
| `join.range` | `read.join` | 428.00 ms | 16.72 ms | 0.039x | [0.040, 0.045] | 30 |
| `write.insert.batch` | `write` | 130.44 ms | 41.53 ms | 0.246x | [0.179, 0.345] | 30 |
| `write.insert.autocommit` | `write` | 67.94 ms | 67.35 ms | 1.016x | [0.959, 1.139] | 30 |
| `write.update.indexed` | `write` | 1.44 s | 126.49 ms | 0.084x | [0.078, 0.090] | 30 |
| `write.delete` | `write` | 1.03 s | 130.11 ms | 0.119x | [0.108, 0.124] | 30 |
| `write.upsert` | `write` | 7.10 ms | 3.48 ms | 0.531x | [0.432, 0.544] | 30 |
| `txn.autocommit` | `transaction` | 23.57 ms | 21.09 ms | 0.893x | [0.852, 0.950] | 30 |
| `txn.batched` | `transaction` | 149.86 ms | 113.03 ms | 0.755x | [0.726, 0.793] | 30 |
| `txn.large` | `transaction` | 20.74 ms | 500.65 us | 0.023x | [0.022, 0.024] | 30 |
| `schema.index` | `schema` | 15.31 s | 276.57 ms | 0.018x | [0.017, 0.019] | 30 |
| `extension.json` | `extension` | 13.08 ms | 603.00 us | 0.045x | [0.041, 0.046] | 30 |
| `extension.fts.build` | `extension` | 45.35 ms | 2.28 ms | 0.050x | [0.048, 0.053] | 30 |
| `extension.fts.query` | `extension` | 56.51 ms | 5.03 ms | 0.088x | [0.093, 0.113] | 30 |
| `extension.rtree.insert` | `extension` | 5.47 ms | 2.00 ms | 0.380x | [0.351, 0.425] | 30 |
| `extension.rtree.query` | `extension` | 3.67 ms | 6.14 ms | 1.151x | [1.052, 1.326] | 30 |
| `large.read` | `large.values` | 11.26 ms | 13.21 ms | 1.243x | [1.106, 1.448] | 30 |
| `large.write` | `large.values` | 2.21 ms | 1.79 ms | 0.770x | [0.745, 0.879] | 30 |


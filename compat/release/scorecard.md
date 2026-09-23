# inillucent performance scorecard

> **These are the `release` gate's own output files, taken on the engine that came
> before the rearchitecture, and they are kept as that run's record. They are not the current
> numbers and they are not edited by hand.** The engine measured here was slower than SQLite on
> every family; the shipping engine is **397% faster**, measured 2026-09-23, weighted over the same ten families, with
> no family below the contract's 1.00x floor. The current run is
> [docs/performance.md](../../docs/performance.md) and
> [docs/feature-comparison.md](../../docs/feature-comparison.md).

Label `release`, platform `windows-x86_64`, 30 paired rounds per scale, bootstrap seed 17900001. Every optimization is on, which is the shipped engine.

Both engines read the same plan file. The ratio is SQLite over inillucent, so **above one means inillucent is faster**. A workload whose two engines returned different answers is reported as a correctness failure and is not timed.

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

Weighted geometric mean **0.316x**, 95% interval [0.313, 0.321]. The release bound is a lower bound of at least 1.50x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.214x | [0.151, 0.310] | loss | **below 0.90x** |
| `read.point` | 0.16 | 0.871x | [0.764, 0.994] | loss | **below 0.90x** |
| `read.range` | 0.12 | 0.303x | [0.249, 0.372] | loss | **below 0.90x** |
| `read.analytical` | 0.10 | 0.120x | [0.108, 0.133] | loss | **below 0.90x** |
| `read.join` | 0.08 | 0.326x | [0.234, 0.452] | loss | **below 0.90x** |
| `write` | 0.20 | 0.270x | [0.236, 0.308] | loss | **below 0.90x** |
| `transaction` | 0.10 | 0.477x | [0.386, 0.582] | loss | **below 0.90x** |
| `schema` | 0.04 | 0.260x | [0.249, 0.271] | loss | **below 0.90x** |
| `extension` | 0.08 | 0.130x | [0.108, 0.158] | loss | **below 0.90x** |
| `large.values` | 0.04 | 0.855x | [0.780, 0.961] | loss | **below 0.90x** |

### By workload

| workload | family | inillucent median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 30.81 ms | 1.64 ms | 0.053x | [0.050, 0.056] | 30 |
| `prepare.point` | `open.prepare` | 61.83 ms | 52.72 ms | 0.844x | [0.834, 0.888] | 30 |
| `point.rowid` | `read.point` | 28.65 ms | 47.04 ms | 1.636x | [1.561, 1.728] | 30 |
| `point.index` | `read.point` | 45.70 ms | 47.51 ms | 1.040x | [1.007, 1.121] | 30 |
| `point.miss` | `read.point` | 123.28 ms | 45.85 ms | 0.378x | [0.365, 0.400] | 30 |
| `range.covering` | `read.range` | 63.39 ms | 15.23 ms | 0.240x | [0.240, 0.255] | 30 |
| `range.lookaside` | `read.range` | 191.28 ms | 19.73 ms | 0.103x | [0.100, 0.107] | 30 |
| `range.reverse` | `read.range` | 12.82 ms | 14.07 ms | 1.101x | [1.037, 1.146] | 30 |
| `scan.aggregate` | `read.analytical` | 298.48 ms | 49.89 ms | 0.168x | [0.166, 0.173] | 30 |
| `scan.group` | `read.analytical` | 220.83 ms | 45.23 ms | 0.205x | [0.203, 0.211] | 30 |
| `scan.sort` | `read.analytical` | 643.97 ms | 86.89 ms | 0.135x | [0.132, 0.136] | 30 |
| `scan.distinct` | `read.analytical` | 187.55 ms | 8.42 ms | 0.045x | [0.044, 0.046] | 30 |
| `join.selective` | `read.join` | 21.00 ms | 23.66 ms | 1.126x | [1.102, 1.221] | 30 |
| `join.range` | `read.join` | 190.72 ms | 17.62 ms | 0.092x | [0.090, 0.094] | 30 |
| `write.insert.batch` | `write` | 54.82 ms | 5.73 ms | 0.101x | [0.099, 0.120] | 30 |
| `write.insert.autocommit` | `write` | 129.29 ms | 130.12 ms | 1.011x | [0.992, 1.096] | 30 |
| `write.update.indexed` | `write` | 47.39 ms | 7.19 ms | 0.147x | [0.143, 0.153] | 30 |
| `write.delete` | `write` | 31.41 ms | 6.09 ms | 0.193x | [0.188, 0.202] | 30 |
| `write.upsert` | `write` | 9.32 ms | 3.73 ms | 0.447x | [0.413, 0.472] | 30 |
| `txn.autocommit` | `transaction` | 38.29 ms | 37.11 ms | 0.982x | [0.954, 1.037] | 30 |
| `txn.batched` | `transaction` | 275.17 ms | 251.47 ms | 0.899x | [0.857, 1.002] | 30 |
| `txn.large` | `transaction` | 5.74 ms | 665.20 us | 0.118x | [0.114, 0.123] | 30 |
| `schema.index` | `schema` | 11.62 ms | 3.07 ms | 0.252x | [0.249, 0.271] | 30 |
| `extension.json` | `extension` | 24.19 ms | 1.14 ms | 0.046x | [0.043, 0.047] | 30 |
| `extension.fts.build` | `extension` | 42.95 ms | 2.63 ms | 0.060x | [0.054, 0.062] | 30 |
| `extension.fts.query` | `extension` | 235.95 ms | 12.15 ms | 0.051x | [0.052, 0.059] | 30 |
| `extension.rtree.insert` | `extension` | 8.97 ms | 2.49 ms | 0.270x | [0.260, 0.308] | 30 |
| `extension.rtree.query` | `extension` | 7.64 ms | 6.73 ms | 0.874x | [0.830, 1.041] | 30 |
| `large.read` | `large.values` | 27.36 ms | 23.65 ms | 0.861x | [0.867, 0.960] | 30 |
| `large.write` | `large.values` | 3.03 ms | 2.26 ms | 0.781x | [0.679, 1.008] | 30 |

## Scale `medium` - 100000 rows

Weighted geometric mean **0.232x**, 95% interval [0.229, 0.237]. The release bound is a lower bound of at least 1.50x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.209x | [0.150, 0.295] | loss | **below 0.90x** |
| `read.point` | 0.16 | 0.636x | [0.558, 0.724] | loss | **below 0.90x** |
| `read.range` | 0.12 | 0.271x | [0.223, 0.331] | loss | **below 0.90x** |
| `read.analytical` | 0.10 | 0.063x | [0.046, 0.084] | loss | **below 0.90x** |
| `read.join` | 0.08 | 0.216x | [0.154, 0.302] | loss | **below 0.90x** |
| `write` | 0.20 | 0.192x | [0.156, 0.233] | loss | **below 0.90x** |
| `transaction` | 0.10 | 0.399x | [0.309, 0.511] | loss | **below 0.90x** |
| `schema` | 0.04 | 0.071x | [0.070, 0.073] | loss | **below 0.90x** |
| `extension` | 0.08 | 0.128x | [0.106, 0.154] | loss | **below 0.90x** |
| `large.values` | 0.04 | 0.759x | [0.697, 0.821] | loss | **below 0.90x** |

### By workload

| workload | family | inillucent median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 30.40 ms | 1.90 ms | 0.062x | [0.055, 0.061] | 30 |
| `prepare.point` | `open.prepare` | 91.64 ms | 66.85 ms | 0.725x | [0.720, 0.796] | 30 |
| `point.rowid` | `read.point` | 53.40 ms | 58.54 ms | 1.094x | [1.068, 1.129] | 30 |
| `point.index` | `read.point` | 55.34 ms | 49.52 ms | 0.899x | [0.864, 0.903] | 30 |
| `point.miss` | `read.point` | 178.85 ms | 46.07 ms | 0.260x | [0.254, 0.280] | 30 |
| `range.covering` | `read.range` | 67.93 ms | 15.60 ms | 0.227x | [0.224, 0.239] | 30 |
| `range.lookaside` | `read.range` | 411.51 ms | 36.52 ms | 0.088x | [0.085, 0.092] | 30 |
| `range.reverse` | `read.range` | 20.16 ms | 19.46 ms | 0.971x | [0.952, 0.983] | 30 |
| `scan.aggregate` | `read.analytical` | 600.58 ms | 90.96 ms | 0.150x | [0.150, 0.153] | 30 |
| `scan.group` | `read.analytical` | 418.40 ms | 75.42 ms | 0.181x | [0.178, 0.181] | 30 |
| `scan.sort` | `read.analytical` | 1.62 s | 285.90 ms | 0.177x | [0.175, 0.183] | 30 |
| `scan.distinct` | `read.analytical` | 371.91 ms | 1.18 ms | 0.003x | [0.003, 0.003] | 30 |
| `join.selective` | `read.join` | 39.19 ms | 30.13 ms | 0.771x | [0.766, 0.805] | 30 |
| `join.range` | `read.join` | 410.65 ms | 24.61 ms | 0.060x | [0.059, 0.061] | 30 |
| `write.insert.batch` | `write` | 68.13 ms | 16.30 ms | 0.222x | [0.139, 0.257] | 30 |
| `write.insert.autocommit` | `write` | 124.62 ms | 119.26 ms | 0.990x | [0.932, 1.009] | 30 |
| `write.update.indexed` | `write` | 1.55 s | 84.04 ms | 0.054x | [0.053, 0.061] | 30 |
| `write.delete` | `write` | 1.73 s | 80.14 ms | 0.046x | [0.045, 0.048] | 30 |
| `write.upsert` | `write` | 8.56 ms | 4.83 ms | 0.559x | [0.494, 0.602] | 30 |
| `txn.autocommit` | `transaction` | 41.56 ms | 38.94 ms | 0.917x | [0.904, 0.966] | 30 |
| `txn.batched` | `transaction` | 261.68 ms | 232.51 ms | 0.884x | [0.860, 0.956] | 30 |
| `txn.large` | `transaction` | 10.60 ms | 781.75 us | 0.075x | [0.071, 0.079] | 30 |
| `schema.index` | `schema` | 564.47 ms | 40.41 ms | 0.071x | [0.070, 0.073] | 30 |
| `extension.json` | `extension` | 23.88 ms | 1.12 ms | 0.047x | [0.045, 0.048] | 30 |
| `extension.fts.build` | `extension` | 42.48 ms | 2.59 ms | 0.061x | [0.058, 0.063] | 30 |
| `extension.fts.query` | `extension` | 234.34 ms | 11.70 ms | 0.050x | [0.050, 0.053] | 30 |
| `extension.rtree.insert` | `extension` | 8.57 ms | 2.48 ms | 0.283x | [0.223, 0.302] | 30 |
| `extension.rtree.query` | `extension` | 7.57 ms | 6.60 ms | 0.871x | [0.813, 0.985] | 30 |
| `large.read` | `large.values` | 27.32 ms | 23.80 ms | 0.864x | [0.836, 0.988] | 30 |
| `large.write` | `large.values` | 3.30 ms | 2.18 ms | 0.651x | [0.567, 0.710] | 30 |

## Scale `large` - 600000 rows

Weighted geometric mean **0.260x**, 95% interval [0.257, 0.263]. The release bound is a lower bound of at least 1.50x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.203x | [0.145, 0.285] | loss | **below 0.90x** |
| `read.point` | 0.16 | 0.761x | [0.697, 0.827] | loss | **below 0.90x** |
| `read.range` | 0.12 | 0.267x | [0.217, 0.330] | loss | **below 0.90x** |
| `read.analytical` | 0.10 | 0.064x | [0.044, 0.091] | loss | **below 0.90x** |
| `read.join` | 0.08 | 0.201x | [0.142, 0.282] | loss | **below 0.90x** |
| `write` | 0.20 | 0.317x | [0.274, 0.368] | loss | **below 0.90x** |
| `transaction` | 0.10 | 0.354x | [0.265, 0.471] | loss | **below 0.90x** |
| `schema` | 0.04 | 0.019x | [0.019, 0.020] | loss | **below 0.90x** |
| `extension` | 0.08 | 0.203x | [0.168, 0.245] | loss | **below 0.90x** |
| `large.values` | 0.04 | 1.120x | [1.028, 1.210] | inconclusive | met |

### By workload

| workload | family | inillucent median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 15.48 ms | 949.20 us | 0.056x | [0.053, 0.059] | 30 |
| `prepare.point` | `open.prepare` | 47.57 ms | 35.30 ms | 0.736x | [0.700, 0.786] | 30 |
| `point.rowid` | `read.point` | 27.78 ms | 30.18 ms | 1.077x | [1.034, 1.142] | 30 |
| `point.index` | `read.point` | 29.34 ms | 26.65 ms | 0.917x | [0.893, 0.967] | 30 |
| `point.miss` | `read.point` | 53.62 ms | 22.96 ms | 0.431x | [0.427, 0.457] | 30 |
| `range.covering` | `read.range` | 34.20 ms | 8.91 ms | 0.261x | [0.254, 0.266] | 30 |
| `range.lookaside` | `read.range` | 246.50 ms | 18.78 ms | 0.076x | [0.074, 0.078] | 30 |
| `range.reverse` | `read.range` | 10.20 ms | 10.05 ms | 0.983x | [0.937, 0.986] | 30 |
| `scan.aggregate` | `read.analytical` | 371.30 ms | 77.87 ms | 0.210x | [0.209, 0.216] | 30 |
| `scan.group` | `read.analytical` | 256.47 ms | 69.30 ms | 0.270x | [0.266, 0.275] | 30 |
| `scan.sort` | `read.analytical` | 1.07 s | 180.42 ms | 0.169x | [0.168, 0.173] | 30 |
| `scan.distinct` | `read.analytical` | 230.62 ms | 394.95 us | 0.002x | [0.002, 0.002] | 30 |
| `join.selective` | `read.join` | 20.85 ms | 16.32 ms | 0.777x | [0.713, 0.778] | 30 |
| `join.range` | `read.join` | 288.09 ms | 15.55 ms | 0.054x | [0.053, 0.055] | 30 |
| `write.insert.batch` | `write` | 104.55 ms | 51.32 ms | 0.452x | [0.394, 0.518] | 30 |
| `write.insert.autocommit` | `write` | 67.06 ms | 68.04 ms | 1.017x | [0.984, 1.043] | 30 |
| `write.update.indexed` | `write` | 1.24 s | 117.26 ms | 0.095x | [0.090, 0.098] | 30 |
| `write.delete` | `write` | 911.28 ms | 117.56 ms | 0.127x | [0.123, 0.136] | 30 |
| `write.upsert` | `write` | 6.18 ms | 3.68 ms | 0.585x | [0.522, 0.629] | 30 |
| `txn.autocommit` | `transaction` | 21.98 ms | 20.25 ms | 0.926x | [0.901, 0.983] | 30 |
| `txn.batched` | `transaction` | 122.33 ms | 112.92 ms | 0.919x | [0.907, 0.971] | 30 |
| `txn.large` | `transaction` | 9.21 ms | 460.70 us | 0.050x | [0.048, 0.053] | 30 |
| `schema.index` | `schema` | 13.01 s | 245.67 ms | 0.019x | [0.019, 0.020] | 30 |
| `extension.json` | `extension` | 11.92 ms | 582.90 us | 0.049x | [0.045, 0.049] | 30 |
| `extension.fts.build` | `extension` | 19.34 ms | 2.54 ms | 0.128x | [0.122, 0.142] | 30 |
| `extension.fts.query` | `extension` | 52.43 ms | 4.80 ms | 0.091x | [0.092, 0.109] | 30 |
| `extension.rtree.insert` | `extension` | 4.91 ms | 2.17 ms | 0.438x | [0.416, 0.491] | 30 |
| `extension.rtree.query` | `extension` | 3.35 ms | 3.42 ms | 1.016x | [1.078, 1.409] | 30 |
| `large.read` | `large.values` | 9.67 ms | 12.62 ms | 1.291x | [1.253, 1.503] | 30 |
| `large.write` | `large.values` | 1.89 ms | 1.84 ms | 0.926x | [0.836, 1.004] | 30 |


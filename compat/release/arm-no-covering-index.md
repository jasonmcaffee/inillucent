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

Weighted geometric mean **0.208x**, 95% interval [0.205, 0.213]. The release bound is a lower bound of at least 1.50x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.210x | [0.144, 0.310] | loss | **below 0.90x** |
| `read.point` | 0.16 | 0.789x | [0.695, 0.898] | loss | **below 0.90x** |
| `read.range` | 0.12 | 0.128x | [0.097, 0.169] | loss | **below 0.90x** |
| `read.analytical` | 0.10 | 0.036x | [0.030, 0.042] | loss | **below 0.90x** |
| `read.join` | 0.08 | 0.186x | [0.125, 0.270] | loss | **below 0.90x** |
| `write` | 0.20 | 0.208x | [0.177, 0.244] | loss | **below 0.90x** |
| `transaction` | 0.10 | 0.304x | [0.221, 0.414] | loss | **below 0.90x** |
| `schema` | 0.04 | 0.210x | [0.202, 0.219] | loss | **below 0.90x** |
| `extension` | 0.08 | 0.103x | [0.083, 0.128] | loss | **below 0.90x** |
| `large.values` | 0.04 | 0.771x | [0.709, 0.837] | loss | **below 0.90x** |

### By workload

| workload | family | rust-db median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 35.19 ms | 1.90 ms | 0.049x | [0.046, 0.051] | 30 |
| `prepare.point` | `open.prepare` | 73.37 ms | 68.27 ms | 0.929x | [0.855, 0.972] | 30 |
| `point.rowid` | `read.point` | 33.84 ms | 55.12 ms | 1.557x | [1.459, 1.684] | 30 |
| `point.index` | `read.point` | 64.60 ms | 49.06 ms | 0.771x | [0.768, 0.884] | 30 |
| `point.miss` | `read.point` | 136.43 ms | 51.58 ms | 0.376x | [0.360, 0.409] | 30 |
| `range.covering` | `read.range` | 506.15 ms | 15.80 ms | 0.032x | [0.032, 0.036] | 30 |
| `range.lookaside` | `read.range` | 283.98 ms | 21.84 ms | 0.077x | [0.075, 0.083] | 30 |
| `range.reverse` | `read.range` | 19.41 ms | 14.52 ms | 0.750x | [0.725, 0.829] | 30 |
| `scan.aggregate` | `read.analytical` | 905.60 ms | 54.11 ms | 0.060x | [0.058, 0.062] | 30 |
| `scan.group` | `read.analytical` | 1.08 s | 49.70 ms | 0.046x | [0.045, 0.047] | 30 |
| `scan.sort` | `read.analytical` | 1.34 s | 94.22 ms | 0.070x | [0.070, 0.074] | 30 |
| `scan.distinct` | `read.analytical` | 1.23 s | 9.43 ms | 0.008x | [0.008, 0.009] | 30 |
| `join.selective` | `read.join` | 35.49 ms | 24.92 ms | 0.781x | [0.771, 0.890] | 30 |
| `join.range` | `read.join` | 467.42 ms | 18.29 ms | 0.039x | [0.040, 0.044] | 30 |
| `write.insert.batch` | `write` | 93.96 ms | 6.60 ms | 0.069x | [0.066, 0.081] | 30 |
| `write.insert.autocommit` | `write` | 131.95 ms | 130.66 ms | 0.995x | [0.971, 1.084] | 30 |
| `write.update.indexed` | `write` | 79.13 ms | 7.59 ms | 0.096x | [0.091, 0.098] | 30 |
| `write.delete` | `write` | 47.99 ms | 6.38 ms | 0.134x | [0.128, 0.138] | 30 |
| `write.upsert` | `write` | 9.86 ms | 4.03 ms | 0.420x | [0.388, 0.445] | 30 |
| `txn.autocommit` | `transaction` | 41.46 ms | 38.14 ms | 0.906x | [0.883, 0.956] | 30 |
| `txn.batched` | `transaction` | 317.56 ms | 260.91 ms | 0.824x | [0.806, 0.957] | 30 |
| `txn.large` | `transaction` | 19.77 ms | 690.80 us | 0.036x | [0.034, 0.036] | 30 |
| `schema.index` | `schema` | 14.89 ms | 3.15 ms | 0.208x | [0.202, 0.219] | 30 |
| `extension.json` | `extension` | 26.07 ms | 1.17 ms | 0.044x | [0.039, 0.045] | 30 |
| `extension.fts.build` | `extension` | 128.94 ms | 2.66 ms | 0.020x | [0.020, 0.027] | 30 |
| `extension.fts.query` | `extension` | 256.28 ms | 12.63 ms | 0.051x | [0.052, 0.060] | 30 |
| `extension.rtree.insert` | `extension` | 9.71 ms | 2.40 ms | 0.245x | [0.209, 0.269] | 30 |
| `extension.rtree.query` | `extension` | 8.29 ms | 7.15 ms | 0.891x | [0.831, 1.070] | 30 |
| `large.read` | `large.values` | 31.10 ms | 30.63 ms | 1.005x | [0.826, 1.038] | 30 |
| `large.write` | `large.values` | 3.26 ms | 2.19 ms | 0.634x | [0.593, 0.697] | 30 |


# rust-db performance scorecard

Label `release-candidate (no streaming-group)`, platform `windows-x86_64`, 30 paired rounds per scale, bootstrap seed 17900001. **Arm: `streaming-group` switched off.** This is one side of an A/B pair and not the shipped engine; compare it with the run whose arm is empty.

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

Weighted geometric mean **0.239x**, 95% interval [0.234, 0.247]. The release bound is a lower bound of at least 1.50x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.208x | [0.144, 0.303] | loss | **below 0.90x** |
| `read.point` | 0.16 | 0.781x | [0.687, 0.888] | loss | **below 0.90x** |
| `read.range` | 0.12 | 0.213x | [0.176, 0.262] | loss | **below 0.90x** |
| `read.analytical` | 0.10 | 0.058x | [0.051, 0.065] | loss | **below 0.90x** |
| `read.join` | 0.08 | 0.270x | [0.194, 0.371] | loss | **below 0.90x** |
| `write` | 0.20 | 0.211x | [0.181, 0.249] | loss | **below 0.90x** |
| `transaction` | 0.10 | 0.304x | [0.220, 0.413] | loss | **below 0.90x** |
| `schema` | 0.04 | 0.205x | [0.193, 0.216] | loss | **below 0.90x** |
| `extension` | 0.08 | 0.100x | [0.080, 0.123] | loss | **below 0.90x** |
| `large.values` | 0.04 | 0.781x | [0.706, 0.897] | loss | **below 0.90x** |

### By workload

| workload | family | rust-db median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 33.82 ms | 1.89 ms | 0.051x | [0.048, 0.054] | 30 |
| `prepare.point` | `open.prepare` | 67.47 ms | 59.81 ms | 0.835x | [0.782, 0.913] | 30 |
| `point.rowid` | `read.point` | 31.43 ms | 48.51 ms | 1.532x | [1.465, 1.688] | 30 |
| `point.index` | `read.point` | 62.32 ms | 49.52 ms | 0.780x | [0.772, 0.854] | 30 |
| `point.miss` | `read.point` | 126.02 ms | 46.86 ms | 0.372x | [0.356, 0.396] | 30 |
| `range.covering` | `read.range` | 102.81 ms | 15.49 ms | 0.151x | [0.150, 0.166] | 30 |
| `range.lookaside` | `read.range` | 261.23 ms | 20.16 ms | 0.077x | [0.077, 0.085] | 30 |
| `range.reverse` | `read.range` | 18.34 ms | 14.37 ms | 0.779x | [0.734, 0.819] | 30 |
| `scan.aggregate` | `read.analytical` | 607.15 ms | 50.19 ms | 0.083x | [0.082, 0.087] | 30 |
| `scan.group` | `read.analytical` | 566.17 ms | 45.88 ms | 0.081x | [0.081, 0.085] | 30 |
| `scan.sort` | `read.analytical` | 994.35 ms | 90.98 ms | 0.091x | [0.088, 0.095] | 30 |
| `scan.distinct` | `read.analytical` | 508.08 ms | 8.56 ms | 0.017x | [0.017, 0.019] | 30 |
| `join.selective` | `read.join` | 26.20 ms | 25.46 ms | 0.920x | [0.887, 1.012] | 30 |
| `join.range` | `read.join` | 241.85 ms | 17.85 ms | 0.075x | [0.075, 0.081] | 30 |
| `write.insert.batch` | `write` | 90.89 ms | 7.26 ms | 0.076x | [0.067, 0.080] | 30 |
| `write.insert.autocommit` | `write` | 135.05 ms | 135.81 ms | 0.999x | [0.966, 1.103] | 30 |
| `write.update.indexed` | `write` | 72.83 ms | 7.37 ms | 0.099x | [0.093, 0.102] | 30 |
| `write.delete` | `write` | 46.08 ms | 6.55 ms | 0.139x | [0.127, 0.141] | 30 |
| `write.upsert` | `write` | 9.97 ms | 4.28 ms | 0.419x | [0.392, 0.451] | 30 |
| `txn.autocommit` | `transaction` | 42.33 ms | 39.76 ms | 0.932x | [0.802, 1.007] | 30 |
| `txn.batched` | `transaction` | 322.42 ms | 268.43 ms | 0.852x | [0.794, 0.910] | 30 |
| `txn.large` | `transaction` | 18.69 ms | 673.50 us | 0.036x | [0.035, 0.038] | 30 |
| `schema.index` | `schema` | 14.73 ms | 2.98 ms | 0.204x | [0.193, 0.216] | 30 |
| `extension.json` | `extension` | 26.03 ms | 1.12 ms | 0.043x | [0.039, 0.044] | 30 |
| `extension.fts.build` | `extension` | 128.15 ms | 2.51 ms | 0.020x | [0.019, 0.020] | 30 |
| `extension.fts.query` | `extension` | 253.05 ms | 12.42 ms | 0.050x | [0.050, 0.058] | 30 |
| `extension.rtree.insert` | `extension` | 9.50 ms | 2.40 ms | 0.264x | [0.237, 0.271] | 30 |
| `extension.rtree.query` | `extension` | 8.19 ms | 6.76 ms | 0.846x | [0.793, 0.983] | 30 |
| `large.read` | `large.values` | 28.60 ms | 23.81 ms | 0.831x | [0.741, 0.864] | 30 |
| `large.write` | `large.values` | 3.40 ms | 2.34 ms | 0.672x | [0.631, 1.012] | 30 |


# inillucent performance scorecard

Label `no-streaming-group (no streaming-group)`, platform `windows-x86_64`, 30 paired rounds per scale, bootstrap seed 17900001. **Arm: `streaming-group` switched off.** This is one side of an A/B pair and not the shipped engine; compare it with the run whose arm is empty.

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

Weighted geometric mean **0.311x**, 95% interval [0.306, 0.319]. The release bound is a lower bound of at least 1.50x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.227x | [0.160, 0.325] | loss | **below 0.90x** |
| `read.point` | 0.16 | 0.873x | [0.767, 0.995] | loss | **below 0.90x** |
| `read.range` | 0.12 | 0.303x | [0.249, 0.372] | loss | **below 0.90x** |
| `read.analytical` | 0.10 | 0.099x | [0.086, 0.112] | loss | **below 0.90x** |
| `read.join` | 0.08 | 0.326x | [0.234, 0.448] | loss | **below 0.90x** |
| `write` | 0.20 | 0.277x | [0.239, 0.319] | loss | **below 0.90x** |
| `transaction` | 0.10 | 0.475x | [0.384, 0.584] | loss | **below 0.90x** |
| `schema` | 0.04 | 0.252x | [0.239, 0.264] | loss | **below 0.90x** |
| `extension` | 0.08 | 0.127x | [0.106, 0.153] | loss | **below 0.90x** |
| `large.values` | 0.04 | 0.804x | [0.758, 0.851] | loss | **below 0.90x** |

### By workload

| workload | family | inillucent median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 30.50 ms | 1.81 ms | 0.058x | [0.056, 0.060] | 30 |
| `prepare.point` | `open.prepare` | 61.86 ms | 54.46 ms | 0.881x | [0.863, 0.914] | 30 |
| `point.rowid` | `read.point` | 28.52 ms | 47.09 ms | 1.644x | [1.575, 1.682] | 30 |
| `point.index` | `read.point` | 45.38 ms | 47.62 ms | 1.052x | [1.034, 1.113] | 30 |
| `point.miss` | `read.point` | 121.63 ms | 45.99 ms | 0.379x | [0.368, 0.399] | 30 |
| `range.covering` | `read.range` | 63.98 ms | 15.30 ms | 0.240x | [0.238, 0.248] | 30 |
| `range.lookaside` | `read.range` | 188.44 ms | 19.86 ms | 0.105x | [0.102, 0.106] | 30 |
| `range.reverse` | `read.range` | 12.78 ms | 13.97 ms | 1.092x | [1.056, 1.140] | 30 |
| `scan.aggregate` | `read.analytical` | 299.43 ms | 49.55 ms | 0.165x | [0.165, 0.173] | 30 |
| `scan.group` | `read.analytical` | 305.79 ms | 45.34 ms | 0.148x | [0.146, 0.151] | 30 |
| `scan.sort` | `read.analytical` | 647.70 ms | 87.07 ms | 0.134x | [0.132, 0.138] | 30 |
| `scan.distinct` | `read.analytical` | 304.67 ms | 8.44 ms | 0.028x | [0.027, 0.029] | 30 |
| `join.selective` | `read.join` | 20.96 ms | 23.81 ms | 1.140x | [1.090, 1.197] | 30 |
| `join.range` | `read.join` | 190.50 ms | 17.61 ms | 0.093x | [0.092, 0.094] | 30 |
| `write.insert.batch` | `write` | 53.60 ms | 5.88 ms | 0.111x | [0.100, 0.138] | 30 |
| `write.insert.autocommit` | `write` | 129.02 ms | 127.87 ms | 0.984x | [0.927, 1.076] | 30 |
| `write.update.indexed` | `write` | 47.07 ms | 7.16 ms | 0.149x | [0.147, 0.185] | 30 |
| `write.delete` | `write` | 30.60 ms | 5.96 ms | 0.189x | [0.173, 0.237] | 30 |
| `write.upsert` | `write` | 8.49 ms | 4.06 ms | 0.451x | [0.353, 0.563] | 30 |
| `txn.autocommit` | `transaction` | 38.56 ms | 36.39 ms | 0.931x | [0.924, 0.979] | 30 |
| `txn.batched` | `transaction` | 270.97 ms | 254.21 ms | 0.929x | [0.872, 1.020] | 30 |
| `txn.large` | `transaction` | 5.61 ms | 661.20 us | 0.121x | [0.114, 0.125] | 30 |
| `schema.index` | `schema` | 11.71 ms | 2.93 ms | 0.252x | [0.239, 0.264] | 30 |
| `extension.json` | `extension` | 24.03 ms | 1.16 ms | 0.047x | [0.047, 0.049] | 30 |
| `extension.fts.build` | `extension` | 44.00 ms | 2.37 ms | 0.053x | [0.051, 0.057] | 30 |
| `extension.fts.query` | `extension` | 233.71 ms | 12.07 ms | 0.052x | [0.050, 0.056] | 30 |
| `extension.rtree.insert` | `extension` | 8.93 ms | 2.30 ms | 0.259x | [0.247, 0.286] | 30 |
| `extension.rtree.query` | `extension` | 7.67 ms | 6.67 ms | 0.876x | [0.813, 0.984] | 30 |
| `large.read` | `large.values` | 27.72 ms | 23.54 ms | 0.848x | [0.796, 0.878] | 30 |
| `large.write` | `large.values` | 2.83 ms | 2.28 ms | 0.778x | [0.698, 0.852] | 30 |


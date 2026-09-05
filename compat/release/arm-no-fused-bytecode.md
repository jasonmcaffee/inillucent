# inillucent performance scorecard

Label `no-fused-bytecode (no fused-bytecode)`, platform `windows-x86_64`, 30 paired rounds per scale, bootstrap seed 17900001. **Arm: `fused-bytecode` switched off.** This is one side of an A/B pair and not the shipped engine; compare it with the run whose arm is empty.

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

Weighted geometric mean **0.310x**, 95% interval [0.308, 0.318]. The release bound is a lower bound of at least 1.50x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.228x | [0.160, 0.326] | loss | **below 0.90x** |
| `read.point` | 0.16 | 0.862x | [0.759, 0.981] | loss | **below 0.90x** |
| `read.range` | 0.12 | 0.299x | [0.246, 0.366] | loss | **below 0.90x** |
| `read.analytical` | 0.10 | 0.112x | [0.101, 0.123] | loss | **below 0.90x** |
| `read.join` | 0.08 | 0.329x | [0.237, 0.452] | loss | **below 0.90x** |
| `write` | 0.20 | 0.265x | [0.233, 0.303] | loss | **below 0.90x** |
| `transaction` | 0.10 | 0.490x | [0.396, 0.604] | loss | **below 0.90x** |
| `schema` | 0.04 | 0.246x | [0.238, 0.255] | loss | **below 0.90x** |
| `extension` | 0.08 | 0.124x | [0.104, 0.149] | loss | **below 0.90x** |
| `large.values` | 0.04 | 0.827x | [0.772, 0.887] | loss | **below 0.90x** |

### By workload

| workload | family | inillucent median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 29.84 ms | 1.89 ms | 0.056x | [0.056, 0.060] | 30 |
| `prepare.point` | `open.prepare` | 61.13 ms | 54.82 ms | 0.891x | [0.872, 0.915] | 30 |
| `point.rowid` | `read.point` | 28.61 ms | 46.94 ms | 1.634x | [1.586, 1.658] | 30 |
| `point.index` | `read.point` | 45.88 ms | 47.40 ms | 1.034x | [1.013, 1.067] | 30 |
| `point.miss` | `read.point` | 120.82 ms | 45.62 ms | 0.376x | [0.372, 0.390] | 30 |
| `range.covering` | `read.range` | 66.66 ms | 15.21 ms | 0.229x | [0.227, 0.249] | 30 |
| `range.lookaside` | `read.range` | 191.94 ms | 19.81 ms | 0.103x | [0.102, 0.107] | 30 |
| `range.reverse` | `read.range` | 13.20 ms | 14.00 ms | 1.069x | [1.063, 1.120] | 30 |
| `scan.aggregate` | `read.analytical` | 329.77 ms | 49.36 ms | 0.150x | [0.149, 0.154] | 30 |
| `scan.group` | `read.analytical` | 238.75 ms | 45.16 ms | 0.189x | [0.185, 0.193] | 30 |
| `scan.sort` | `read.analytical` | 694.65 ms | 86.81 ms | 0.125x | [0.123, 0.128] | 30 |
| `scan.distinct` | `read.analytical` | 202.03 ms | 8.47 ms | 0.042x | [0.042, 0.046] | 30 |
| `join.selective` | `read.join` | 21.05 ms | 23.91 ms | 1.127x | [1.078, 1.206] | 30 |
| `join.range` | `read.join` | 191.29 ms | 17.62 ms | 0.093x | [0.092, 0.099] | 30 |
| `write.insert.batch` | `write` | 54.47 ms | 5.79 ms | 0.106x | [0.100, 0.132] | 30 |
| `write.insert.autocommit` | `write` | 129.07 ms | 129.71 ms | 1.003x | [0.982, 1.027] | 30 |
| `write.update.indexed` | `write` | 47.06 ms | 7.10 ms | 0.150x | [0.147, 0.155] | 30 |
| `write.delete` | `write` | 31.43 ms | 6.02 ms | 0.189x | [0.179, 0.191] | 30 |
| `write.upsert` | `write` | 9.49 ms | 3.71 ms | 0.418x | [0.391, 0.448] | 30 |
| `txn.autocommit` | `transaction` | 37.78 ms | 36.34 ms | 0.961x | [0.952, 1.142] | 30 |
| `txn.batched` | `transaction` | 274.24 ms | 246.11 ms | 0.894x | [0.883, 1.076] | 30 |
| `txn.large` | `transaction` | 5.69 ms | 668.85 us | 0.118x | [0.115, 0.125] | 30 |
| `schema.index` | `schema` | 11.58 ms | 2.85 ms | 0.249x | [0.238, 0.255] | 30 |
| `extension.json` | `extension` | 24.25 ms | 1.14 ms | 0.047x | [0.043, 0.048] | 30 |
| `extension.fts.build` | `extension` | 42.96 ms | 2.45 ms | 0.057x | [0.052, 0.059] | 30 |
| `extension.fts.query` | `extension` | 236.36 ms | 12.09 ms | 0.051x | [0.052, 0.059] | 30 |
| `extension.rtree.insert` | `extension` | 9.09 ms | 2.19 ms | 0.245x | [0.239, 0.258] | 30 |
| `extension.rtree.query` | `extension` | 7.65 ms | 6.60 ms | 0.863x | [0.760, 0.940] | 30 |
| `large.read` | `large.values` | 27.35 ms | 23.51 ms | 0.855x | [0.812, 0.935] | 30 |
| `large.write` | `large.values` | 3.14 ms | 2.40 ms | 0.812x | [0.703, 0.885] | 30 |


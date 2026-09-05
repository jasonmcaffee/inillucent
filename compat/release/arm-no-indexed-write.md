# inillucent performance scorecard

Label `no-indexed-write (no indexed-write)`, platform `windows-x86_64`, 30 paired rounds per scale, bootstrap seed 17900001. **Arm: `indexed-write` switched off.** This is one side of an A/B pair and not the shipped engine; compare it with the run whose arm is empty.

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

Weighted geometric mean **0.220x**, 95% interval [0.218, 0.224]. The release bound is a lower bound of at least 1.50x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.226x | [0.159, 0.327] | loss | **below 0.90x** |
| `read.point` | 0.16 | 0.875x | [0.770, 0.992] | loss | **below 0.90x** |
| `read.range` | 0.12 | 0.307x | [0.252, 0.378] | loss | **below 0.90x** |
| `read.analytical` | 0.10 | 0.122x | [0.110, 0.135] | loss | **below 0.90x** |
| `read.join` | 0.08 | 0.334x | [0.239, 0.461] | loss | **below 0.90x** |
| `write` | 0.20 | 0.086x | [0.064, 0.117] | loss | **below 0.90x** |
| `transaction` | 0.10 | 0.142x | [0.087, 0.228] | loss | **below 0.90x** |
| `schema` | 0.04 | 0.217x | [0.206, 0.232] | loss | **below 0.90x** |
| `extension` | 0.08 | 0.126x | [0.104, 0.151] | loss | **below 0.90x** |
| `large.values` | 0.04 | 0.628x | [0.568, 0.691] | loss | **below 0.90x** |

### By workload

| workload | family | inillucent median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 30.82 ms | 1.91 ms | 0.059x | [0.055, 0.059] | 30 |
| `prepare.point` | `open.prepare` | 62.37 ms | 53.83 ms | 0.861x | [0.838, 0.955] | 30 |
| `point.rowid` | `read.point` | 28.61 ms | 46.86 ms | 1.640x | [1.579, 1.729] | 30 |
| `point.index` | `read.point` | 45.75 ms | 47.86 ms | 1.048x | [1.023, 1.068] | 30 |
| `point.miss` | `read.point` | 120.56 ms | 45.96 ms | 0.382x | [0.373, 0.409] | 30 |
| `range.covering` | `read.range` | 63.95 ms | 15.35 ms | 0.241x | [0.236, 0.259] | 30 |
| `range.lookaside` | `read.range` | 189.47 ms | 19.93 ms | 0.105x | [0.102, 0.109] | 30 |
| `range.reverse` | `read.range` | 12.88 ms | 14.07 ms | 1.092x | [1.075, 1.173] | 30 |
| `scan.aggregate` | `read.analytical` | 296.78 ms | 49.74 ms | 0.168x | [0.167, 0.174] | 30 |
| `scan.group` | `read.analytical` | 221.41 ms | 45.13 ms | 0.204x | [0.202, 0.208] | 30 |
| `scan.sort` | `read.analytical` | 646.86 ms | 87.53 ms | 0.136x | [0.131, 0.138] | 30 |
| `scan.distinct` | `read.analytical` | 187.43 ms | 8.49 ms | 0.045x | [0.045, 0.050] | 30 |
| `join.selective` | `read.join` | 21.11 ms | 24.00 ms | 1.145x | [1.131, 1.244] | 30 |
| `join.range` | `read.join` | 189.81 ms | 17.55 ms | 0.093x | [0.092, 0.098] | 30 |
| `write.insert.batch` | `write` | 55.39 ms | 6.35 ms | 0.111x | [0.104, 0.140] | 30 |
| `write.insert.autocommit` | `write` | 125.48 ms | 126.52 ms | 1.005x | [0.982, 1.098] | 30 |
| `write.update.indexed` | `write` | 698.08 ms | 6.98 ms | 0.010x | [0.010, 0.010] | 30 |
| `write.delete` | `write` | 591.11 ms | 5.98 ms | 0.010x | [0.010, 0.010] | 30 |
| `write.upsert` | `write` | 9.61 ms | 3.65 ms | 0.372x | [0.373, 0.427] | 30 |
| `txn.autocommit` | `transaction` | 45.59 ms | 37.23 ms | 0.831x | [0.797, 0.843] | 30 |
| `txn.batched` | `transaction` | 404.95 ms | 252.22 ms | 0.619x | [0.602, 0.727] | 30 |
| `txn.large` | `transaction` | 126.58 ms | 667.15 us | 0.005x | [0.005, 0.006] | 30 |
| `schema.index` | `schema` | 13.44 ms | 2.84 ms | 0.215x | [0.206, 0.232] | 30 |
| `extension.json` | `extension` | 24.16 ms | 1.13 ms | 0.047x | [0.044, 0.047] | 30 |
| `extension.fts.build` | `extension` | 43.05 ms | 2.29 ms | 0.054x | [0.054, 0.058] | 30 |
| `extension.fts.query` | `extension` | 233.62 ms | 11.96 ms | 0.051x | [0.050, 0.057] | 30 |
| `extension.rtree.insert` | `extension` | 8.57 ms | 2.18 ms | 0.269x | [0.253, 0.283] | 30 |
| `extension.rtree.query` | `extension` | 7.63 ms | 6.63 ms | 0.867x | [0.787, 0.930] | 30 |
| `large.read` | `large.values` | 27.68 ms | 23.56 ms | 0.854x | [0.833, 0.936] | 30 |
| `large.write` | `large.values` | 5.35 ms | 2.33 ms | 0.447x | [0.418, 0.488] | 30 |


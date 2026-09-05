# inillucent performance scorecard

Label `no-covering-index (no covering-index)`, platform `windows-x86_64`, 30 paired rounds per scale, bootstrap seed 17900001. **Arm: `covering-index` switched off.** This is one side of an A/B pair and not the shipped engine; compare it with the run whose arm is empty.

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

Weighted geometric mean **0.260x**, 95% interval [0.257, 0.264]. The release bound is a lower bound of at least 1.50x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.216x | [0.153, 0.310] | loss | **below 0.90x** |
| `read.point` | 0.16 | 0.878x | [0.775, 0.995] | loss | **below 0.90x** |
| `read.range` | 0.12 | 0.168x | [0.127, 0.222] | loss | **below 0.90x** |
| `read.analytical` | 0.10 | 0.052x | [0.044, 0.062] | loss | **below 0.90x** |
| `read.join` | 0.08 | 0.215x | [0.145, 0.316] | loss | **below 0.90x** |
| `write` | 0.20 | 0.257x | [0.226, 0.295] | loss | **below 0.90x** |
| `transaction` | 0.10 | 0.477x | [0.387, 0.584] | loss | **below 0.90x** |
| `schema` | 0.04 | 0.250x | [0.239, 0.261] | loss | **below 0.90x** |
| `extension` | 0.08 | 0.130x | [0.108, 0.157] | loss | **below 0.90x** |
| `large.values` | 0.04 | 0.848x | [0.797, 0.903] | loss | **below 0.90x** |

### By workload

| workload | family | inillucent median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 30.54 ms | 1.64 ms | 0.053x | [0.052, 0.058] | 30 |
| `prepare.point` | `open.prepare` | 61.51 ms | 52.77 ms | 0.857x | [0.812, 0.886] | 30 |
| `point.rowid` | `read.point` | 28.54 ms | 46.67 ms | 1.629x | [1.578, 1.743] | 30 |
| `point.index` | `read.point` | 46.44 ms | 47.55 ms | 1.027x | [1.012, 1.051] | 30 |
| `point.miss` | `read.point` | 120.88 ms | 46.28 ms | 0.385x | [0.386, 0.410] | 30 |
| `range.covering` | `read.range` | 358.02 ms | 15.49 ms | 0.043x | [0.043, 0.049] | 30 |
| `range.lookaside` | `read.range` | 202.02 ms | 19.75 ms | 0.098x | [0.095, 0.101] | 30 |
| `range.reverse` | `read.range` | 13.52 ms | 14.05 ms | 1.040x | [1.005, 1.110] | 30 |
| `scan.aggregate` | `read.analytical` | 454.86 ms | 49.74 ms | 0.109x | [0.107, 0.112] | 30 |
| `scan.group` | `read.analytical` | 670.61 ms | 45.35 ms | 0.067x | [0.066, 0.069] | 30 |
| `scan.sort` | `read.analytical` | 911.02 ms | 87.22 ms | 0.095x | [0.094, 0.099] | 30 |
| `scan.distinct` | `read.analytical` | 842.65 ms | 8.42 ms | 0.010x | [0.010, 0.011] | 30 |
| `join.selective` | `read.join` | 24.99 ms | 23.89 ms | 0.941x | [0.919, 0.980] | 30 |
| `join.range` | `read.join` | 354.85 ms | 17.58 ms | 0.050x | [0.048, 0.050] | 30 |
| `write.insert.batch` | `write` | 54.62 ms | 5.53 ms | 0.102x | [0.101, 0.115] | 30 |
| `write.insert.autocommit` | `write` | 129.35 ms | 127.91 ms | 0.991x | [0.956, 1.036] | 30 |
| `write.update.indexed` | `write` | 48.58 ms | 7.07 ms | 0.142x | [0.125, 0.143] | 30 |
| `write.delete` | `write` | 32.17 ms | 6.02 ms | 0.190x | [0.172, 0.192] | 30 |
| `write.upsert` | `write` | 8.95 ms | 3.76 ms | 0.414x | [0.399, 0.457] | 30 |
| `txn.autocommit` | `transaction` | 38.51 ms | 36.77 ms | 0.977x | [0.925, 0.997] | 30 |
| `txn.batched` | `transaction` | 275.29 ms | 247.08 ms | 0.910x | [0.891, 1.010] | 30 |
| `txn.large` | `transaction` | 5.69 ms | 666.15 us | 0.118x | [0.116, 0.123] | 30 |
| `schema.index` | `schema` | 11.64 ms | 2.87 ms | 0.243x | [0.239, 0.261] | 30 |
| `extension.json` | `extension` | 24.07 ms | 1.14 ms | 0.047x | [0.044, 0.048] | 30 |
| `extension.fts.build` | `extension` | 42.37 ms | 2.30 ms | 0.055x | [0.055, 0.059] | 30 |
| `extension.fts.query` | `extension` | 234.40 ms | 12.04 ms | 0.051x | [0.053, 0.059] | 30 |
| `extension.rtree.insert` | `extension` | 8.58 ms | 2.28 ms | 0.270x | [0.254, 0.277] | 30 |
| `extension.rtree.query` | `extension` | 7.60 ms | 6.71 ms | 0.877x | [0.856, 1.060] | 30 |
| `large.read` | `large.values` | 27.37 ms | 23.52 ms | 0.859x | [0.828, 0.964] | 30 |
| `large.write` | `large.values` | 2.86 ms | 2.27 ms | 0.769x | [0.731, 0.897] | 30 |


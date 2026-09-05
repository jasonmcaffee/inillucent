# inillucent performance scorecard

Label `no-ordered-walk (no ordered-walk)`, platform `windows-x86_64`, 30 paired rounds per scale, bootstrap seed 17900001. **Arm: `ordered-walk` switched off.** This is one side of an A/B pair and not the shipped engine; compare it with the run whose arm is empty.

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

Weighted geometric mean **0.266x**, 95% interval [0.261, 0.270]. The release bound is a lower bound of at least 1.50x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.219x | [0.154, 0.314] | loss | **below 0.90x** |
| `read.point` | 0.16 | 0.858x | [0.753, 0.978] | loss | **below 0.90x** |
| `read.range` | 0.12 | 0.085x | [0.069, 0.103] | loss | **below 0.90x** |
| `read.analytical` | 0.10 | 0.112x | [0.101, 0.125] | loss | **below 0.90x** |
| `read.join` | 0.08 | 0.320x | [0.232, 0.439] | loss | **below 0.90x** |
| `write` | 0.20 | 0.252x | [0.218, 0.291] | loss | **below 0.90x** |
| `transaction` | 0.10 | 0.491x | [0.395, 0.603] | loss | **below 0.90x** |
| `schema` | 0.04 | 0.251x | [0.239, 0.263] | loss | **below 0.90x** |
| `extension` | 0.08 | 0.127x | [0.106, 0.152] | loss | **below 0.90x** |
| `large.values` | 0.04 | 0.832x | [0.781, 0.890] | loss | **below 0.90x** |

### By workload

| workload | family | inillucent median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 30.56 ms | 1.75 ms | 0.056x | [0.053, 0.058] | 30 |
| `prepare.point` | `open.prepare` | 62.18 ms | 53.78 ms | 0.856x | [0.812, 0.905] | 30 |
| `point.rowid` | `read.point` | 28.62 ms | 46.64 ms | 1.628x | [1.539, 1.743] | 30 |
| `point.index` | `read.point` | 46.54 ms | 47.48 ms | 1.019x | [1.007, 1.062] | 30 |
| `point.miss` | `read.point` | 120.87 ms | 45.97 ms | 0.379x | [0.365, 0.380] | 30 |
| `range.covering` | `read.range` | 64.19 ms | 15.22 ms | 0.238x | [0.234, 0.246] | 30 |
| `range.lookaside` | `read.range` | 191.56 ms | 19.80 ms | 0.103x | [0.099, 0.104] | 30 |
| `range.reverse` | `read.range` | 547.83 ms | 13.93 ms | 0.025x | [0.025, 0.025] | 30 |
| `scan.aggregate` | `read.analytical` | 293.72 ms | 49.38 ms | 0.168x | [0.168, 0.175] | 30 |
| `scan.group` | `read.analytical` | 224.20 ms | 45.57 ms | 0.203x | [0.199, 0.208] | 30 |
| `scan.sort` | `read.analytical` | 854.33 ms | 86.71 ms | 0.102x | [0.101, 0.106] | 30 |
| `scan.distinct` | `read.analytical` | 189.98 ms | 8.41 ms | 0.044x | [0.043, 0.045] | 30 |
| `join.selective` | `read.join` | 21.76 ms | 23.77 ms | 1.093x | [1.058, 1.111] | 30 |
| `join.range` | `read.join` | 188.86 ms | 17.59 ms | 0.093x | [0.093, 0.097] | 30 |
| `write.insert.batch` | `write` | 53.69 ms | 5.90 ms | 0.108x | [0.098, 0.118] | 30 |
| `write.insert.autocommit` | `write` | 126.08 ms | 128.68 ms | 1.021x | [0.961, 1.112] | 30 |
| `write.update.indexed` | `write` | 48.53 ms | 7.07 ms | 0.145x | [0.134, 0.148] | 30 |
| `write.delete` | `write` | 31.65 ms | 5.83 ms | 0.182x | [0.163, 0.186] | 30 |
| `write.upsert` | `write` | 9.12 ms | 3.71 ms | 0.423x | [0.282, 0.447] | 30 |
| `txn.autocommit` | `transaction` | 36.09 ms | 37.32 ms | 1.008x | [0.988, 1.195] | 30 |
| `txn.batched` | `transaction` | 270.56 ms | 250.28 ms | 0.947x | [0.894, 0.996] | 30 |
| `txn.large` | `transaction` | 5.81 ms | 674.85 us | 0.118x | [0.114, 0.122] | 30 |
| `schema.index` | `schema` | 11.84 ms | 3.07 ms | 0.247x | [0.239, 0.263] | 30 |
| `extension.json` | `extension` | 24.04 ms | 1.16 ms | 0.047x | [0.044, 0.049] | 30 |
| `extension.fts.build` | `extension` | 42.42 ms | 2.54 ms | 0.060x | [0.057, 0.061] | 30 |
| `extension.fts.query` | `extension` | 236.67 ms | 12.08 ms | 0.052x | [0.051, 0.058] | 30 |
| `extension.rtree.insert` | `extension` | 8.61 ms | 2.16 ms | 0.259x | [0.216, 0.276] | 30 |
| `extension.rtree.query` | `extension` | 7.62 ms | 6.64 ms | 0.868x | [0.792, 0.982] | 30 |
| `large.read` | `large.values` | 27.31 ms | 23.45 ms | 0.861x | [0.863, 0.963] | 30 |
| `large.write` | `large.values` | 2.95 ms | 2.27 ms | 0.785x | [0.685, 0.849] | 30 |


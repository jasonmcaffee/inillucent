# rust-db performance scorecard

Label `release-candidate (no fused-bytecode)`, platform `windows-x86_64`, 30 paired rounds per scale, bootstrap seed 17900001. **Arm: `fused-bytecode` switched off.** This is one side of an A/B pair and not the shipped engine; compare it with the run whose arm is empty.

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

Weighted geometric mean **0.244x**, 95% interval [0.240, 0.250]. The release bound is a lower bound of at least 1.50x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.216x | [0.149, 0.313] | loss | **below 0.90x** |
| `read.point` | 0.16 | 0.826x | [0.725, 0.941] | loss | **below 0.90x** |
| `read.range` | 0.12 | 0.216x | [0.178, 0.268] | loss | **below 0.90x** |
| `read.analytical` | 0.10 | 0.060x | [0.053, 0.067] | loss | **below 0.90x** |
| `read.join` | 0.08 | 0.275x | [0.198, 0.378] | loss | **below 0.90x** |
| `write` | 0.20 | 0.219x | [0.185, 0.259] | loss | **below 0.90x** |
| `transaction` | 0.10 | 0.298x | [0.216, 0.408] | loss | **below 0.90x** |
| `schema` | 0.04 | 0.205x | [0.198, 0.212] | loss | **below 0.90x** |
| `extension` | 0.08 | 0.097x | [0.079, 0.122] | loss | **below 0.90x** |
| `large.values` | 0.04 | 0.781x | [0.731, 0.835] | loss | **below 0.90x** |

### By workload

| workload | family | rust-db median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 32.20 ms | 1.89 ms | 0.053x | [0.048, 0.055] | 30 |
| `prepare.point` | `open.prepare` | 65.30 ms | 59.90 ms | 0.882x | [0.838, 0.981] | 30 |
| `point.rowid` | `read.point` | 31.19 ms | 50.42 ms | 1.541x | [1.518, 1.818] | 30 |
| `point.index` | `read.point` | 63.39 ms | 53.35 ms | 0.820x | [0.811, 0.954] | 30 |
| `point.miss` | `read.point` | 129.68 ms | 47.88 ms | 0.377x | [0.363, 0.418] | 30 |
| `range.covering` | `read.range` | 110.21 ms | 15.68 ms | 0.144x | [0.146, 0.165] | 30 |
| `range.lookaside` | `read.range` | 269.04 ms | 20.36 ms | 0.080x | [0.076, 0.084] | 30 |
| `range.reverse` | `read.range` | 19.39 ms | 14.56 ms | 0.754x | [0.753, 0.906] | 30 |
| `scan.aggregate` | `read.analytical` | 687.89 ms | 52.24 ms | 0.076x | [0.076, 0.081] | 30 |
| `scan.group` | `read.analytical` | 484.21 ms | 46.15 ms | 0.097x | [0.097, 0.103] | 30 |
| `scan.sort` | `read.analytical` | 1.11 s | 92.41 ms | 0.083x | [0.082, 0.088] | 30 |
| `scan.distinct` | `read.analytical` | 498.45 ms | 8.68 ms | 0.017x | [0.018, 0.020] | 30 |
| `join.selective` | `read.join` | 26.17 ms | 24.64 ms | 0.927x | [0.884, 1.042] | 30 |
| `join.range` | `read.join` | 248.44 ms | 18.05 ms | 0.073x | [0.075, 0.084] | 30 |
| `write.insert.batch` | `write` | 91.59 ms | 6.28 ms | 0.070x | [0.067, 0.102] | 30 |
| `write.insert.autocommit` | `write` | 131.18 ms | 133.17 ms | 1.038x | [1.007, 1.118] | 30 |
| `write.update.indexed` | `write` | 75.24 ms | 7.51 ms | 0.098x | [0.094, 0.100] | 30 |
| `write.delete` | `write` | 46.78 ms | 6.49 ms | 0.134x | [0.129, 0.139] | 30 |
| `write.upsert` | `write` | 10.09 ms | 4.21 ms | 0.429x | [0.393, 0.576] | 30 |
| `txn.autocommit` | `transaction` | 41.92 ms | 39.04 ms | 0.899x | [0.876, 0.945] | 30 |
| `txn.batched` | `transaction` | 319.91 ms | 264.00 ms | 0.829x | [0.773, 0.873] | 30 |
| `txn.large` | `transaction` | 19.86 ms | 678.00 us | 0.035x | [0.034, 0.037] | 30 |
| `schema.index` | `schema` | 15.15 ms | 3.07 ms | 0.204x | [0.198, 0.212] | 30 |
| `extension.json` | `extension` | 25.68 ms | 1.19 ms | 0.045x | [0.039, 0.044] | 30 |
| `extension.fts.build` | `extension` | 129.59 ms | 2.46 ms | 0.019x | [0.019, 0.020] | 30 |
| `extension.fts.query` | `extension` | 258.96 ms | 12.20 ms | 0.048x | [0.049, 0.055] | 30 |
| `extension.rtree.insert` | `extension` | 9.66 ms | 2.36 ms | 0.248x | [0.210, 0.261] | 30 |
| `extension.rtree.query` | `extension` | 8.40 ms | 6.90 ms | 0.827x | [0.792, 0.976] | 30 |
| `large.read` | `large.values` | 31.29 ms | 24.11 ms | 0.825x | [0.762, 0.946] | 30 |
| `large.write` | `large.values` | 3.04 ms | 2.22 ms | 0.711x | [0.670, 0.775] | 30 |


# rust-db performance scorecard

Label `release-candidate (no indexed-write)`, platform `windows-x86_64`, 30 paired rounds per scale, bootstrap seed 17900001. **Arm: `indexed-write` switched off.** This is one side of an A/B pair and not the shipped engine; compare it with the run whose arm is empty.

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

Weighted geometric mean **0.138x**, 95% interval [0.135, 0.145]. The release bound is a lower bound of at least 1.50x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.198x | [0.135, 0.295] | loss | **below 0.90x** |
| `read.point` | 0.16 | 0.873x | [0.761, 1.003] | inconclusive | **below 0.90x** |
| `read.range` | 0.12 | 0.061x | [0.049, 0.076] | loss | **below 0.90x** |
| `read.analytical` | 0.10 | 0.054x | [0.047, 0.060] | loss | **below 0.90x** |
| `read.join` | 0.08 | 0.287x | [0.205, 0.395] | loss | **below 0.90x** |
| `write` | 0.20 | 0.065x | [0.046, 0.091] | loss | **below 0.90x** |
| `transaction` | 0.10 | 0.099x | [0.058, 0.165] | loss | **below 0.90x** |
| `schema` | 0.04 | 0.082x | [0.075, 0.087] | loss | **below 0.90x** |
| `extension` | 0.08 | 0.100x | [0.080, 0.124] | loss | **below 0.90x** |
| `large.values` | 0.04 | 0.499x | [0.428, 0.583] | loss | **below 0.90x** |

### By workload

| workload | family | rust-db median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 42.30 ms | 1.93 ms | 0.044x | [0.042, 0.047] | 30 |
| `prepare.point` | `open.prepare` | 77.93 ms | 72.76 ms | 0.907x | [0.819, 0.971] | 30 |
| `point.rowid` | `read.point` | 31.80 ms | 57.40 ms | 1.670x | [1.624, 1.898] | 30 |
| `point.index` | `read.point` | 63.91 ms | 57.65 ms | 0.888x | [0.864, 1.046] | 30 |
| `point.miss` | `read.point` | 139.63 ms | 58.22 ms | 0.394x | [0.371, 0.430] | 30 |
| `range.covering` | `read.range` | 106.88 ms | 17.53 ms | 0.161x | [0.169, 0.208] | 30 |
| `range.lookaside` | `read.range` | 303.04 ms | 20.37 ms | 0.074x | [0.071, 0.083] | 30 |
| `range.reverse` | `read.range` | 1.08 s | 15.54 ms | 0.014x | [0.015, 0.018] | 30 |
| `scan.aggregate` | `read.analytical` | 710.07 ms | 53.43 ms | 0.076x | [0.076, 0.089] | 30 |
| `scan.group` | `read.analytical` | 616.82 ms | 52.24 ms | 0.083x | [0.081, 0.093] | 30 |
| `scan.sort` | `read.analytical` | 1.48 s | 96.62 ms | 0.066x | [0.064, 0.076] | 30 |
| `scan.distinct` | `read.analytical` | 561.33 ms | 8.77 ms | 0.016x | [0.016, 0.019] | 30 |
| `join.selective` | `read.join` | 28.56 ms | 25.01 ms | 0.921x | [0.862, 1.154] | 30 |
| `join.range` | `read.join` | 244.45 ms | 18.43 ms | 0.077x | [0.078, 0.091] | 30 |
| `write.insert.batch` | `write` | 78.17 ms | 7.47 ms | 0.096x | [0.087, 0.124] | 30 |
| `write.insert.autocommit` | `write` | 138.88 ms | 131.87 ms | 0.947x | [0.872, 1.085] | 30 |
| `write.update.indexed` | `write` | 1.41 s | 7.69 ms | 0.005x | [0.005, 0.006] | 30 |
| `write.delete` | `write` | 1.20 s | 6.58 ms | 0.005x | [0.005, 0.006] | 30 |
| `write.upsert` | `write` | 10.10 ms | 4.16 ms | 0.410x | [0.335, 0.431] | 30 |
| `txn.autocommit` | `transaction` | 56.02 ms | 38.74 ms | 0.708x | [0.646, 0.739] | 30 |
| `txn.batched` | `transaction` | 558.47 ms | 264.39 ms | 0.467x | [0.466, 0.544] | 30 |
| `txn.large` | `transaction` | 256.41 ms | 707.70 us | 0.003x | [0.003, 0.003] | 30 |
| `schema.index` | `schema` | 39.08 ms | 3.17 ms | 0.085x | [0.075, 0.087] | 30 |
| `extension.json` | `extension` | 25.79 ms | 1.20 ms | 0.044x | [0.040, 0.045] | 30 |
| `extension.fts.build` | `extension` | 130.25 ms | 2.77 ms | 0.021x | [0.020, 0.022] | 30 |
| `extension.fts.query` | `extension` | 259.99 ms | 13.20 ms | 0.052x | [0.053, 0.062] | 30 |
| `extension.rtree.insert` | `extension` | 10.28 ms | 2.59 ms | 0.245x | [0.195, 0.260] | 30 |
| `extension.rtree.query` | `extension` | 8.63 ms | 7.10 ms | 0.860x | [0.763, 0.969] | 30 |
| `large.read` | `large.values` | 32.96 ms | 27.49 ms | 0.833x | [0.770, 0.944] | 30 |
| `large.write` | `large.values` | 7.99 ms | 2.42 ms | 0.292x | [0.272, 0.318] | 30 |


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

Weighted geometric mean **0.166x**, 95% interval [0.163, 0.169]. The release bound is a lower bound of at least 1.50x.

### By family

| family | weight | ratio | 95% interval | verdict | required floor |
|---|---:|---:|---|---|---|
| `open.prepare` | 0.08 | 0.198x | [0.136, 0.288] | loss | **below 0.90x** |
| `read.point` | 0.16 | 0.834x | [0.738, 0.940] | loss | **below 0.90x** |
| `read.range` | 0.12 | 0.221x | [0.183, 0.271] | loss | **below 0.90x** |
| `read.analytical` | 0.10 | 0.066x | [0.058, 0.074] | loss | **below 0.90x** |
| `read.join` | 0.08 | 0.272x | [0.196, 0.376] | loss | **below 0.90x** |
| `write` | 0.20 | 0.061x | [0.043, 0.086] | loss | **below 0.90x** |
| `transaction` | 0.10 | 0.092x | [0.054, 0.153] | loss | **below 0.90x** |
| `schema` | 0.04 | 0.198x | [0.189, 0.208] | loss | **below 0.90x** |
| `extension` | 0.08 | 0.095x | [0.078, 0.119] | loss | **below 0.90x** |
| `large.values` | 0.04 | 0.481x | [0.394, 0.575] | loss | **below 0.90x** |

### By workload

| workload | family | rust-db median | SQLite median | ratio | 95% interval | samples |
|---|---|---:|---:|---:|---|---:|
| `prepare.trivial` | `open.prepare` | 36.73 ms | 1.89 ms | 0.049x | [0.045, 0.051] | 30 |
| `prepare.point` | `open.prepare` | 76.60 ms | 61.63 ms | 0.804x | [0.771, 0.864] | 30 |
| `point.rowid` | `read.point` | 31.97 ms | 50.09 ms | 1.559x | [1.486, 1.697] | 30 |
| `point.index` | `read.point` | 64.03 ms | 56.42 ms | 0.855x | [0.836, 0.951] | 30 |
| `point.miss` | `read.point` | 135.45 ms | 57.15 ms | 0.389x | [0.386, 0.440] | 30 |
| `range.covering` | `read.range` | 105.48 ms | 16.09 ms | 0.152x | [0.154, 0.173] | 30 |
| `range.lookaside` | `read.range` | 267.27 ms | 21.53 ms | 0.083x | [0.081, 0.088] | 30 |
| `range.reverse` | `read.range` | 19.65 ms | 14.79 ms | 0.785x | [0.734, 0.858] | 30 |
| `scan.aggregate` | `read.analytical` | 633.07 ms | 55.00 ms | 0.086x | [0.084, 0.089] | 30 |
| `scan.group` | `read.analytical` | 445.86 ms | 49.73 ms | 0.111x | [0.108, 0.115] | 30 |
| `scan.sort` | `read.analytical` | 1.02 s | 96.59 ms | 0.093x | [0.090, 0.095] | 30 |
| `scan.distinct` | `read.analytical` | 469.18 ms | 8.84 ms | 0.019x | [0.020, 0.023] | 30 |
| `join.selective` | `read.join` | 26.74 ms | 24.80 ms | 0.914x | [0.882, 1.029] | 30 |
| `join.range` | `read.join` | 249.01 ms | 18.44 ms | 0.074x | [0.075, 0.082] | 30 |
| `write.insert.batch` | `write` | 93.01 ms | 6.21 ms | 0.066x | [0.062, 0.072] | 30 |
| `write.insert.autocommit` | `write` | 129.21 ms | 129.02 ms | 0.990x | [0.973, 1.047] | 30 |
| `write.update.indexed` | `write` | 1.38 s | 7.68 ms | 0.006x | [0.005, 0.006] | 30 |
| `write.delete` | `write` | 1.17 s | 6.41 ms | 0.005x | [0.005, 0.006] | 30 |
| `write.upsert` | `write` | 9.90 ms | 4.00 ms | 0.400x | [0.383, 0.423] | 30 |
| `txn.autocommit` | `transaction` | 57.80 ms | 37.09 ms | 0.649x | [0.616, 0.667] | 30 |
| `txn.batched` | `transaction` | 563.11 ms | 256.70 ms | 0.445x | [0.438, 0.465] | 30 |
| `txn.large` | `transaction` | 265.01 ms | 691.00 us | 0.003x | [0.003, 0.003] | 30 |
| `schema.index` | `schema` | 15.33 ms | 2.92 ms | 0.194x | [0.189, 0.208] | 30 |
| `extension.json` | `extension` | 26.15 ms | 1.17 ms | 0.043x | [0.039, 0.044] | 30 |
| `extension.fts.build` | `extension` | 131.38 ms | 2.55 ms | 0.019x | [0.018, 0.020] | 30 |
| `extension.fts.query` | `extension` | 256.08 ms | 12.42 ms | 0.049x | [0.050, 0.057] | 30 |
| `extension.rtree.insert` | `extension` | 10.01 ms | 2.35 ms | 0.233x | [0.227, 0.248] | 30 |
| `extension.rtree.query` | `extension` | 8.41 ms | 6.89 ms | 0.819x | [0.721, 0.903] | 30 |
| `large.read` | `large.values` | 32.49 ms | 27.25 ms | 0.835x | [0.803, 0.947] | 30 |
| `large.write` | `large.values` | 7.57 ms | 2.31 ms | 0.302x | [0.215, 0.314] | 30 |


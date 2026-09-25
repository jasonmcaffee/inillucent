# inillucent performance dashboard

Platform `windows-x86_64`. Every number is the paired speed ratio, SQLite over inillucent, so above one is faster than the reference. Columns are runs in the order they were taken.

## Scale `medium`

| workload | nightly-20260925-072550 |
|---|---:|
| `*headline*` | 0.759x |
| `*family* open.prepare` | 0.332x |
| `*family* read.point` | 1.504x |
| `*family* read.range` | 0.907x |
| `*family* read.analytical` | 0.308x |
| `*family* read.join` | 0.636x |
| `*family* write` | 0.760x |
| `*family* transaction` | 0.765x |
| `*family* schema` | 1.443x |
| `*family* extension` | 0.568x |
| `*family* large.values` | 1.826x |
| `prepare.trivial` | 0.087x |
| `prepare.point` | 1.252x |
| `point.rowid` | 1.977x |
| `point.index` | 1.180x |
| `point.miss` | 1.441x |
| `range.covering` | 0.540x |
| `range.lookaside` | 0.894x |
| `range.reverse` | 1.529x |
| `scan.aggregate` | 0.402x |
| `scan.group` | 0.371x |
| `scan.sort` | 9.611x |
| `scan.distinct` | 0.006x |
| `join.selective` | 1.106x |
| `join.range` | 0.365x |
| `correlated.exists` | 0.074x |
| `correlated.in` | 0.060x |
| `correlated.exists.selective` | 0.308x |
| `correlated.scalar.selective` | 0.356x |
| `write.insert.batch` | 1.072x |
| `write.insert.autocommit` | 3.202x |
| `write.update.indexed` | 0.157x |
| `write.delete` | 0.519x |
| `write.upsert` | 1.104x |
| `txn.autocommit` | 0.870x |
| `txn.batched` | 3.205x |
| `txn.large` | 0.162x |
| `schema.index` | 1.442x |
| `extension.json` | 0.059x |
| `extension.fts.build` | 1.058x |
| `extension.fts.query` | 0.496x |
| `extension.rtree.insert` | 2.002x |
| `extension.rtree.query` | 0.918x |
| `large.read` | 1.211x |
| `large.write` | 2.976x |

## Regressions

None open. A regression opens when a workload's lower confidence bound sits more than five percent below the best it had reached, for two consecutive comparable runs - one run is noise.

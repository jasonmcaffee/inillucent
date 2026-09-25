# inillucent performance dashboard

Platform `windows-x86_64`. Every number is the paired speed ratio, SQLite over inillucent, so above one is faster than the reference. Columns are runs in the order they were taken.

## Scale `medium`

| workload | nightly-20260925-072550 | nightly-20260925-082330 |
|---|---:|---:|
| `*headline*` | 0.759x | 0.751x |
| `*family* open.prepare` | 0.332x | 0.330x |
| `*family* read.point` | 1.504x | 1.478x |
| `*family* read.range` | 0.907x | 0.903x |
| `*family* read.analytical` | 0.308x | 0.308x |
| `*family* read.join` | 0.636x | 0.636x |
| `*family* write` | 0.760x | 0.734x |
| `*family* transaction` | 0.765x | 0.739x |
| `*family* schema` | 1.443x | 1.572x |
| `*family* extension` | 0.568x | 0.546x |
| `*family* large.values` | 1.826x | 1.919x |
| `prepare.trivial` | 0.087x | 0.087x |
| `prepare.point` | 1.252x | 1.253x |
| `point.rowid` | 1.977x | 1.947x |
| `point.index` | 1.180x | 1.173x |
| `point.miss` | 1.441x | 1.426x |
| `range.covering` | 0.540x | 0.531x |
| `range.lookaside` | 0.894x | 0.894x |
| `range.reverse` | 1.529x | 1.553x |
| `scan.aggregate` | 0.402x | 0.404x |
| `scan.group` | 0.371x | 0.367x |
| `scan.sort` | 9.611x | 9.681x |
| `scan.distinct` | 0.006x | 0.006x |
| `join.selective` | 1.106x | 1.103x |
| `join.range` | 0.365x | 0.363x |
| `correlated.exists` | 0.074x | 0.074x |
| `correlated.in` | 0.060x | 0.051x |
| `correlated.exists.selective` | 0.308x | 0.302x |
| `correlated.scalar.selective` | 0.356x | 0.360x |
| `write.insert.batch` | 1.072x | 1.065x |
| `write.insert.autocommit` | 3.202x | 3.160x |
| `write.update.indexed` | 0.157x | 0.153x |
| `write.delete` | 0.519x | 0.514x |
| `write.upsert` | 1.104x | 1.082x |
| `txn.autocommit` | 0.870x | 0.857x |
| `txn.batched` | 3.205x | 3.177x |
| `txn.large` | 0.162x | 0.160x |
| `schema.index` | 1.442x | 1.473x |
| `extension.json` | 0.059x | 0.059x |
| `extension.fts.build` | 1.058x | 1.038x |
| `extension.fts.query` | 0.496x | 0.493x |
| `extension.rtree.insert` | 2.002x | 1.948x |
| `extension.rtree.query` | 0.918x | 0.904x |
| `large.read` | 1.211x | 1.195x |
| `large.write` | 2.976x | 3.243x |

## Regressions

None open. A regression opens when a workload's lower confidence bound sits more than five percent below the best it had reached, for two consecutive comparable runs - one run is noise.

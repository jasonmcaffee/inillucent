# inillucent performance dashboard

Platform `windows-x86_64`. Every number is the paired speed ratio, SQLite over inillucent, so above one is faster than the reference. Columns are runs in the order they were taken.

## Scale `medium`

| workload | nightly-20260925-072550 | nightly-20260925-082330 | nightly-20260925-174312 |
|---|---:|---:|---:|
| `*headline*` | 0.759x | 0.751x | 0.754x |
| `*family* open.prepare` | 0.332x | 0.330x | 0.330x |
| `*family* read.point` | 1.504x | 1.478x | 1.481x |
| `*family* read.range` | 0.907x | 0.903x | 0.917x |
| `*family* read.analytical` | 0.308x | 0.308x | 0.308x |
| `*family* read.join` | 0.636x | 0.636x | 0.635x |
| `*family* write` | 0.760x | 0.734x | 0.747x |
| `*family* transaction` | 0.765x | 0.739x | 0.774x |
| `*family* schema` | 1.443x | 1.572x | 1.425x |
| `*family* extension` | 0.568x | 0.546x | 0.532x |
| `*family* large.values` | 1.826x | 1.919x | 1.941x |
| `prepare.trivial` | 0.087x | 0.087x | 0.087x |
| `prepare.point` | 1.252x | 1.253x | 1.250x |
| `point.rowid` | 1.977x | 1.947x | 1.909x |
| `point.index` | 1.180x | 1.173x | 1.181x |
| `point.miss` | 1.441x | 1.426x | 1.441x |
| `range.covering` | 0.540x | 0.531x | 0.545x |
| `range.lookaside` | 0.894x | 0.894x | 0.909x |
| `range.reverse` | 1.529x | 1.553x | 1.561x |
| `scan.aggregate` | 0.402x | 0.404x | 0.401x |
| `scan.group` | 0.371x | 0.367x | 0.366x |
| `scan.sort` | 9.611x | 9.681x | 9.658x |
| `scan.distinct` | 0.006x | 0.006x | 0.006x |
| `join.selective` | 1.106x | 1.103x | 1.107x |
| `join.range` | 0.365x | 0.363x | 0.365x |
| `correlated.exists` | 0.074x | 0.074x | 0.074x |
| `correlated.in` | 0.060x | 0.051x | 0.062x |
| `correlated.exists.selective` | 0.308x | 0.302x | 0.322x |
| `correlated.scalar.selective` | 0.356x | 0.360x | 0.374x |
| `write.insert.batch` | 1.072x | 1.065x | 1.073x |
| `write.insert.autocommit` | 3.202x | 3.160x | 3.190x |
| `write.update.indexed` | 0.157x | 0.153x | 0.158x |
| `write.delete` | 0.519x | 0.514x | 0.520x |
| `write.upsert` | 1.104x | 1.082x | 1.067x |
| `txn.autocommit` | 0.870x | 0.857x | 0.868x |
| `txn.batched` | 3.205x | 3.177x | 3.227x |
| `txn.large` | 0.162x | 0.160x | 0.166x |
| `schema.index` | 1.442x | 1.473x | 1.428x |
| `extension.json` | 0.059x | 0.059x | 0.059x |
| `extension.fts.build` | 1.058x | 1.038x | 1.047x |
| `extension.fts.query` | 0.496x | 0.493x | 0.498x |
| `extension.rtree.insert` | 2.002x | 1.948x | 1.923x |
| `extension.rtree.query` | 0.918x | 0.904x | 0.909x |
| `large.read` | 1.211x | 1.195x | 1.209x |
| `large.write` | 2.976x | 3.243x | 3.296x |

## Regressions

| scale | workload | arm | best lower bound | now | runs |
|---|---|---|---:|---:|---|
| `medium` | `extension.rtree.insert` | shipped | 1.833x | 1.360x | nightly-20260925-082330, nightly-20260925-174312 |
| `medium` | `write.insert.autocommit` | shipped | 3.124x | 2.879x | nightly-20260925-082330, nightly-20260925-174312 |

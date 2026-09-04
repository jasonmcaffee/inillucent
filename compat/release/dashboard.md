# rust-db performance dashboard

Platform `windows-x86_64`. Every number is the paired speed ratio, SQLite over rust-db, so above one is faster than the reference. Columns are runs in the order they were taken.

## Scale `small`

| workload | release-candidate (no covering-index) | release-candidate (no indexed-write) | release-candidate |
|---|---:|---:|---:|
| `*headline*` | 0.170x | 0.138x | 0.205x |
| `*family* open.prepare` | 0.192x | 0.198x | 0.198x |
| `*family* read.point` | 0.820x | 0.873x | 0.833x |
| `*family* read.range` | 0.035x | 0.061x | 0.062x |
| `*family* read.analytical` | 0.024x | 0.054x | 0.052x |
| `*family* read.join` | 0.192x | 0.287x | 0.288x |
| `*family* write` | 0.219x | 0.065x | 0.218x |
| `*family* transaction` | 0.341x | 0.099x | 0.348x |
| `*family* schema` | 0.103x | 0.082x | 0.091x |
| `*family* extension` | 0.095x | 0.100x | 0.096x |
| `*family* large.values` | 0.773x | 0.499x | 0.820x |
| `prepare.trivial` | 0.043x | 0.044x | 0.046x |
| `prepare.point` | 0.813x | 0.907x | 0.895x |
| `point.rowid` | 1.554x | 1.670x | 1.602x |
| `point.index` | 0.788x | 0.888x | 0.853x |
| `point.miss` | 0.367x | 0.394x | 0.367x |
| `range.covering` | 0.031x | 0.161x | 0.158x |
| `range.lookaside` | 0.078x | 0.074x | 0.074x |
| `range.reverse` | 0.014x | 0.014x | 0.016x |
| `scan.aggregate` | 0.039x | 0.076x | 0.080x |
| `scan.group` | 0.031x | 0.083x | 0.078x |
| `scan.sort` | 0.049x | 0.066x | 0.064x |
| `scan.distinct` | 0.005x | 0.016x | 0.017x |
| `join.selective` | 0.787x | 0.921x | 0.975x |
| `join.range` | 0.043x | 0.077x | 0.078x |
| `write.insert.batch` | 0.097x | 0.096x | 0.098x |
| `write.insert.autocommit` | 0.995x | 0.947x | 0.971x |
| `write.update.indexed` | 0.094x | 0.005x | 0.097x |
| `write.delete` | 0.134x | 0.005x | 0.132x |
| `write.upsert` | 0.412x | 0.410x | 0.407x |
| `txn.autocommit` | 0.982x | 0.708x | 1.004x |
| `txn.batched` | 0.842x | 0.467x | 0.839x |
| `txn.large` | 0.049x | 0.003x | 0.048x |
| `schema.index` | 0.096x | 0.085x | 0.091x |
| `extension.json` | 0.040x | 0.044x | 0.040x |
| `extension.fts.build` | 0.020x | 0.021x | 0.019x |
| `extension.fts.query` | 0.050x | 0.052x | 0.048x |
| `extension.rtree.insert` | 0.239x | 0.245x | 0.242x |
| `extension.rtree.query` | 0.846x | 0.860x | 0.854x |
| `large.read` | 0.809x | 0.833x | 0.893x |
| `large.write` | 0.698x | 0.292x | 0.741x |

## Scale `medium`

| workload | release-candidate (no covering-index) | release-candidate (no indexed-write) | release-candidate |
|---|---:|---:|---:|
| `*headline*` | - | - | 0.137x |
| `*family* open.prepare` | - | - | 0.181x |
| `*family* read.point` | - | - | 0.664x |
| `*family* read.range` | - | - | 0.025x |
| `*family* read.analytical` | - | - | 0.029x |
| `*family* read.join` | - | - | 0.177x |
| `*family* write` | - | - | 0.165x |
| `*family* transaction` | - | - | 0.352x |
| `*family* schema` | - | - | 0.010x |
| `*family* extension` | - | - | 0.101x |
| `*family* large.values` | - | - | 0.832x |
| `prepare.trivial` | - | - | 0.044x |
| `prepare.point` | - | - | 0.775x |
| `point.rowid` | - | - | 1.320x |
| `point.index` | - | - | 0.723x |
| `point.miss` | - | - | 0.289x |
| `range.covering` | - | - | 0.149x |
| `range.lookaside` | - | - | 0.078x |
| `range.reverse` | - | - | 0.001x |
| `scan.aggregate` | - | - | 0.069x |
| `scan.group` | - | - | 0.063x |
| `scan.sort` | - | - | 0.124x |
| `scan.distinct` | - | - | 0.001x |
| `join.selective` | - | - | 0.668x |
| `join.range` | - | - | 0.042x |
| `write.insert.batch` | - | - | 0.146x |
| `write.insert.autocommit` | - | - | 0.953x |
| `write.update.indexed` | - | - | 0.053x |
| `write.delete` | - | - | 0.043x |
| `write.upsert` | - | - | 0.460x |
| `txn.autocommit` | - | - | 1.154x |
| `txn.batched` | - | - | 0.836x |
| `txn.large` | - | - | 0.043x |
| `schema.index` | - | - | 0.010x |
| `extension.json` | - | - | 0.041x |
| `extension.fts.build` | - | - | 0.021x |
| `extension.fts.query` | - | - | 0.049x |
| `extension.rtree.insert` | - | - | 0.259x |
| `extension.rtree.query` | - | - | 0.899x |
| `large.read` | - | - | 0.816x |
| `large.write` | - | - | 0.762x |

## Scale `large`

| workload | release-candidate (no covering-index) | release-candidate (no indexed-write) | release-candidate |
|---|---:|---:|---:|
| `*headline*` | - | - | 0.143x |
| `*family* open.prepare` | - | - | 0.180x |
| `*family* read.point` | - | - | 0.797x |
| `*family* read.range` | - | - | 0.015x |
| `*family* read.analytical` | - | - | 0.031x |
| `*family* read.join` | - | - | 0.178x |
| `*family* write` | - | - | 0.281x |
| `*family* transaction` | - | - | 0.281x |
| `*family* schema` | - | - | 0.002x |
| `*family* extension` | - | - | 0.161x |
| `*family* large.values` | - | - | 1.103x |
| `prepare.trivial` | - | - | 0.037x |
| `prepare.point` | - | - | 0.835x |
| `point.rowid` | - | - | 1.194x |
| `point.index` | - | - | 0.777x |
| `point.miss` | - | - | 0.479x |
| `range.covering` | - | - | 0.228x |
| `range.lookaside` | - | - | 0.067x |
| `range.reverse` | - | - | 0.000x |
| `scan.aggregate` | - | - | 0.105x |
| `scan.group` | - | - | 0.095x |
| `scan.sort` | - | - | 0.126x |
| `scan.distinct` | - | - | 0.001x |
| `join.selective` | - | - | 0.703x |
| `join.range` | - | - | 0.040x |
| `write.insert.batch` | - | - | 0.389x |
| `write.insert.autocommit` | - | - | 0.989x |
| `write.update.indexed` | - | - | 0.090x |
| `write.delete` | - | - | 0.118x |
| `write.upsert` | - | - | 0.532x |
| `txn.autocommit` | - | - | 1.140x |
| `txn.batched` | - | - | 0.832x |
| `txn.large` | - | - | 0.023x |
| `schema.index` | - | - | 0.002x |
| `extension.json` | - | - | 0.036x |
| `extension.fts.build` | - | - | 0.055x |
| `extension.fts.query` | - | - | 0.116x |
| `extension.rtree.insert` | - | - | 0.385x |
| `extension.rtree.query` | - | - | 1.111x |
| `large.read` | - | - | 1.418x |
| `large.write` | - | - | 0.927x |

## Regressions

None open. A regression opens when a workload's lower confidence bound sits more than five percent below the best it had reached, for two consecutive comparable runs - one run is noise.

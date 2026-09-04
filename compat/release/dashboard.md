# rust-db performance dashboard

Platform `windows-x86_64`. Every number is the paired speed ratio, SQLite over rust-db, so above one is faster than the reference. Columns are runs in the order they were taken.

## Scale `small`

| workload | release-candidate (no covering-index) | release-candidate (no indexed-write) | release-candidate (no ordered-walk) | release-candidate (no streaming-group) | release-candidate (no fused-bytecode) | release-candidate |
|---|---:|---:|---:|---:|---:|---:|
| `*headline*` | 0.208x | 0.166x | 0.213x | 0.239x | 0.244x | 0.256x |
| `*family* open.prepare` | 0.210x | 0.198x | 0.211x | 0.208x | 0.216x | 0.199x |
| `*family* read.point` | 0.789x | 0.834x | 0.874x | 0.781x | 0.826x | 0.894x |
| `*family* read.range` | 0.128x | 0.221x | 0.065x | 0.213x | 0.216x | 0.236x |
| `*family* read.analytical` | 0.036x | 0.066x | 0.064x | 0.058x | 0.060x | 0.068x |
| `*family* read.join` | 0.186x | 0.272x | 0.281x | 0.270x | 0.275x | 0.304x |
| `*family* write` | 0.208x | 0.061x | 0.210x | 0.211x | 0.219x | 0.212x |
| `*family* transaction` | 0.304x | 0.092x | 0.302x | 0.304x | 0.298x | 0.300x |
| `*family* schema` | 0.210x | 0.198x | 0.216x | 0.205x | 0.205x | 0.204x |
| `*family* extension` | 0.103x | 0.095x | 0.100x | 0.100x | 0.097x | 0.102x |
| `*family* large.values` | 0.771x | 0.481x | 0.777x | 0.781x | 0.781x | 0.853x |
| `prepare.trivial` | 0.049x | 0.049x | 0.050x | 0.051x | 0.053x | 0.042x |
| `prepare.point` | 0.929x | 0.804x | 0.939x | 0.835x | 0.882x | 0.911x |
| `point.rowid` | 1.557x | 1.559x | 1.744x | 1.532x | 1.541x | 1.723x |
| `point.index` | 0.771x | 0.855x | 0.873x | 0.780x | 0.820x | 0.953x |
| `point.miss` | 0.376x | 0.389x | 0.404x | 0.372x | 0.377x | 0.426x |
| `range.covering` | 0.032x | 0.152x | 0.163x | 0.151x | 0.144x | 0.197x |
| `range.lookaside` | 0.077x | 0.083x | 0.080x | 0.077x | 0.080x | 0.081x |
| `range.reverse` | 0.750x | 0.785x | 0.016x | 0.779x | 0.754x | 0.765x |
| `scan.aggregate` | 0.060x | 0.086x | 0.084x | 0.083x | 0.076x | 0.088x |
| `scan.group` | 0.046x | 0.111x | 0.111x | 0.081x | 0.097x | 0.113x |
| `scan.sort` | 0.070x | 0.093x | 0.077x | 0.091x | 0.083x | 0.098x |
| `scan.distinct` | 0.008x | 0.019x | 0.024x | 0.017x | 0.017x | 0.021x |
| `join.selective` | 0.781x | 0.914x | 0.916x | 0.920x | 0.927x | 1.172x |
| `join.range` | 0.039x | 0.074x | 0.079x | 0.075x | 0.073x | 0.085x |
| `write.insert.batch` | 0.069x | 0.066x | 0.070x | 0.076x | 0.070x | 0.072x |
| `write.insert.autocommit` | 0.995x | 0.990x | 0.990x | 0.999x | 1.038x | 1.014x |
| `write.update.indexed` | 0.096x | 0.006x | 0.095x | 0.099x | 0.098x | 0.095x |
| `write.delete` | 0.134x | 0.005x | 0.131x | 0.139x | 0.134x | 0.140x |
| `write.upsert` | 0.420x | 0.400x | 0.471x | 0.419x | 0.429x | 0.432x |
| `txn.autocommit` | 0.906x | 0.649x | 0.909x | 0.932x | 0.899x | 0.993x |
| `txn.batched` | 0.824x | 0.445x | 0.826x | 0.852x | 0.829x | 0.823x |
| `txn.large` | 0.036x | 0.003x | 0.036x | 0.036x | 0.035x | 0.036x |
| `schema.index` | 0.208x | 0.194x | 0.217x | 0.204x | 0.204x | 0.210x |
| `extension.json` | 0.044x | 0.043x | 0.040x | 0.043x | 0.045x | 0.037x |
| `extension.fts.build` | 0.020x | 0.019x | 0.018x | 0.020x | 0.019x | 0.021x |
| `extension.fts.query` | 0.051x | 0.049x | 0.052x | 0.050x | 0.048x | 0.057x |
| `extension.rtree.insert` | 0.245x | 0.233x | 0.243x | 0.264x | 0.248x | 0.246x |
| `extension.rtree.query` | 0.891x | 0.819x | 0.941x | 0.846x | 0.827x | 0.979x |
| `large.read` | 1.005x | 0.835x | 0.896x | 0.831x | 0.825x | 0.936x |
| `large.write` | 0.634x | 0.302x | 0.774x | 0.672x | 0.711x | 0.703x |

## Scale `medium`

| workload | release-candidate (no covering-index) | release-candidate (no indexed-write) | release-candidate (no ordered-walk) | release-candidate (no streaming-group) | release-candidate (no fused-bytecode) | release-candidate |
|---|---:|---:|---:|---:|---:|---:|
| `*headline*` | - | - | - | - | - | 0.198x |
| `*family* open.prepare` | - | - | - | - | - | 0.187x |
| `*family* read.point` | - | - | - | - | - | 0.661x |
| `*family* read.range` | - | - | - | - | - | 0.230x |
| `*family* read.analytical` | - | - | - | - | - | 0.036x |
| `*family* read.join` | - | - | - | - | - | 0.180x |
| `*family* write` | - | - | - | - | - | 0.172x |
| `*family* transaction` | - | - | - | - | - | 0.318x |
| `*family* schema` | - | - | - | - | - | 0.073x |
| `*family* extension` | - | - | - | - | - | 0.107x |
| `*family* large.values` | - | - | - | - | - | 0.826x |
| `prepare.trivial` | - | - | - | - | - | 0.046x |
| `prepare.point` | - | - | - | - | - | 0.776x |
| `point.rowid` | - | - | - | - | - | 1.095x |
| `point.index` | - | - | - | - | - | 0.894x |
| `point.miss` | - | - | - | - | - | 0.273x |
| `range.covering` | - | - | - | - | - | 0.160x |
| `range.lookaside` | - | - | - | - | - | 0.079x |
| `range.reverse` | - | - | - | - | - | 0.799x |
| `scan.aggregate` | - | - | - | - | - | 0.078x |
| `scan.group` | - | - | - | - | - | 0.094x |
| `scan.sort` | - | - | - | - | - | 0.143x |
| `scan.distinct` | - | - | - | - | - | 0.001x |
| `join.selective` | - | - | - | - | - | 0.650x |
| `join.range` | - | - | - | - | - | 0.044x |
| `write.insert.batch` | - | - | - | - | - | 0.149x |
| `write.insert.autocommit` | - | - | - | - | - | 0.982x |
| `write.update.indexed` | - | - | - | - | - | 0.055x |
| `write.delete` | - | - | - | - | - | 0.050x |
| `write.upsert` | - | - | - | - | - | 0.475x |
| `txn.autocommit` | - | - | - | - | - | 0.879x |
| `txn.batched` | - | - | - | - | - | 0.850x |
| `txn.large` | - | - | - | - | - | 0.041x |
| `schema.index` | - | - | - | - | - | 0.074x |
| `extension.json` | - | - | - | - | - | 0.040x |
| `extension.fts.build` | - | - | - | - | - | 0.021x |
| `extension.fts.query` | - | - | - | - | - | 0.068x |
| `extension.rtree.insert` | - | - | - | - | - | 0.254x |
| `extension.rtree.query` | - | - | - | - | - | 0.985x |
| `large.read` | - | - | - | - | - | 0.999x |
| `large.write` | - | - | - | - | - | 0.712x |

## Scale `large`

| workload | release-candidate (no covering-index) | release-candidate (no indexed-write) | release-candidate (no ordered-walk) | release-candidate (no streaming-group) | release-candidate (no fused-bytecode) | release-candidate |
|---|---:|---:|---:|---:|---:|---:|
| `*headline*` | - | - | - | - | - | 0.219x |
| `*family* open.prepare` | - | - | - | - | - | 0.196x |
| `*family* read.point` | - | - | - | - | - | 0.812x |
| `*family* read.range` | - | - | - | - | - | 0.243x |
| `*family* read.analytical` | - | - | - | - | - | 0.040x |
| `*family* read.join` | - | - | - | - | - | 0.182x |
| `*family* write` | - | - | - | - | - | 0.247x |
| `*family* transaction` | - | - | - | - | - | 0.235x |
| `*family* schema` | - | - | - | - | - | 0.018x |
| `*family* extension` | - | - | - | - | - | 0.162x |
| `*family* large.values` | - | - | - | - | - | 1.026x |
| `prepare.trivial` | - | - | - | - | - | 0.043x |
| `prepare.point` | - | - | - | - | - | 0.854x |
| `point.rowid` | - | - | - | - | - | 1.263x |
| `point.index` | - | - | - | - | - | 0.917x |
| `point.miss` | - | - | - | - | - | 0.498x |
| `range.covering` | - | - | - | - | - | 0.223x |
| `range.lookaside` | - | - | - | - | - | 0.074x |
| `range.reverse` | - | - | - | - | - | 0.844x |
| `scan.aggregate` | - | - | - | - | - | 0.119x |
| `scan.group` | - | - | - | - | - | 0.169x |
| `scan.sort` | - | - | - | - | - | 0.149x |
| `scan.distinct` | - | - | - | - | - | 0.001x |
| `join.selective` | - | - | - | - | - | 0.692x |
| `join.range` | - | - | - | - | - | 0.048x |
| `write.insert.batch` | - | - | - | - | - | 0.221x |
| `write.insert.autocommit` | - | - | - | - | - | 0.969x |
| `write.update.indexed` | - | - | - | - | - | 0.088x |
| `write.delete` | - | - | - | - | - | 0.116x |
| `write.upsert` | - | - | - | - | - | 0.508x |
| `txn.autocommit` | - | - | - | - | - | 0.869x |
| `txn.batched` | - | - | - | - | - | 0.705x |
| `txn.large` | - | - | - | - | - | 0.023x |
| `schema.index` | - | - | - | - | - | 0.018x |
| `extension.json` | - | - | - | - | - | 0.037x |
| `extension.fts.build` | - | - | - | - | - | 0.051x |
| `extension.fts.query` | - | - | - | - | - | 0.123x |
| `extension.rtree.insert` | - | - | - | - | - | 0.445x |
| `extension.rtree.query` | - | - | - | - | - | 1.112x |
| `large.read` | - | - | - | - | - | 1.417x |
| `large.write` | - | - | - | - | - | 0.843x |

## Regressions

None open. A regression opens when a workload's lower confidence bound sits more than five percent below the best it had reached, for two consecutive comparable runs - one run is noise.

# rust-db performance dashboard

Platform `windows-x86_64`. Every number is the paired speed ratio, SQLite over rust-db, so above one is faster than the reference. Columns are runs in the order they were taken.

## Scale `small`

| workload | release-candidate (no covering-index) | release-candidate (no indexed-write) | release-candidate (no ordered-walk) | release-candidate |
|---|---:|---:|---:|---:|
| `*headline*` | 0.216x | 0.166x | 0.207x | 0.240x |
| `*family* open.prepare` | 0.212x | 0.221x | 0.216x | 0.223x |
| `*family* read.point` | 0.891x | 0.820x | 0.902x | 0.854x |
| `*family* read.range` | 0.144x | 0.210x | 0.063x | 0.212x |
| `*family* read.analytical` | 0.035x | 0.054x | 0.055x | 0.055x |
| `*family* read.join` | 0.208x | 0.276x | 0.295x | 0.280x |
| `*family* write` | 0.207x | 0.060x | 0.195x | 0.206x |
| `*family* transaction` | 0.297x | 0.092x | 0.287x | 0.306x |
| `*family* schema` | 0.207x | 0.201x | 0.190x | 0.211x |
| `*family* extension` | 0.098x | 0.103x | 0.096x | 0.098x |
| `*family* large.values` | 0.802x | 0.546x | 0.849x | 0.805x |
| `prepare.trivial` | 0.044x | 0.050x | 0.048x | 0.052x |
| `prepare.point` | 1.073x | 1.000x | 1.074x | 0.993x |
| `point.rowid` | 1.546x | 1.584x | 1.788x | 1.591x |
| `point.index` | 0.881x | 0.807x | 0.879x | 0.893x |
| `point.miss` | 0.424x | 0.375x | 0.421x | 0.373x |
| `range.covering` | 0.038x | 0.144x | 0.183x | 0.143x |
| `range.lookaside` | 0.085x | 0.078x | 0.073x | 0.075x |
| `range.reverse` | 0.834x | 0.727x | 0.016x | 0.759x |
| `scan.aggregate` | 0.055x | 0.074x | 0.079x | 0.073x |
| `scan.group` | 0.044x | 0.077x | 0.083x | 0.080x |
| `scan.sort` | 0.070x | 0.085x | 0.075x | 0.086x |
| `scan.distinct` | 0.009x | 0.016x | 0.018x | 0.016x |
| `join.selective` | 0.903x | 0.955x | 0.959x | 0.949x |
| `join.range` | 0.048x | 0.073x | 0.080x | 0.076x |
| `write.insert.batch` | 0.079x | 0.067x | 0.071x | 0.061x |
| `write.insert.autocommit` | 0.997x | 0.976x | 0.977x | 0.982x |
| `write.update.indexed` | 0.095x | 0.006x | 0.087x | 0.101x |
| `write.delete` | 0.135x | 0.006x | 0.112x | 0.134x |
| `write.upsert` | 0.412x | 0.390x | 0.432x | 0.433x |
| `txn.autocommit` | 0.911x | 0.657x | 0.882x | 0.934x |
| `txn.batched` | 0.791x | 0.451x | 0.800x | 0.798x |
| `txn.large` | 0.035x | 0.003x | 0.036x | 0.035x |
| `schema.index` | 0.211x | 0.203x | 0.199x | 0.214x |
| `extension.json` | 0.039x | 0.040x | 0.037x | 0.038x |
| `extension.fts.build` | 0.019x | 0.019x | 0.018x | 0.020x |
| `extension.fts.query` | 0.058x | 0.067x | 0.051x | 0.051x |
| `extension.rtree.insert` | 0.249x | 0.246x | 0.249x | 0.249x |
| `extension.rtree.query` | 0.989x | 0.975x | 0.878x | 0.892x |
| `large.read` | 0.950x | 1.086x | 0.860x | 0.830x |
| `large.write` | 0.724x | 0.289x | 0.711x | 0.699x |

## Scale `medium`

| workload | release-candidate (no covering-index) | release-candidate (no indexed-write) | release-candidate (no ordered-walk) | release-candidate |
|---|---:|---:|---:|---:|
| `*headline*` | - | - | - | 0.192x |
| `*family* open.prepare` | - | - | - | 0.186x |
| `*family* read.point` | - | - | - | 0.646x |
| `*family* read.range` | - | - | - | 0.217x |
| `*family* read.analytical` | - | - | - | 0.029x |
| `*family* read.join` | - | - | - | 0.163x |
| `*family* write` | - | - | - | 0.171x |
| `*family* transaction` | - | - | - | 0.313x |
| `*family* schema` | - | - | - | 0.073x |
| `*family* extension` | - | - | - | 0.099x |
| `*family* large.values` | - | - | - | 0.760x |
| `prepare.trivial` | - | - | - | 0.047x |
| `prepare.point` | - | - | - | 0.704x |
| `point.rowid` | - | - | - | 1.125x |
| `point.index` | - | - | - | 0.805x |
| `point.miss` | - | - | - | 0.288x |
| `range.covering` | - | - | - | 0.144x |
| `range.lookaside` | - | - | - | 0.073x |
| `range.reverse` | - | - | - | 0.751x |
| `scan.aggregate` | - | - | - | 0.070x |
| `scan.group` | - | - | - | 0.064x |
| `scan.sort` | - | - | - | 0.121x |
| `scan.distinct` | - | - | - | 0.001x |
| `join.selective` | - | - | - | 0.618x |
| `join.range` | - | - | - | 0.040x |
| `write.insert.batch` | - | - | - | 0.150x |
| `write.insert.autocommit` | - | - | - | 0.998x |
| `write.update.indexed` | - | - | - | 0.051x |
| `write.delete` | - | - | - | 0.047x |
| `write.upsert` | - | - | - | 0.478x |
| `txn.autocommit` | - | - | - | 0.925x |
| `txn.batched` | - | - | - | 0.821x |
| `txn.large` | - | - | - | 0.041x |
| `schema.index` | - | - | - | 0.072x |
| `extension.json` | - | - | - | 0.041x |
| `extension.fts.build` | - | - | - | 0.021x |
| `extension.fts.query` | - | - | - | 0.052x |
| `extension.rtree.insert` | - | - | - | 0.242x |
| `extension.rtree.query` | - | - | - | 0.896x |
| `large.read` | - | - | - | 0.866x |
| `large.write` | - | - | - | 0.689x |

## Scale `large`

| workload | release-candidate (no covering-index) | release-candidate (no indexed-write) | release-candidate (no ordered-walk) | release-candidate |
|---|---:|---:|---:|---:|
| `*headline*` | - | - | - | 0.209x |
| `*family* open.prepare` | - | - | - | 0.195x |
| `*family* read.point` | - | - | - | 0.694x |
| `*family* read.range` | - | - | - | 0.196x |
| `*family* read.analytical` | - | - | - | 0.031x |
| `*family* read.join` | - | - | - | 0.180x |
| `*family* write` | - | - | - | 0.263x |
| `*family* transaction` | - | - | - | 0.249x |
| `*family* schema` | - | - | - | 0.018x |
| `*family* extension` | - | - | - | 0.159x |
| `*family* large.values` | - | - | - | 1.013x |
| `prepare.trivial` | - | - | - | 0.050x |
| `prepare.point` | - | - | - | 0.749x |
| `point.rowid` | - | - | - | 1.094x |
| `point.index` | - | - | - | 0.708x |
| `point.miss` | - | - | - | 0.413x |
| `range.covering` | - | - | - | 0.154x |
| `range.lookaside` | - | - | - | 0.061x |
| `range.reverse` | - | - | - | 0.747x |
| `scan.aggregate` | - | - | - | 0.097x |
| `scan.group` | - | - | - | 0.095x |
| `scan.sort` | - | - | - | 0.123x |
| `scan.distinct` | - | - | - | 0.001x |
| `join.selective` | - | - | - | 0.756x |
| `join.range` | - | - | - | 0.039x |
| `write.insert.batch` | - | - | - | 0.246x |
| `write.insert.autocommit` | - | - | - | 1.016x |
| `write.update.indexed` | - | - | - | 0.084x |
| `write.delete` | - | - | - | 0.119x |
| `write.upsert` | - | - | - | 0.531x |
| `txn.autocommit` | - | - | - | 0.893x |
| `txn.batched` | - | - | - | 0.755x |
| `txn.large` | - | - | - | 0.023x |
| `schema.index` | - | - | - | 0.018x |
| `extension.json` | - | - | - | 0.045x |
| `extension.fts.build` | - | - | - | 0.050x |
| `extension.fts.query` | - | - | - | 0.088x |
| `extension.rtree.insert` | - | - | - | 0.380x |
| `extension.rtree.query` | - | - | - | 1.151x |
| `large.read` | - | - | - | 1.243x |
| `large.write` | - | - | - | 0.770x |

## Regressions

None open. A regression opens when a workload's lower confidence bound sits more than five percent below the best it had reached, for two consecutive comparable runs - one run is noise.

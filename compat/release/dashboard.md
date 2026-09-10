# inillucent performance dashboard

> **These are the `release` gate's own output files, taken on the engine that came
> before the rearchitecture, and they are kept as that run's record. They are not the current
> numbers and they are not edited by hand.** The engine measured here was slower than SQLite on
> every family; the shipping engine is **326% faster** weighted over the same ten families, with
> no family below the contract's 1.00x floor. The current run is
> [docs/performance.md](../../docs/performance.md) and
> [docs/feature-comparison.md](../../docs/feature-comparison.md).

Platform `windows-x86_64`. Every number is the paired speed ratio, SQLite over inillucent, so above one is faster than the reference. Columns are runs in the order they were taken.

## Scale `small`

| workload | release |
|---|---:|
| `*headline*` | 0.316x |
| `*family* open.prepare` | 0.214x |
| `*family* read.point` | 0.871x |
| `*family* read.range` | 0.303x |
| `*family* read.analytical` | 0.120x |
| `*family* read.join` | 0.326x |
| `*family* write` | 0.270x |
| `*family* transaction` | 0.477x |
| `*family* schema` | 0.260x |
| `*family* extension` | 0.130x |
| `*family* large.values` | 0.855x |
| `prepare.trivial` | 0.053x |
| `prepare.point` | 0.844x |
| `point.rowid` | 1.636x |
| `point.index` | 1.040x |
| `point.miss` | 0.378x |
| `range.covering` | 0.240x |
| `range.lookaside` | 0.103x |
| `range.reverse` | 1.101x |
| `scan.aggregate` | 0.168x |
| `scan.group` | 0.205x |
| `scan.sort` | 0.135x |
| `scan.distinct` | 0.045x |
| `join.selective` | 1.126x |
| `join.range` | 0.092x |
| `write.insert.batch` | 0.101x |
| `write.insert.autocommit` | 1.011x |
| `write.update.indexed` | 0.147x |
| `write.delete` | 0.193x |
| `write.upsert` | 0.447x |
| `txn.autocommit` | 0.982x |
| `txn.batched` | 0.899x |
| `txn.large` | 0.118x |
| `schema.index` | 0.252x |
| `extension.json` | 0.046x |
| `extension.fts.build` | 0.060x |
| `extension.fts.query` | 0.051x |
| `extension.rtree.insert` | 0.270x |
| `extension.rtree.query` | 0.874x |
| `large.read` | 0.861x |
| `large.write` | 0.781x |

## Scale `medium`

| workload | release |
|---|---:|
| `*headline*` | 0.232x |
| `*family* open.prepare` | 0.209x |
| `*family* read.point` | 0.636x |
| `*family* read.range` | 0.271x |
| `*family* read.analytical` | 0.063x |
| `*family* read.join` | 0.216x |
| `*family* write` | 0.192x |
| `*family* transaction` | 0.399x |
| `*family* schema` | 0.071x |
| `*family* extension` | 0.128x |
| `*family* large.values` | 0.759x |
| `prepare.trivial` | 0.062x |
| `prepare.point` | 0.725x |
| `point.rowid` | 1.094x |
| `point.index` | 0.899x |
| `point.miss` | 0.260x |
| `range.covering` | 0.227x |
| `range.lookaside` | 0.088x |
| `range.reverse` | 0.971x |
| `scan.aggregate` | 0.150x |
| `scan.group` | 0.181x |
| `scan.sort` | 0.177x |
| `scan.distinct` | 0.003x |
| `join.selective` | 0.771x |
| `join.range` | 0.060x |
| `write.insert.batch` | 0.222x |
| `write.insert.autocommit` | 0.990x |
| `write.update.indexed` | 0.054x |
| `write.delete` | 0.046x |
| `write.upsert` | 0.559x |
| `txn.autocommit` | 0.917x |
| `txn.batched` | 0.884x |
| `txn.large` | 0.075x |
| `schema.index` | 0.071x |
| `extension.json` | 0.047x |
| `extension.fts.build` | 0.061x |
| `extension.fts.query` | 0.050x |
| `extension.rtree.insert` | 0.283x |
| `extension.rtree.query` | 0.871x |
| `large.read` | 0.864x |
| `large.write` | 0.651x |

## Scale `large`

| workload | release |
|---|---:|
| `*headline*` | 0.260x |
| `*family* open.prepare` | 0.203x |
| `*family* read.point` | 0.761x |
| `*family* read.range` | 0.267x |
| `*family* read.analytical` | 0.064x |
| `*family* read.join` | 0.201x |
| `*family* write` | 0.317x |
| `*family* transaction` | 0.354x |
| `*family* schema` | 0.019x |
| `*family* extension` | 0.203x |
| `*family* large.values` | 1.120x |
| `prepare.trivial` | 0.056x |
| `prepare.point` | 0.736x |
| `point.rowid` | 1.077x |
| `point.index` | 0.917x |
| `point.miss` | 0.431x |
| `range.covering` | 0.261x |
| `range.lookaside` | 0.076x |
| `range.reverse` | 0.983x |
| `scan.aggregate` | 0.210x |
| `scan.group` | 0.270x |
| `scan.sort` | 0.169x |
| `scan.distinct` | 0.002x |
| `join.selective` | 0.777x |
| `join.range` | 0.054x |
| `write.insert.batch` | 0.452x |
| `write.insert.autocommit` | 1.017x |
| `write.update.indexed` | 0.095x |
| `write.delete` | 0.127x |
| `write.upsert` | 0.585x |
| `txn.autocommit` | 0.926x |
| `txn.batched` | 0.919x |
| `txn.large` | 0.050x |
| `schema.index` | 0.019x |
| `extension.json` | 0.049x |
| `extension.fts.build` | 0.128x |
| `extension.fts.query` | 0.091x |
| `extension.rtree.insert` | 0.438x |
| `extension.rtree.query` | 1.016x |
| `large.read` | 1.291x |
| `large.write` | 0.926x |

## Regressions

None open. A regression opens when a workload's lower confidence bound sits more than five percent below the best it had reached, for two consecutive comparable runs - one run is noise.

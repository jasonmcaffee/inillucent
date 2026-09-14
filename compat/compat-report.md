# inillucent compatibility with SQLite sqlite-3.53.4

Generated from `compat/sqlite-3.53.4.toml`. Do not edit.

| status | capabilities |
|---|---|
| missing | 14 |
| partial | 14 |
| pass | 247 |
| **total** | **275** |

## By phase

| phase | pass | partial | missing | deviation |
|---|---|---|---|---|
| phase 0: contract, provenance, and harness foundation | 24 | 0 | 0 | 0 |
| phase 10: WAL and concurrent connection semantics | 10 | 2 | 0 | 0 |
| phase 11: full built-ins, PRAGMAs, virtual tables, FTS5, and R-Tree | 9 | 4 | 0 | 0 |
| phase 12: C ABI and CLI completion | 12 | 0 | 0 | 0 |
| phase 13: transactional inillucent search and legacy migration | 3 | 0 | 0 | 0 |
| phase 14: performance qualification and release | 3 | 0 | 0 | 0 |
| phase 15: optional surfaces this release does not implement | 0 | 0 | 7 | 0 |
| phase 1: VFS, binary primitives, and simulator | 46 | 0 | 0 | 0 |
| phase 2: values, affinities, collations, and records | 17 | 0 | 0 | 0 |
| phase 3: read-only header, pager, page cache, and B-tree | 19 | 0 | 0 | 0 |
| phase 4: B-tree mutation, allocation, and rollback pages | 16 | 0 | 1 | 0 |
| phase 5: lexer, parser, AST, and syntax parity | 10 | 0 | 0 | 0 |
| phase 6: catalog, binder, expression VM, and read-only SELECT | 14 | 6 | 2 | 0 |
| phase 7: single-database rollback transactions and DML | 26 | 0 | 4 | 0 |
| phase 8: complete SELECT, planner, schema, and SQL semantics | 31 | 2 | 0 | 0 |
| phase 9: foreign keys, ATTACH, and multi-database commit | 7 | 0 | 0 | 0 |

## Problems

| kind | capability | detail |
|---|---|---|
| unsupported-release-claim | `api.rust.bind-step-reset` | no passing result recorded on linux-x86_64 |
| unsupported-release-claim | `catalog.sqlite-schema` | no passing result recorded on linux-x86_64 |
| unsupported-release-claim | `ext.fts5.queries` | no passing result recorded on windows-x86_64, linux-x86_64 |
| unsupported-release-claim | `functions.date-time` | no passing result recorded on linux-x86_64 |
| unsupported-release-claim | `pragma.rearchitecture.fixed` | no passing result recorded on linux-x86_64 |
| unsupported-release-claim | `pragma.rearchitecture.honoured` | no passing result recorded on linux-x86_64 |
| unsupported-release-claim | `pragma.rearchitecture.silent` | no passing result recorded on linux-x86_64 |
| unsupported-release-claim | `sql.binder.name-resolution` | no passing result recorded on linux-x86_64 |
| unsupported-release-claim | `sql.expr.like-glob` | no passing result recorded on linux-x86_64 |
| unsupported-release-claim | `sql.expr.operators` | no passing result recorded on linux-x86_64 |
| unsupported-release-claim | `sql.functions.scalar-core` | no passing result recorded on linux-x86_64 |
| unsupported-release-claim | `sql.select.window` | no passing result recorded on linux-x86_64 |
| unsupported-release-claim | `wal.checkpoint.full` | no passing result recorded on linux-x86_64 |
| unsupported-release-claim | `wal.checkpoint.restart` | no passing result recorded on linux-x86_64 |

## Capabilities

| id | claimed | evidenced | platforms | tests |
|---|---|---|---|---|
| `harness.manifest.schema` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `harness.manifest.duplicate-ids` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `harness.manifest.missing-tests` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `harness.manifest.dead-source-links` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `harness.manifest.unsupported-release-claims` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `harness.report.reproducible` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `harness.report.shipped-manifest-is-sound` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `harness.reference.pinned-metadata` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `harness.reference.checksums-verify` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `harness.oracle.protocol` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `harness.oracle.value-tagging` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `harness.oracle.error-tagging` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `harness.dependency.no-engine` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `harness.dependency.layering` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `harness.evidence.results-model` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `harness.policy.unsafe-code` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `harness.policy.documentation` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `harness.policy.formatting` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `harness.policy.provenance` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `harness.baseline.retrieval-unchanged` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `errors.table.primary-codes` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `errors.table.extended-codes` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `errors.table.recovery-contract` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `limits.defaults` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `binary.bigendian.widths` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `binary.bigendian.bounds` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `binary.varint.codec` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `binary.varint.robustness` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `binary.checksum.wal` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `binary.checksum.crc32` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `binary.page.size-rules` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `binary.page.offsets` | pass | pass | linux-x86_64, windows-x86_64 | 5 |
| `binary.buffers.fallible` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `binary.determinism.rng` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `vfs.contract.open` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `vfs.contract.read` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `vfs.contract.write` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `vfs.contract.truncate` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `vfs.contract.sync` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `vfs.contract.readonly` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `vfs.contract.delete` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `vfs.contract.access` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `vfs.contract.fullpath` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `vfs.contract.identity` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `vfs.contract.temp-files` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `vfs.contract.randomness` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `vfs.contract.clock` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `vfs.contract.device-characteristics` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `vfs.lock.shared` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `vfs.lock.reserved` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `vfs.lock.pending` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `vfs.lock.exclusive` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `vfs.lock.protocol-errors` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `vfs.lock.release-on-close` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `vfs.lock.cross-process` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `vfs.paths.companion-files` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `vfs.shm.mapping` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `vfs.shm.locks` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `vfs.conformance.memory` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `vfs.conformance.os` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `sim.determinism.trace` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `sim.determinism.schedule` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `sim.crash.synced-writes-survive` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `sim.crash.unsynced-writes-may-be-lost` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `sim.crash.torn-sectors` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `sim.crash.artifacts` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `sim.failpoints.campaign` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `sim.failpoints.error-codes` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `sim.failpoints.short-write` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `sim.vfs.conformance` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `values.storage-classes` | pass | pass | linux-x86_64, windows-x86_64 | 6 |
| `values.affinity` | pass | pass | linux-x86_64, windows-x86_64 | 10 |
| `values.comparison` | pass | pass | linux-x86_64, windows-x86_64 | 9 |
| `values.collation.binary` | pass | pass | linux-x86_64, windows-x86_64 | 6 |
| `values.collation.nocase` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `values.collation.rtrim` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `values.cast` | pass | pass | linux-x86_64, windows-x86_64 | 10 |
| `values.numeric-parsing` | pass | pass | linux-x86_64, windows-x86_64 | 12 |
| `values.record-format` | pass | pass | linux-x86_64, windows-x86_64 | 9 |
| `values.index-key-order` | pass | pass | linux-x86_64, windows-x86_64 | 5 |
| `storage.header.codec` | pass | pass | linux-x86_64, windows-x86_64 | 5 |
| `storage.header.validation` | pass | pass | linux-x86_64, windows-x86_64 | 7 |
| `storage.pager.read-path` | pass | pass | linux-x86_64, windows-x86_64 | 9 |
| `storage.page-cache` | pass | pass | linux-x86_64, windows-x86_64 | 11 |
| `storage.btree.table-leaf` | pass | pass | linux-x86_64, windows-x86_64 | 6 |
| `storage.btree.table-interior` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `storage.btree.index-leaf` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `storage.btree.index-interior` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `storage.overflow-chains` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `storage.cursor.seek` | pass | pass | linux-x86_64, windows-x86_64 | 5 |
| `storage.btree.insert` | pass | pass | linux-x86_64, windows-x86_64 | 6 |
| `storage.btree.delete` | pass | pass | linux-x86_64, windows-x86_64 | 5 |
| `storage.btree.balance` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `storage.freelist` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `storage.pointer-maps` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `storage.auto-vacuum` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `storage.pager.write-transaction` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `storage.pager.statement-undo` | pass | pass | linux-x86_64, windows-x86_64 | 5 |
| `storage.pager.savepoints` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `storage.page.cell-edit` | pass | pass | linux-x86_64, windows-x86_64 | 6 |
| `storage.overflow.write` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `storage.cursor.restore` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `storage.vacuum.incremental` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `storage.vacuum.copy` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `storage.interop.cross-mutation` | missing | missing | - | 0 |
| `storage.failure.statement-atomicity` | pass | pass | linux-x86_64, windows-x86_64 | 7 |
| `harness.model.btree` | pass | pass | linux-x86_64, windows-x86_64 | 5 |
| `storage.integrity-check` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `sql.lexer.tokens` | pass | pass | linux-x86_64, windows-x86_64 | 5 |
| `sql.lexer.string-literals` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `sql.lexer.identifiers` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `sql.parser.statement-splitting` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `sql.parser.error-offsets` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `sql.parser.keyword-set` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `sql.select.basic` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `sql.select.where` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `sql.select.order-by` | pass | pass | linux-x86_64, windows-x86_64 | 5 |
| `sql.select.limit-offset` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `sql.expr.operators` | pass | partial | windows-x86_64 | 7 |
| `sql.expr.case` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `sql.expr.like-glob` | pass | partial | windows-x86_64 | 6 |
| `sql.expr.in-subquery` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `catalog.sqlite-schema` | pass | partial | windows-x86_64 | 6 |
| `catalog.schema-cookie` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `catalog.prepared-statement-invalidation` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `sql.insert` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `sql.update` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `sql.delete` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `sql.create-table` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `sql.drop-table` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `sql.create-index` | pass | pass | linux-x86_64, windows-x86_64 | 5 |
| `sql.conflict-resolution` | pass | pass | linux-x86_64, windows-x86_64 | 5 |
| `txn.begin-commit-rollback` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `txn.rollback-journal` | pass | pass | linux-x86_64, windows-x86_64 | 7 |
| `txn.hot-journal-recovery` | pass | pass | linux-x86_64, windows-x86_64 | 6 |
| `txn.savepoints` | pass | pass | linux-x86_64, windows-x86_64 | 6 |
| `txn.statement-journal` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `txn.synchronous-modes` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `sql.select.joins` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `sql.select.compound` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `sql.select.group-by-having` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `sql.select.distinct` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `sql.with.cte` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `sql.with.recursive` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `sql.select.window` | pass | partial | windows-x86_64 | 4 |
| `sql.upsert` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `sql.returning` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `sql.create-view` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `sql.create-trigger` | pass | pass | linux-x86_64, windows-x86_64 | 6 |
| `sql.alter-table` | pass | pass | linux-x86_64, windows-x86_64 | 8 |
| `sql.generated-columns` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `sql.autoincrement` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `sql.negative.autoincrement-placement` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `sql.without-rowid` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `sql.strict-tables` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `sql.explain` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `sql.analyze` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `sql.reindex` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `sql.vacuum` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `planner.access-paths` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `planner.join-order` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `planner.statistics` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `sql.foreign-keys.immediate` | pass | pass | linux-x86_64, windows-x86_64 | 10 |
| `sql.foreign-keys.deferred` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `sql.foreign-keys.actions` | pass | pass | linux-x86_64, windows-x86_64 | 7 |
| `sql.temp-objects` | pass | pass | linux-x86_64, windows-x86_64 | 12 |
| `sql.attach-detach` | pass | pass | linux-x86_64, windows-x86_64 | 13 |
| `txn.multi-database-commit` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `txn.master-journal` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `wal.mode-switch` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `wal.frame-format` | pass | pass | linux-x86_64, windows-x86_64 | 7 |
| `wal.index` | pass | pass | linux-x86_64, windows-x86_64 | 10 |
| `wal.read-transactions` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `wal.write-transactions` | pass | pass | linux-x86_64, windows-x86_64 | 5 |
| `wal.checkpoint.passive` | pass | pass | linux-x86_64, windows-x86_64 | 5 |
| `wal.checkpoint.full` | pass | partial | windows-x86_64 | 2 |
| `wal.checkpoint.restart` | pass | partial | windows-x86_64 | 2 |
| `wal.checkpoint.truncate` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `wal.recovery` | pass | pass | linux-x86_64, windows-x86_64 | 6 |
| `txn.busy-handler` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `txn.isolation` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `functions.core` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `functions.aggregate` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `functions.date-time` | pass | partial | windows-x86_64 | 2 |
| `functions.math` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `functions.json` | pass | pass | linux-x86_64, windows-x86_64 | 8 |
| `functions.window` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `pragma.schema` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `pragma.pager` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `pragma.integrity` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `pragma.query-only` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `pragma.rearchitecture.honoured` | pass | partial | windows-x86_64 | 4 |
| `pragma.rearchitecture.fixed` | pass | partial | windows-x86_64 | 1 |
| `pragma.rearchitecture.silent` | pass | partial | windows-x86_64 | 1 |
| `vtab.contract` | pass | pass | linux-x86_64, windows-x86_64 | 7 |
| `vtab.eponymous` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `ext.fts5.queries` | pass | partial | - | 18 |
| `ext.fts5.ranking` | pass | pass | linux-x86_64, windows-x86_64 | 5 |
| `ext.rtree` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `capi.open-close` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `capi.prepare-step-finalize` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `capi.bind` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `capi.column-metadata` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `capi.hooks` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `capi.backup` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `capi.blob-io` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `capi.serialize-deserialize` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `capi.custom-functions` | pass | pass | linux-x86_64, windows-x86_64 | 7 |
| `capi.vfs-registration` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `cli.dot-commands` | pass | pass | linux-x86_64, windows-x86_64 | 10 |
| `cli.output-modes` | pass | pass | linux-x86_64, windows-x86_64 | 9 |
| `search.virtual-table` | pass | pass | linux-x86_64, windows-x86_64 | 13 |
| `search.transactional-visibility` | pass | pass | linux-x86_64, windows-x86_64 | 13 |
| `search.legacy-migration` | pass | pass | linux-x86_64, windows-x86_64 | 11 |
| `perf.qualified-measurement` | pass | pass | linux-x86_64, windows-x86_64 | 11 |
| `perf.optimization-arms` | pass | pass | linux-x86_64, windows-x86_64 | 6 |
| `perf.regression-tracking` | pass | pass | linux-x86_64, windows-x86_64 | 6 |
| `optional.session-extension` | missing | missing | - | 0 |
| `optional.preupdate-hook` | missing | missing | - | 0 |
| `optional.snapshot-api` | missing | missing | - | 0 |
| `optional.unlock-notify` | missing | missing | - | 0 |
| `optional.rbu` | missing | missing | - | 0 |
| `optional.geopoly` | missing | missing | - | 0 |
| `optional.rtree-geometry-callbacks` | missing | missing | - | 0 |
| `sql.negative.right-outer-join-pre-3-39` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `sql.negative.grant-revoke` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `sql.negative.full-alter-table` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `sql.negative.trigger-for-each-statement` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `sql.negative.writable-views` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `values.collation.registry` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `values.real-to-text` | pass | pass | linux-x86_64, windows-x86_64 | 6 |
| `values.text-encodings` | pass | pass | linux-x86_64, windows-x86_64 | 11 |
| `values.subtype` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `values.record-validation` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `values.limits` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `values.no-panic-on-hostile-input` | pass | pass | linux-x86_64, windows-x86_64 | 6 |
| `storage.pager.sticky-errors` | pass | pass | linux-x86_64, windows-x86_64 | 5 |
| `storage.pager.read-only` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `storage.btree.page-validation` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `storage.schema.scan` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `storage.freelist.read` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `storage.pointer-map.read` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `storage.without-rowid.read` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `harness.fixtures.corpus` | pass | pass | linux-x86_64, windows-x86_64 | 5 |
| `sql.parser.syntax-obligations` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `sql.parser.differential-fuzzing` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `sql.parser.limits` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `sql.parser.purity` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `sql.binder.name-resolution` | pass | partial | windows-x86_64 | 4 |
| `sql.binder.collation-precedence` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `sql.select.distinct-basic` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `sql.select.aggregates-basic` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `sql.select.group-by-basic` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `sql.select.inner-join-basic` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `sql.functions.scalar-core` | pass | partial | windows-x86_64 | 7 |
| `vm.bytecode.verifier` | missing | missing | - | 0 |
| `vm.statement.lifecycle` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `vm.statement.interrupt` | missing | missing | - | 0 |
| `api.rust.bind-step-reset` | pass | partial | windows-x86_64 | 3 |
| `api.rust.read-only-open` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `txn.journal-modes` | pass | pass | linux-x86_64, windows-x86_64 | 6 |
| `txn.crash-matrix` | pass | pass | linux-x86_64, windows-x86_64 | 6 |
| `txn.change-counters` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `txn.autocommit` | pass | pass | linux-x86_64, windows-x86_64 | 4 |
| `txn.resource-failures` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `txn.oom-injection` | missing | missing | - | 0 |
| `txn.writer-contention` | missing | missing | - | 0 |
| `sql.constraints.not-null` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `sql.constraints.check` | pass | pass | linux-x86_64, windows-x86_64 | 2 |
| `sql.constraints.unique` | pass | pass | linux-x86_64, windows-x86_64 | 3 |
| `sql.drop-index` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `sql.rowid-allocation` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `catalog.schema-table-query` | pass | pass | linux-x86_64, windows-x86_64 | 1 |
| `interop.cross-write` | missing | missing | - | 0 |
| `txn.hooks` | missing | missing | - | 0 |

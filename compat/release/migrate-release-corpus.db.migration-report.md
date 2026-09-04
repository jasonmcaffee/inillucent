# Migration report

- source: `C:\jason\dev\rust-db\_agent_output/task-1790/migrate\release\index`
- source generation: `g000000000001`
- staging destination: `C:\jason\dev\rust-db\_agent_output/task-1790/migrate\release\corpus.db.migrating`
- published to: `C:\jason\dev\rust-db\_agent_output/task-1790/migrate\release\corpus.db`
- target commit sequence: `5`

## Source sections

| file | bytes | sha256 |
|---|---:|---|
| `config.bin` | 697 | `30d01bbc0668357f54d3e93050bca520f2b0a6a413ff37435e2719f5d7e35ff8` |
| `graph.bin` | 345343 | `8766824e5e81203de22ac764949d00476c7fedd0110703fdd79cca606a8fd9c2` |
| `lexical.bin` | 1762530 | `1dfc54a6bfeda5176ad0f958edb11a7a5b67aa5252b3029c0bd9455122ba9441` |
| `store.bin` | 1604404 | `9dfe12b12abf10cff2834dbb9050b46d7ca5bbfbcca44724ae21e99e8aed7efa` |
| `vectors.bin` | 593429 | `cf3ac9ba1b7cb574b729323bf6536d371e1d3545db5fc022a28087129d4ad007` |

## Target tables

| table | rows | digest |
|---|---:|---|
| `document` | 520 | `0ae7d417251b125cda174e4d860413e99ff3f0c8c11072a53291b2f95d3d5416` |
| `chunk` | 2318 | `70eae38b288b561d913e9a826eeaac03adaa1b50386d211a83c4d55f50cd7e2d` |
| `document_label` | 1040 | `31e2e42f4ee66c878cf6436f2fd972d9787a5d7534295c11198996e3b03797fe` |
| `document_attribute` | 520 | `df2ce688add36a9276ea599abe1b9d4772456b14956fbba07770836097c64c92` |
| `document_flag` | 64 | `481c859973fd0c33e7be9034fe85d1448ad2cbe56993105ccaa3ff24bdf45abc` |

## Verification

| check | verdict | detail |
|---|---|---|
| `sqlite.integrity` | pass | integrity_check ok; the other engine reads 2318/2318 rows out of chunk and out of the search index's own storage |
| `counts.document` | pass | 520 rows |
| `counts.chunk` | pass | 2318 rows |
| `counts.label` | pass | 1040 rows |
| `counts.attribute` | pass | 520 rows |
| `counts.flag` | pass | 64 rows |
| `counts.search` | pass | 2318 indexed rows |
| `digest.chunk` | pass | 2318 chunks, 70eae38b288b561d913e9a826eeaac03adaa1b50386d211a83c4d55f50cd7e2d |
| `digest.document` | pass | 520 documents, 0ae7d417251b125cda174e4d860413e99ff3f0c8c11072a53291b2f95d3d5416 |
| `tombstone` | pass | 256 tombstoned documents preserved |
| `dictionary` | pass | 2 sources and 21 labels resolve to the same text |
| `bm25.raw` | pass | 12 probes rank identically before grouping |
| `bm25.grouped` | pass | 12 probes answer identically once the per-document cap of 2 is applied through the document table |
| `bm25.scores` | pass | 120 hits carry the same fused score |
| `vector.exact` | pass | 12 probes rank identically when both graphs are traversed in full |
| `vector.recall` | pass | at the default width the source finds 0.975 of the exact answer and the copy finds 0.983 |
| `hybrid.exact` | pass | 12 fused rankings agree when both graphs are traversed in full |
| `filter.deleted` | pass | 256 tombstoned chunks excluded identically |
| `reopen` | pass | 17 checks pass again on a fresh open |

## Rollback

The source was neither modified nor removed. To go back, point the
application at the source path above; nothing has to be undone, because
nothing was done to it.

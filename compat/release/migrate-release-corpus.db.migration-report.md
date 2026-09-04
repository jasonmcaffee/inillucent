# Migration report

- source: `/mnt/c/jason/dev/rust-db/_agent_output/task-1790/migrate/release/index`
- source generation: `g000000000001`
- staging destination: `/mnt/c/jason/dev/rust-db/_agent_output/task-1790/migrate/release/corpus.db.migrating`
- published to: `/mnt/c/jason/dev/rust-db/_agent_output/task-1790/migrate/release/corpus.db`
- target commit sequence: `5`

## Source sections

| file | bytes | sha256 |
|---|---:|---|
| `config.bin` | 697 | `30d01bbc0668357f54d3e93050bca520f2b0a6a413ff37435e2719f5d7e35ff8` |
| `graph.bin` | 349177 | `34c9740c6ce05605940e408154a61e27a62d59d694f7bc46bd49ac26238b8be0` |
| `lexical.bin` | 1780997 | `5de15fabce13b2a75b7605a498330be4ae8f1bc89899fd1a9b5b2e58015ff17f` |
| `store.bin` | 1629911 | `e6cb7bc4cf6f2b5f08ccc677bb42401f74a5108b899214459e7a50317bb813b5` |
| `vectors.bin` | 600085 | `8bfce6bcddab86d08d71b34af43502968737e9e601c67c636108c29a930dd03d` |

## Target tables

| table | rows | digest |
|---|---:|---|
| `document` | 569 | `dfcbe1eb8a79f915ba5d69b92dcaa2b4b91c5f80e93996ee3fddd66ad2e671c6` |
| `chunk` | 2344 | `6239ba145721f2f11bc414347d461a0003a1f6a89014406632e776d3c7ba0f08` |
| `document_label` | 1138 | `aa0d12aae03cfdb3e9aa4e277d7eadd58f7c38f9faf5c8ae98b702d3760f5e28` |
| `document_attribute` | 569 | `28715c6f780df3c585729227c1a9e7bfcac7e9c6c07c1a6ad58f94ebb23fb9ed` |
| `document_flag` | 69 | `8e5417549dda571b8fa3ac38bd9c557ce470af7b8d7ad1feb4a5089307535be2` |

## Verification

| check | verdict | detail |
|---|---|---|
| `sqlite.integrity` | pass | integrity_check ok; the other engine reads 2344/2344 rows out of chunk and out of the search index's own storage |
| `counts.document` | pass | 569 rows |
| `counts.chunk` | pass | 2344 rows |
| `counts.label` | pass | 1138 rows |
| `counts.attribute` | pass | 569 rows |
| `counts.flag` | pass | 69 rows |
| `counts.search` | pass | 2344 indexed rows |
| `digest.chunk` | pass | 2344 chunks, 6239ba145721f2f11bc414347d461a0003a1f6a89014406632e776d3c7ba0f08 |
| `digest.document` | pass | 569 documents, dfcbe1eb8a79f915ba5d69b92dcaa2b4b91c5f80e93996ee3fddd66ad2e671c6 |
| `tombstone` | pass | 303 tombstoned documents preserved |
| `dictionary` | pass | 2 sources and 21 labels resolve to the same text |
| `bm25.raw` | pass | 12 probes rank identically before grouping |
| `bm25.grouped` | pass | 12 probes answer identically once the per-document cap of 2 is applied through the document table |
| `bm25.scores` | pass | 120 hits carry the same fused score |
| `vector.exact` | pass | 12 probes rank identically when both graphs are traversed in full |
| `vector.recall` | pass | at the default width the source finds 1.000 of the exact answer and the copy finds 1.000 |
| `hybrid.exact` | pass | 12 fused rankings agree when both graphs are traversed in full |
| `filter.deleted` | pass | 303 tombstoned chunks excluded identically |
| `reopen` | pass | 17 checks pass again on a fresh open |

## Rollback

The source was neither modified nor removed. To go back, point the
application at the source path above; nothing has to be undone, because
nothing was done to it.

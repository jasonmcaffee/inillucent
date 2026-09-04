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
| `graph.bin` | 342524 | `f69a5a471cc197e455745b3c4e16e87218f28905eb9a315f12ea5823ac0fceee` |
| `lexical.bin` | 1749276 | `b9b417c5e7aa65cf0258dd7fc7b8f559e445e845f8a6034f73066381810713df` |
| `store.bin` | 1582946 | `8b61460a47610fbc382848fdf3c33a81965687c2530ca3c474dffbfd7cee6ee6` |
| `vectors.bin` | 588565 | `cf60b436f716bbc58220e2d38ed5fc749c27dc3060be24a3eabdea5bc176b166` |

## Target tables

| table | rows | digest |
|---|---:|---|
| `document` | 468 | `dd0f1ad86417a85adacb317d667c57c7613ad86236b898ad61254deaaa468d19` |
| `chunk` | 2299 | `308505b7673ea9193ac68f43114e8b58573bced9365015c074f0af338fbc2f93` |
| `document_label` | 936 | `e34e4fd50c712876d2cac3accb95989963120caec985b599c3037015efbb1fc3` |
| `document_attribute` | 468 | `c4f15cc483e84dc930708a2098d606ffe6c94989dc286fc918b80d3cea1e589d` |
| `document_flag` | 78 | `91ad6421b35f9b9e71c87e2e14c5cce459c47698cf837332eaf3f1e791f4c04d` |

## Verification

| check | verdict | detail |
|---|---|---|
| `sqlite.integrity` | pass | integrity_check ok; the other engine reads 2299/2299 rows out of chunk and out of the search index's own storage |
| `counts.document` | pass | 468 rows |
| `counts.chunk` | pass | 2299 rows |
| `counts.label` | pass | 936 rows |
| `counts.attribute` | pass | 468 rows |
| `counts.flag` | pass | 78 rows |
| `counts.search` | pass | 2299 indexed rows |
| `digest.chunk` | pass | 2299 chunks, 308505b7673ea9193ac68f43114e8b58573bced9365015c074f0af338fbc2f93 |
| `digest.document` | pass | 468 documents, dd0f1ad86417a85adacb317d667c57c7613ad86236b898ad61254deaaa468d19 |
| `tombstone` | pass | 205 tombstoned documents preserved |
| `dictionary` | pass | 2 sources and 21 labels resolve to the same text |
| `bm25.raw` | pass | 12 probes rank identically before grouping |
| `bm25.grouped` | pass | 12 probes answer identically once the per-document cap of 2 is applied through the document table |
| `bm25.scores` | pass | 120 hits carry the same fused score |
| `vector.exact` | pass | 12 probes rank identically when both graphs are traversed in full |
| `vector.recall` | pass | at the default width the source finds 1.000 of the exact answer and the copy finds 1.000 |
| `hybrid.exact` | pass | 12 fused rankings agree when both graphs are traversed in full |
| `filter.deleted` | pass | 205 tombstoned chunks excluded identically |
| `reopen` | pass | 17 checks pass again on a fresh open |

## Rollback

The source was neither modified nor removed. To go back, point the
application at the source path above; nothing has to be undone, because
nothing was done to it.

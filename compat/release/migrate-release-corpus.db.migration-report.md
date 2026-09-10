# Migration report

- source: `C:/jason/dev/inillucent/_agent_output/migrate/release/index`
- source generation: `g000000000001`
- staging destination: `C:/jason/dev/inillucent/_agent_output/measurements/migrate/release\corpus.db.migrating`
- published to: `C:/jason/dev/inillucent/_agent_output/measurements/migrate/release/corpus.db`
- target commit sequence: `5`

## Source sections

| file | bytes | sha256 |
|---|---:|---|
| `config.bin` | 697 | `30d01bbc0668357f54d3e93050bca520f2b0a6a413ff37435e2719f5d7e35ff8` |
| `graph.bin` | 353284 | `556a322b426ff1faba8521c36945dd346680ee94bdba53b8fbcbf29ffd895302` |
| `lexical.bin` | 1802743 | `dbdc3aefdad184d49255c03d7b31afc9a3835e7c5b58f985088698208e1af7d1` |
| `store.bin` | 1620159 | `f001ca11550b514115d68893596cd8eaf779779a6c1f4f957e680ae16c3ea5a4` |
| `vectors.bin` | 606997 | `81db740e59260131a3fdac511d24aa4e36ed22e27e30568e07586c7b2b8633a4` |

## Target tables

| table | rows | digest |
|---|---:|---|
| `document` | 426 | `88a40b8d0cbc28ac32d5d174a2dc707c2242dd6e332a66003736c821eb7b319e` |
| `chunk` | 2371 | `72f6d4400140e1b0f371a2841b7340c4cbb08aad8a042a4c534e897a9bd8cf24` |
| `document_label` | 852 | `19cd2d917ea199dd28c7ec4b72b561d29b953d1be3d235a912885fb9fd2d73f4` |
| `document_attribute` | 426 | `43727fbe429417f90d6772c8d0e3471b9f03f69c1855bee041ed1febd298afe5` |
| `document_flag` | 59 | `f3eade5dd1916f53b9628a76d94f65cf646a6898c6e53cefac6979697912fce1` |

## Verification

| check | verdict | detail |
|---|---|---|
| `sqlite.integrity` | pass | integrity_check ok; the other engine reads 2371/2371 rows out of chunk and out of the search index's own storage |
| `counts.document` | pass | 426 rows |
| `counts.chunk` | pass | 2371 rows |
| `counts.label` | pass | 852 rows |
| `counts.attribute` | pass | 426 rows |
| `counts.flag` | pass | 59 rows |
| `counts.search` | pass | 2371 indexed rows |
| `digest.chunk` | pass | 2371 chunks, 72f6d4400140e1b0f371a2841b7340c4cbb08aad8a042a4c534e897a9bd8cf24 |
| `digest.document` | pass | 426 documents, 88a40b8d0cbc28ac32d5d174a2dc707c2242dd6e332a66003736c821eb7b319e |
| `tombstone` | pass | 157 tombstoned documents preserved |
| `dictionary` | pass | 2 sources and 21 labels resolve to the same text |
| `bm25.raw` | pass | 12 probes rank identically before grouping |
| `bm25.grouped` | pass | 12 probes answer identically once the per-document cap of 2 is applied through the document table |
| `bm25.scores` | pass | 120 hits carry the same fused score |
| `vector.exact` | pass | 12 probes rank identically when both graphs are traversed in full |
| `vector.recall` | pass | at the default width the source finds 1.000 of the exact answer and the copy finds 1.000 |
| `hybrid.exact` | pass | 12 fused rankings agree when both graphs are traversed in full |
| `filter.deleted` | pass | 157 tombstoned chunks excluded identically |
| `reopen` | pass | 17 checks pass again on a fresh open |

## Rollback

The source was neither modified nor removed. To go back, point the
application at the source path above; nothing has to be undone, because
nothing was done to it.

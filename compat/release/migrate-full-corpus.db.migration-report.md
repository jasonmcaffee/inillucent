# Migration report

- source: `_agent_output/migrate/full/index`
- source generation: `g000000000001`
- staging destination: `_agent_output/measurements/migrate/full\corpus.db.migrating`
- published to: `_agent_output/measurements/migrate/full/corpus.db`
- target commit sequence: `1`

## Source sections

| file | bytes | sha256 |
|---|---:|---|
| `config.bin` | 696 | `eddc6675a222e1d379207b668d33168848fa31f9a7fff94a1877d6d3bf0d26ec` |
| `graph.bin` | 9905 | `488d2631bb201ddc6b3659dff4240cf19ba66063937f31eeb3727d8ff7f3063e` |
| `lexical.bin` | 10144 | `4bf2950f555903462a9203ba51fa3917bbf4f795b4d7b420e8421801cf173ed0` |
| `store.bin` | 13427 | `61c8561156d070120dac95e28aa8e514dca10d791d2c56069f25acf5196c396d` |
| `vectors.bin` | 2325 | `7ec20e5e35b2b0c3d50bd27e7427778e05c6e4539265b2e841e91f04cf882544` |

## Target tables

| table | rows | digest |
|---|---:|---|
| `document` | 36 | `aa26c117a63e808bf9ec4d863a3afdd37d37138d925945ef0dd4193ebaf3198b` |
| `chunk` | 72 | `00ded57565f550c73669ef01cafdc7956e57a39c4263027170d5baf742d56f0f` |
| `document_label` | 72 | `98380c7cf4ab1afaa43f124b3a59de8753ea6de76b072b9e8a5274f9b7ce7762` |
| `document_attribute` | 36 | `015fa6182b4a26df9ddcee056b1580890a7f9bc5d191a50ef1085961888b8cb6` |
| `document_flag` | 7 | `1dbc9ee15113e0c9336aabd68c5cc7e91d289397a966ad388d4451a4202213aa` |

## Verification

| check | verdict | detail |
|---|---|---|
| `sqlite.integrity` | pass | integrity_check ok; the other engine reads 72/72 rows out of chunk and out of the search index's own storage |
| `counts.document` | pass | 36 rows |
| `counts.chunk` | pass | 72 rows |
| `counts.label` | pass | 72 rows |
| `counts.attribute` | pass | 36 rows |
| `counts.flag` | pass | 7 rows |
| `counts.search` | pass | 72 indexed rows |
| `digest.chunk` | pass | 72 chunks, 00ded57565f550c73669ef01cafdc7956e57a39c4263027170d5baf742d56f0f |
| `digest.document` | pass | 36 documents, aa26c117a63e808bf9ec4d863a3afdd37d37138d925945ef0dd4193ebaf3198b |
| `tombstone` | pass | 18 tombstoned documents preserved |
| `dictionary` | pass | 2 sources and 3 labels resolve to the same text |
| `bm25.raw` | pass | 11 probes rank identically before grouping |
| `bm25.grouped` | pass | 11 probes answer identically once the per-document cap of 2 is applied through the document table |
| `bm25.scores` | pass | 110 hits carry the same fused score |
| `vector.exact` | pass | 12 probes rank identically when both graphs are traversed in full |
| `vector.recall` | pass | at the default width the source finds 1.000 of the exact answer and the copy finds 1.000 |
| `hybrid.exact` | pass | 11 fused rankings agree when both graphs are traversed in full |
| `filter.deleted` | pass | 18 tombstoned chunks excluded identically |
| `reopen` | pass | 17 checks pass again on a fresh open |

## Rollback

The source was neither modified nor removed. To go back, point the
application at the source path above; nothing has to be undone, because
nothing was done to it.

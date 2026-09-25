# Fuzz targets

This folder holds sixteen `cargo-fuzz` targets. A fuzz target feeds a parser random input and
reports any input that makes the parser crash. The targets need a nightly Rust toolchain.

The `fuzz` folder is left out of the workspace. That keeps `libfuzzer-sys` out of the dependency
graph that the layering check in `crates/inillucent-compat/tests/tooling/harness.rs` reads.

## Running them

```powershell
pwsh tools/run-fuzz.ps1 -WhatIf                              # print the plan and write nothing
pwsh tools/run-fuzz.ps1 -Install                             # install cargo-fuzz, then run
pwsh tools/run-fuzz.ps1 -Seconds 600                         # every target for 600 seconds
pwsh tools/run-fuzz.ps1 -Only sql_text,fts5_query -Seconds 600
```

`tools/run-fuzz.ps1` runs each target for `-Seconds` seconds (30 by default). It appends one row per
target to `tests/fuzz-history.tsv`. The row is the record of the run. A run that found nothing is
recorded too, with how long it ran.

When the nightly toolchain or `cargo-fuzz` is missing, `tools/run-fuzz.ps1` records the target as
`skipped` with the command that fixes it. A history made only of `skipped` rows shows that no fuzzing
happened.

To run one target by hand, without a row in the history:

```bash
cargo +nightly fuzz run sql_text
```

Nothing runs the fuzz targets on a schedule. Run them by hand.

## The sixteen targets and the stable test for each

Every target has a stable test in the ordinary test suite. The stable test runs on the stable
toolchain and uses a seeded random generator. A checkout without nightly still tests each parser
this way. The stable test does not use coverage feedback, so it explores less than the fuzz target.

| Target | Stable test |
|---|---|
| `varint` | `inillucent-base::varint::tests::arbitrary_bytes_never_panic` (200,000 seeded inputs) |
| `bigendian` | `inillucent-base::bytes::tests::random_offsets_never_panic` (200,000 seeded offsets) |
| `page_header` | `inillucent-base::page::tests::page_sizes_follow_the_file_format_rules` |
| `leaf_page` | `inillucent-tree::leaf::tests::no_single_byte_corruption_panics`, and `corrupt_pages_never_panic` in `inillucent-compat` |
| `interior_page` | `inillucent-pool::interior::tests::corrupting_any_header_field_is_refused`, and `corrupt_pages_never_panic` in `inillucent-compat` |
| `meta_page` | `inillucent-pool::meta::tests::corrupting_any_byte_is_detected` |
| `memcmp_key` | `inillucent-tree::key::tests::encoded_order_matches_value_order_over_random_tuples` |
| `wal_record` | the `fuzz_seeded` suite in `inillucent-wal`, and `a_corrupt_log_never_panics` in its `recovery` suite |
| `mysql` | the `protocol` suite in `inillucent-remote` |
| `postgres` | the `protocol` suite in `inillucent-remote` |
| `json` | the `json` suite in `inillucent-compat`, and the cases for `json_valid()` |
| `store` | the `search` and `vector` suites in `inillucent-compat` |
| `sql_text` | the `syntax`, `hostile` and `differential_part8` suites in `inillucent-compat` |
| `fts5_query` | the `fts5` and `fts5_parity` suites in `inillucent-compat` |
| `sqlite_file` | the `corruption` and `migrate_sqlite` suites in `inillucent-compat` |
| `segment_header` | the `search` suite in `inillucent-compat` |

The last four targets read the most exposed inputs:

| Target | Why it matters |
|---|---|
| `sql_text` | SQL text is the largest untrusted input the product reads |
| `fts5_query` | a full text search query is a second grammar, read by a second parser |
| `sqlite_file` | a SQLite file is the one input that arrives as a whole file |
| `segment_header` | the HNSW and BM25 segment readers once sized an allocation from a count read off disk without checking it |

## What to do with a crash

A crash is reproduced by its input file, which `cargo-fuzz` saves under `fuzz/artifacts/<target>/`.
Keep the file. **Add the input as a regression case in the stable test for that target.** The stable
test then checks the input on every run of the suite.

## The corpus

`fuzz/corpus/<target>/` holds the minimized inputs a run kept. The corpus is checked in, so a later
run starts from the inputs earlier runs found. Shrink the corpus with `cargo fuzz cmin <target>`
before you commit it.

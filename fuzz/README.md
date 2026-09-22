# Fuzz targets

These are `cargo-fuzz` targets. They need a nightly toolchain and are excluded
from the workspace on purpose, so that `libfuzzer-sys` never appears in the
graph the dependency-direction check walks.

## Running them

```powershell
pwsh tools/run-fuzz.ps1 -WhatIf          # the plan, and nothing written
pwsh tools/run-fuzz.ps1 -Install         # install cargo-fuzz, then run
pwsh tools/run-fuzz.ps1 -Seconds 600
pwsh tools/run-fuzz.ps1 -Only sql_text,fts5_query -Seconds 600
```

It runs every target below for a bounded time and appends a row per target to
`tests/fuzz-history.tsv`. **The row is the evidence**: a run that found nothing
is worth recording, because "nothing" only means something beside how long it
looked. A missing toolchain is a skip carrying the sentence that fixes it, and
it is still recorded, so a history with nothing but `skipped` rows says exactly
that rather than looking like a history of clean runs.

Running one by hand is the same thing without the record:

```bash
cargo +nightly fuzz run sql_text
```

## The sixteen targets, and the stable counterpart of each

Every target has a deterministic counterpart that runs in the ordinary test
suite on a stable toolchain, so a checkout with no nightly still gets the
coverage - it just gets it from a seeded generator rather than from coverage
feedback.

| target | stable counterpart |
|---|---|
| `varint` | `inillucent-base::varint::tests::arbitrary_bytes_never_panic` (200k seeded inputs) |
| `bigendian` | `inillucent-base::bytes::tests::random_offsets_never_panic` (200k seeded offsets) |
| `page_header` | `inillucent-base::page::tests::page_sizes_follow_the_file_format_rules` |
| `leaf_page` | `inillucent-tree::leaf::tests::no_single_byte_corruption_panics` and `inillucent-compat`'s `corrupt_pages_never_panic` |
| `interior_page` | `inillucent-pool::interior::tests::corrupting_any_header_field_is_refused` and `inillucent-compat`'s `corrupt_pages_never_panic` |
| `meta_page` | `inillucent-pool::meta::tests::corrupting_any_byte_is_detected` |
| `memcmp_key` | `inillucent-tree::key::tests::encoded_order_matches_value_order_over_random_tuples` |
| `wal_record` | `inillucent-wal`'s `fuzz_seeded` and `a_corrupt_log_never_panics` in its `recovery` suite |
| `mysql` | `inillucent-remote`'s `protocol` suite |
| `postgres` | `inillucent-remote`'s `protocol` suite |
| `json` | `inillucent-compat`'s `json` suite, and `json_valid()`'s own cases |
| `store` | `inillucent-compat`'s `search` and `vector` suites |
| `sql_text` | `inillucent-compat`'s `syntax`, `hostile` and `differential_part8` suites |
| `fts5_query` | `inillucent-compat`'s `fts5` and `fts5_parity` suites |
| `sqlite_file` | `inillucent-compat`'s `corruption` and `migrate_sqlite` suites |
| `segment_header` | `inillucent-compat`'s `search` suite, and the `u64::MAX` header case task-2066 section 4.1.12 added |

The last four were added by task-2066 section 4.4.7, and they are ranked in the
order that section ranks them: `sql_text` is the largest untrusted input the
product has, `fts5_query` is a second grammar read by a second parser,
`sqlite_file` is the one input that arrives as a whole file, and
`segment_header` is where section 4.1.12 found allocations sized from
unvalidated integers on disk.

## What to do with a crash

A crash found by a target is reproduced by its input file, under
`fuzz/artifacts/<target>/`. Keep the file, and **add the input as a regression
case in the stable counterpart beside it**, so it is checked forever rather than
only while somebody is fuzzing.

## The corpus

`fuzz/corpus/<target>/` holds the minimised inputs a run kept. It is checked in,
so a later run starts from what earlier runs learned rather than from nothing -
which is most of what makes the second hour of fuzzing better than the first.
`cargo fuzz cmin <target>` is what shrinks it before it is committed.

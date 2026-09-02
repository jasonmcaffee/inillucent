# Fuzz targets

These are `cargo-fuzz` targets. They need a nightly toolchain and are excluded
from the workspace on purpose, so that `libfuzzer-sys` never appears in the
graph the dependency-direction check walks.

```bash
cargo +nightly fuzz run varint
cargo +nightly fuzz run bigendian
cargo +nightly fuzz run page_header
```

Every target has a deterministic counterpart that runs in the ordinary test
suite on a stable toolchain, so a checkout with no nightly still gets the
coverage - it just gets it from a seeded generator rather than from coverage
feedback:

| target | stable counterpart |
|---|---|
| `varint` | `rustdb-base::varint::tests::arbitrary_bytes_never_panic` (200k seeded inputs) |
| `bigendian` | `rustdb-base::bytes::tests::random_offsets_never_panic` (200k seeded offsets) |
| `page_header` | `rustdb-base::page::tests::page_sizes_follow_the_file_format_rules` |

A crash found by a target is reproduced by its input file; record the file with
the ticket, and add the input as a regression case in the stable counterpart so
it is checked forever rather than only while someone is fuzzing.

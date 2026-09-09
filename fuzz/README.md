# Fuzz targets

These are `cargo-fuzz` targets. They need a nightly toolchain and are excluded
from the workspace on purpose, so that `libfuzzer-sys` never appears in the
graph the dependency-direction check walks.

```bash
cargo +nightly fuzz run varint
cargo +nightly fuzz run bigendian
cargo +nightly fuzz run page_header
cargo +nightly fuzz run leaf_page
cargo +nightly fuzz run interior_page
cargo +nightly fuzz run meta_page
cargo +nightly fuzz run memcmp_key
```

Every target has a deterministic counterpart that runs in the ordinary test
suite on a stable toolchain, so a checkout with no nightly still gets the
coverage - it just gets it from a seeded generator rather than from coverage
feedback:

| target | stable counterpart |
|---|---|
| `varint` | `inillucent-base::varint::tests::arbitrary_bytes_never_panic` (200k seeded inputs) |
| `bigendian` | `inillucent-base::bytes::tests::random_offsets_never_panic` (200k seeded offsets) |
| `page_header` | `inillucent-base::page::tests::page_sizes_follow_the_file_format_rules` |
| `leaf_page` | `inillucent-tree::leaf::tests::no_single_byte_corruption_panics` and `inillucent-compat`'s `corrupt_pages_never_panic` |
| `interior_page` | `inillucent-pool::interior::tests::corrupting_any_header_field_is_refused` and `inillucent-compat`'s `corrupt_pages_never_panic` |
| `meta_page` | `inillucent-pool::meta::tests::corrupting_any_byte_is_detected` |
| `memcmp_key` | `inillucent-tree::key::tests::encoded_order_matches_value_order_over_random_tuples` |

A crash found by a target is reproduced by its input file. Keep the file, and
add the input as a regression case in the stable counterpart beside it, so it is
checked forever rather than only while somebody is fuzzing.

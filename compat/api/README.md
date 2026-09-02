# The C API surface

Phase 12 fills this in with:

- `symbols.toml`, the exported symbol list with each function's signature and
  the profile it belongs to;
- ABI probes: small C programs that compile against `sqlite3.h`, link against
  rust-db, and check struct layout, calling convention and destructor order.

It is empty until then. The manifest rows for `capi.*` are `missing`, which is
the honest state.

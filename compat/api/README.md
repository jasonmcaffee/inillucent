# The obligation registers

Three generated files, and nothing hand-written. Each answers a question the
parity manifest asks but cannot answer for itself: the manifest says a family is
covered, and these say what "the family" is.

- `builtins.toml` — every SQL function, with its kind, its arity and its flags.
  `deterministic` and `innocuous` are here because they are not decoration: they
  decide where a function may be called from, and a schema calling one that is
  neither is the thing `PRAGMA trusted_schema` exists to stop.
- `pragmas.toml` — every PRAGMA, the columns of its answer, and whether it takes
  an argument in parentheses. That is the whole of what a caller needs before it
  writes one.
- `symbols.toml` — every symbol the C ABI exports, with the signature a caller
  compiles against. It is read out of the `extern "C"` declarations rather than
  kept beside them, because the declarations *are* what a linker resolves: a
  list maintained separately would be a promise, and this is a fact.

Regenerate with:

```
cargo run -p inillucent-compat --bin inillucent-obligations
```

`inillucent-compat`'s `the_registers_match_the_engine` regenerates them and compares,
so a file that has drifted from the engine fails the suite rather than sitting
there being wrong.

The ABI probes that go with `symbols.toml` are in
`crates/inillucent-compat/tests/c/capi_probe.c`. They compile against the official
`sqlite3.h`, link against each engine in turn, and require the same output from
both — which is what turns a symbol list into a claim about behaviour.

<!--
`CONTRIBUTING.md` has the long form. This is the short one, and the two lists
below are the things that fail a build on somebody else's machine rather than
producing a review comment.
-->

## What this changes, and why the obvious thing was wrong

<!-- The argument, not the diff. The diff is below. -->

## How it was verified

<!--
Name the test and say how you know it can fail. The house rule is that a test
which cannot fail is worse than no test, so: revert the fix, watch it go red, and
say so here.
-->

- [ ] `target/debug/inillucent-testrun --changed` is green
- [ ] the new test was proved red with the fix reverted
- [ ] `pwsh tools/validate.ps1` (or `sh tools/validate.sh`) is green

## Contract files, if this change touched one

- [ ] a crate's dependencies → `docs/dependency-policy.md` and `docs/invariants/layering.toml`
- [ ] a `tests/*.rs` file → its row in `tests/selection.toml`
- [ ] the command line or MCP → `crates/inillucent-cli/src/command/registry.rs`
- [ ] a module or function past its recorded size → extracted, not raised
- [ ] a number a document states → the document, and `node tools/doc-facts/check.mjs`

<!--
Not a security report. `SECURITY.md` says where those go, and it is not here.
-->

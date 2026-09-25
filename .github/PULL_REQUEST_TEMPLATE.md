<!--
`CONTRIBUTING.md` has the full checklist. The items below are the ones that make a build fail on
someone else's machine if they are missed.
-->

## What this changes, and why

<!-- Explain the reason for the change. The diff shows the code. -->

## How it was verified

<!--
Name the test and say how you know it can fail. Revert the fix, watch the test fail, and say so
here. A test that cannot fail proves nothing.
-->

- [ ] `target/debug/inillucent-testrun --changed` passes
- [ ] the new test fails with the fix reverted
- [ ] `pwsh tools/validate.ps1` (or `sh tools/validate.sh`) passes

## Rule files, if this change touched one

- [ ] a crate's dependencies: `docs/dependency-policy.md` and `docs/invariants/layering.toml`
- [ ] a `tests/*.rs` file: its row in `tests/selection.toml`
- [ ] the command line or MCP: `crates/inillucent-cli/src/command/registry.rs`
- [ ] a module or function over its recorded size: split up, with the limit left as it is
- [ ] a number a page states: the page, and `node tools/doc-facts/check.mjs`

## Documentation

- [ ] Pages that describe the changed behavior are updated in this pull request (docs/,
      agent-skills/, package readmes, drivers/README.md, and the inillucent.com chapter in
      sites/inillucent of the `black-rainbow-labs-sites` repository)
- [ ] `node tools/doc-style/check.mjs` reports no problems

<!--
Do not report a security problem here. `SECURITY.md` says where to send it.
-->

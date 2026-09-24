@AGENTS.md

## Never put a ticket number in a published document

A ticket number such as `task-NNNN` names a card on a private board. Nobody reading this repository
or inillucent.com can look it up, so it tells them nothing. Do not write one in `README.md`,
`CHANGELOG.md`, `AGENTS.md`, anything under `docs/`, `agent-skills/` or `packaging/`, or a driver or
example readme. Say what the change did instead: "the change that sized a leaf's delta area by its
free space", not "task-NNNN". A commit hash or a date is fine. A path to a design document under
`tasks/` is a file name and may stay. `node tools/doc-facts/check.mjs` fails on any other ticket
number in those files. Commit messages, code comments and the design documents under `tasks/` are
not covered by this rule.

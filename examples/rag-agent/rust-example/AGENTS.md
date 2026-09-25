# AGENTS.md: answering questions with the `philosophy` MCP server

This folder holds `rag-server`, an MCP server that searches 80 Wikipedia articles on Greek and Roman
philosophy. `.mcp.json` (Claude Code) and `opencode.json` (opencode) start it as the server named
`philosophy`. When you are asked a question about Greek or Roman philosophy, **search with the
`philosophy` tools and answer from the passages they return**. Do not answer from memory.

## The tools

| Tool | Use it to |
|---|---|
| `search` | find the passages that answer a question. Call it first, for every question |
| `get_passage` | read a hit together with the passages on either side, when the hit stops before the answer does |
| `list_documents` | check whether a person or subject has an article at all, or find the exact title for `search`'s `title` |
| `sync_status` | see whether the index is still being built or updated |
| `sync_now` | update the index from the source documents without waiting for the timer. Only when the user asks |

### `search`

```json
{ "query": "who was Seneca", "k": 5 }
```

Pass the question in plain words. Leave `mode` out unless one of these applies:

| Mode | When |
|---|---|
| `rrf` | the default. Ranks by meaning and by the question's words, and fuses the two lists |
| `keyword` | the question depends on an exact name or term, such as `Metrodorus`, `ataraxia` or `Lyceum` |
| `vector` | the question shares no words with the likely answer, such as a paraphrase of a famous saying |
| `hybrid` | you want the database's own combined ranking, with `score`, `confidence` and `origin` on each hit |

`title` limits the search to one article. Take the exact title from a hit or from `list_documents`.

## Answering

- **Cite what you used.** Every hit has a `title` and a `url`. Name the articles your answer came
  from, with their URLs.
- **Say when the articles do not cover the question.** A search always returns hits, however poor.
  Look at the smallest `distance` among the hits. Above about 0.4, the articles are about something
  else: questions about Kubernetes, sourdough and the 1994 World Cup scored 0.42 to 0.53 on this
  corpus, and the answerable questions scored 0.14 to 0.34. A small distance proves nothing on its
  own: a question about Kant scores 0.20, like a question about Plato, because both are philosophy
  and no article here is about Kant. **Read the passages.** If they do not answer the question, say
  so, and do not build an answer out of passages about something else.
- **Check `index` in the search result.** While `index.sync.running` is true, the index is still being
  filled, and a missing article may only be missing for now. Tell the user, and give the progress in
  `index.sync`.

## If a tool fails

| The result says | What to do |
|---|---|
| `no embedding model is installed` | the user needs to run `inillucent setup-embeddings all`. See `README.md` |
| `this build has no embedding support compiled in` | the server was built without the `embed` feature. `Cargo.toml` names it, so this means the file was changed |
| `bad arguments: ...` | the message names the field. Fix the call and try again |
| the `philosophy` tools are not available | the server has not been built. The user runs `cargo build --release` in this folder and restarts the agent |

## Changing this example

Read `README.md` first. Then:

```sh
cargo test --release                 # unit tests and the end to end tests, about 12 seconds once built
cargo test --release -- --ignored    # also the evaluation over the whole corpus, several minutes
```

The end to end tests start the real server and need the embedding model installed. Every SQL
statement is in `src/store.rs`, and the protocol is in `src/mcp.rs` and `src/tools.rs`.

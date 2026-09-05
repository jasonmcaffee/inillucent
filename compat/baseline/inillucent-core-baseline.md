# inillucent-core baseline, captured by task-1782

The retrieval engine as it stood before any relational work. `inillucent-baseline verify`
re-checks every digest below, so a later story can prove it did not disturb this.

- retrieval tests: **210 passed, 0 failed**
- scorecard verdict: 17 primary comparisons: 15 better, 1 equivalent, 1 inconclusive, 0 worse. Correctness gates: all pass.
- regenerate the scorecard with: `scripts/fetch-public-corpus.sh && cargo run --release -p inillucent-bench -- score --out inillucent-scorecard.json`
- tracked files: 32

| file | bytes | sha256 |
|---|---|---|
| `crates/inillucent-bench/Cargo.toml` | 370 | `16da37fe27b607f3770e631d32cd15cd18fecf1abd8b70a741a6e3c298af2bf3` |
| `crates/inillucent-bench/src/corpus.rs` | 9423 | `3724991e7b207ee724e32c9c18553e16038d5d4bdd23b0ddf34348e0b9e97639` |
| `crates/inillucent-bench/src/embedcheck.rs` | 5565 | `3d6e9ee3c87d160eb47924ecd66c9ee8b92bc66acd02f38690051a53e2d5e044` |
| `crates/inillucent-bench/src/engine.rs` | 42113 | `cf78c8da15cc53ff1c16f0077af2a2b74166de0998e7eec95349739ed95392f7` |
| `crates/inillucent-bench/src/main.rs` | 28859 | `0707307462cb12a0add15b6fddc1f41512644df03db8c5b7a0182922f7f07bbb` |
| `crates/inillucent-bench/src/metrics.rs` | 15353 | `d8a1a6554e716d883d4c6a68a88e5f1192c9b801d600e2fefe8d2f49c535ae5e` |
| `crates/inillucent-bench/src/queryset.rs` | 43516 | `ff5b63d6dacb8d5b239eb827a0be8c9069f934b9ebc3f0eb3322a28a55cdcbcc` |
| `crates/inillucent-bench/src/report.rs` | 38787 | `608ecf4e712c4de68f7436b9047b2721dc7a2061bbcce78a7e43be11ae1bb624` |
| `crates/inillucent-bench/src/runs.rs` | 10825 | `7e2b03c3126d4861bd9c2cb0b130e285c1c4c7ab11653aa6a7ca61321b0fef6f` |
| `crates/inillucent-bench/src/scenarios.rs` | 82713 | `21e85e561a92b4ac74a104065baf9a5095878d7ff5193994c414cecad6f79205` |
| `crates/inillucent-bench/src/stats.rs` | 13127 | `79a0a053b545d848b95ae6eebc5a11cfc69547ff5877e9aceb749ba06519fb07` |
| `crates/inillucent-bench/src/synth.rs` | 107069 | `54f30da22cc9d9ec38d4f471d5c5b4bf784e2c5c70b57cf2093ac1c21fc523f3` |
| `crates/inillucent-bench/src/tune.rs` | 26868 | `9a016471f18b087cd64e0b4a1d5c7ba48b9f8d40dde28750f4c7aec46638c31a` |
| `crates/inillucent-core/Cargo.toml` | 634 | `ceb3888f3117f53f12fac9005733f18e92073222c76c55b2cd9819f02a278a17` |
| `crates/inillucent-core/src/binio.rs` | 3803 | `e8cf308b0ede089331a1c386c66390343be5afd30d4eff516e7fd75446fe5851` |
| `crates/inillucent-core/src/bm25.rs` | 62187 | `18d24251b6968691a86cea2184619e95e4bdbb47f36fc82a489599bb1bd739c1` |
| `crates/inillucent-core/src/distance.rs` | 3960 | `62d0a05d0fdfaedb5f6d326588b60031b24049264f07e5709e070173f527e969` |
| `crates/inillucent-core/src/embed.rs` | 3597 | `dabdc9b8bf3418c8825b501a2f6816d626ad06aea55d02059afce1de51e73f19` |
| `crates/inillucent-core/src/embed_onnx.rs` | 27857 | `30bcf737c21c168be3642761dbb64a3acca6c5de1e737f65563c1e55be8a3f37` |
| `crates/inillucent-core/src/filter.rs` | 39482 | `6ae5a038017bd3033d2ca5c1f308f262b451020abb0e063acd98e82a1ef83ac3` |
| `crates/inillucent-core/src/flat.rs` | 17861 | `b37eb3643dfaeb24ca3a98545e474c3ca9009bbdd09504308ef983b43a3a020f` |
| `crates/inillucent-core/src/hnsw.rs` | 64460 | `7b0fb3321f2ef047ca880d81d9a2ead4b892f61b7811f7a9cbaefb98b4a3e3dd` |
| `crates/inillucent-core/src/index.rs` | 75634 | `3b04235579bbc4b9d4d0cb7048a67c19b13b4236f2f1b050f9f69d25d623ca83` |
| `crates/inillucent-core/src/lib.rs` | 965 | `c741828e93878907e960a0d0a21bf2fcae43acb8163c7e9af0c44fb322dcf9e7` |
| `crates/inillucent-core/src/persist.rs` | 42266 | `72946d152a75a07dfa6ec3f05af3691e1a8357593482c416d26b24b7cbf81352` |
| `crates/inillucent-core/src/quantize.rs` | 7497 | `707c41caf88d25476d552f3bf69d0c1dca9379fc1de8083d4d1e6efd6bc18018` |
| `crates/inillucent-core/src/rank.rs` | 40185 | `9bacacd7795a2c1c4770a6d8332c78ab757290a26d463740fdbce50c2ede63d5` |
| `crates/inillucent-core/src/store.rs` | 37312 | `032fee95980718ae2d80473eb3c07a88f5c7d23251adfeda4da635eeaf0e3714` |
| `crates/inillucent-core/src/tokenize.rs` | 16492 | `132b1a444e25aa9c4f0ded88e0fdd4a64a44e2f3fe3f29f4716839f42e545f4c` |
| `crates/inillucent-core/src/vectors.rs` | 3772 | `1e9c75ccdae2f5cd680fd2eabe6e59b67789dda57dbace8dce218c72ff36a850` |
| `inillucent-scorecard.json` | 1151994 | `63300397d22cc8935d6c1f97be4bb54677f6d3f07c9b277458ef2387d87b0a43` |
| `inillucent-scorecard.md` | 25968 | `072a4747e6e1008c05f787ed48fdf1c06152e9f17d1143367645acec7491e34d` |

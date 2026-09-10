# inillucent-core baseline, captured by inillucent-baseline

The retrieval engine as it stood before any relational work. `inillucent-baseline verify`
re-checks every digest below, so a later story can prove it did not disturb this.

- retrieval tests: **222 passed, 0 failed**
- scorecard verdict: 17 primary comparisons: 15 better, 1 equivalent, 1 inconclusive, 0 worse. Correctness gates: all pass.
- regenerate the scorecard with: `scripts/fetch-public-corpus.sh && cargo run --release -p inillucent-bench -- score --out inillucent-scorecard.json`
- tracked files: 38

| file | bytes | sha256 |
|---|---|---|
| `crates/inillucent-bench/Cargo.toml` | 964 | `a846c43d65a09d2c43c7d304595a6fdaf9e6361ef313e53746524bf9051d94a3` |
| `crates/inillucent-bench/src/arm.rs` | 8213 | `3b63ebd7cc8fc9dc6f5ea383ef18ddbc7456174cc20e0d6369d4a1a651f92d4f` |
| `crates/inillucent-bench/src/corpus.rs` | 28642 | `d5a8c98d234963a18d71540a2738f672228ab6089a337910d5cc377284caf19e` |
| `crates/inillucent-bench/src/embedcheck.rs` | 16721 | `6292bc36a9499995c1d89b909c4aa1ff2ca3cd457169680f1f2cd23bffa9d04f` |
| `crates/inillucent-bench/src/engine.rs` | 41401 | `40a2312e79b1da7cb1b74d08f57295b8eca01d5a78a373efb9fc99d54ec758d9` |
| `crates/inillucent-bench/src/gradeembed.rs` | 100492 | `a3c30f330cbea1f4dfb0f9dee34eef40350dcc136dda7c524638d90c7db56f2c` |
| `crates/inillucent-bench/src/http.rs` | 8666 | `23a44f8e6cff13abbbcb560968fd91429be40c4a9a9f6d7be73c25ee874f78b8` |
| `crates/inillucent-bench/src/llamacpp.rs` | 18077 | `c606a322fe69ffecb0e7ebc0f58771217a3fda556f24940ea9a580c575e2db31` |
| `crates/inillucent-bench/src/main.rs` | 41890 | `4d34031acedc57bbed51f7631679b522159dd3b7804cc85106d31f7a8daa15c3` |
| `crates/inillucent-bench/src/metrics.rs` | 14943 | `9b66124d3cf8dcb43e16371918a52bedf39fe175afdc1a4cc489c5361a47d8e4` |
| `crates/inillucent-bench/src/models.rs` | 21054 | `0248ed0dc494e9a815edcf5aa19166add598639b62640830bfcc1a3138925a32` |
| `crates/inillucent-bench/src/queryset.rs` | 42813 | `ceb63a8753de9a2e8a8d81d20d7a075ef2e8a63629bdc06344882d1d6494ec1a` |
| `crates/inillucent-bench/src/report.rs` | 38323 | `01190f4138afa9c926334e4ad64e3a9a36f033171f7b73284f00bd721cbee627` |
| `crates/inillucent-bench/src/runs.rs` | 12029 | `ebf2bfe7c63a69fa70f864f026fa02d5548bab9d99db7abb35a73685f44de6cb` |
| `crates/inillucent-bench/src/scenarios.rs` | 83968 | `2a16c2f9eb913dd41efb0b7ab212338900ce416d652b07d21dea52e218efbaef` |
| `crates/inillucent-bench/src/stats.rs` | 13127 | `79a0a053b545d848b95ae6eebc5a11cfc69547ff5877e9aceb749ba06519fb07` |
| `crates/inillucent-bench/src/synth.rs` | 109187 | `98727e1c22c61c09fef9a0ae52fef3bf6dc8244de9c1e507dd8fee7c6a1a4cd1` |
| `crates/inillucent-bench/src/tune.rs` | 26934 | `c9c667f0a3c295a45a30190827298de6c7987b32d5f33dc119e73bf0f9e95411` |
| `crates/inillucent-core/Cargo.toml` | 616 | `eba4b8ecea88ebb6f04d3b601a41e608e497c4e2a7b5d0b2a1accc9b2427cfac` |
| `crates/inillucent-core/src/binio.rs` | 3699 | `d5c156a15ca14b40a13ac8b80959953ff79762ca235db192523199e7b722371e` |
| `crates/inillucent-core/src/bm25.rs` | 60854 | `3ff5060ec3a6c985fc3f9dad0f91b7fa165f2d39a789e75952596e7ca6dd1023` |
| `crates/inillucent-core/src/distance.rs` | 3843 | `1eb12a2405dcfa34076d2d35cc278601d95efbd406b1f5b1335d2f9fe233afac` |
| `crates/inillucent-core/src/embed.rs` | 3494 | `cb22a9903fd4a821ef882ae86620065949b58ff091614a224abbf4a5f9f1cd20` |
| `crates/inillucent-core/src/embed_onnx.rs` | 58416 | `742643888a638e625b66f7dd2b79a2d1a04a9167c27e252f6e72ebf062accef0` |
| `crates/inillucent-core/src/filter.rs` | 38494 | `26bf6907e024506ef79034390d9202ca6ede08196aa4bd6576bb5df709bcde37` |
| `crates/inillucent-core/src/flat.rs` | 22365 | `15fcee15bf5dc8769c8e95df4a8ef5d7a49fc8eb7f71f3e3ad8c44331e7af632` |
| `crates/inillucent-core/src/hnsw.rs` | 73975 | `aa23943c132835c865899f569f68df82431bb3b21ec1c00648fe17856f25bd28` |
| `crates/inillucent-core/src/index.rs` | 76017 | `2c47e6661f15fba3c68e7f77795f772139f90d99154828b8a971ffd0b18a3918` |
| `crates/inillucent-core/src/lib.rs` | 950 | `dc6f22c3ffd84a93dfaf499332f424d847daa32306601095b967580f4c09f667` |
| `crates/inillucent-core/src/model.rs` | 14245 | `cd89c11e1b0961d2b6875102712b463ef5fb335d87e9fe176f37920712e3f90d` |
| `crates/inillucent-core/src/persist.rs` | 50916 | `06aa4ce0e79c19847dbf8cccdb7d7f2859c8b8b7cfd0b952bab4cf50b3f15583` |
| `crates/inillucent-core/src/quantize.rs` | 7973 | `28f4bda2e54cc9a3acd710a9fce2ba9799f4197b7a1fdf8b2025aa6b012bf154` |
| `crates/inillucent-core/src/rank.rs` | 39285 | `51d5e74fa721f236ec30f3ef6beb768c9667c118a195195a782c5e883518a129` |
| `crates/inillucent-core/src/store.rs` | 36381 | `474e946f794c61967de8b513c1abc76ef426e562038d22c0377da159976f238e` |
| `crates/inillucent-core/src/tokenize.rs` | 16109 | `5c89348b53ea3d21cab4f67898cc49f876e4135fef245d5b8baf5c63b99b05bf` |
| `crates/inillucent-core/src/vectors.rs` | 19757 | `f9b947db82720cc05449c044f7915add1e0896c40c8b387be882b68ed63e118b` |
| `inillucent-scorecard.json` | 1152427 | `e77e6a18a49b3c57ab0cbc5c35d832f4989f044cda01782eaf509a1c9ff8a763` |
| `inillucent-scorecard.md` | 26140 | `8173dba95093a77edb3e5df261365f3fa29781a8953bdcc81bd23c1d6846b615` |

// Where the probe reads its binaries and writes its results.
//
// Derived from this file's own location rather than configured, so the probe
// runs from a clone with nothing set up: `node tools/feature-probe/run.js`.

const path = require('path');

/** The workspace root, two directories above this file. */
const ROOT = path.resolve(__dirname, '..', '..');

/** Everything the probe writes. Gitignored; the transcripts are evidence, not source. */
const OUT = path.join(ROOT, '_agent_output', 'feature-probe');

module.exports = {
  ROOT,
  OUT,
  /** The shell under test. Build it with `cargo build --release --bin inillucent-shell`. */
  OURS: path.join(ROOT, 'target', 'release', `inillucent-shell${process.platform === 'win32' ? '.exe' : ''}`),
  /** The pinned reference. Downloaded by `tools/sqlite-reference.sh`; see docs/dependency-policy.md. */
  REF: path.join(ROOT, '.sqlite-ref', '3.53.4', 'shell', `sqlite3${process.platform === 'win32' ? '.exe' : ''}`),
};

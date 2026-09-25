// Where the probe reads its binaries and writes its results.
//
// Derived from this file's own location rather than configured, so the probe
// runs from a clone with nothing set up: `node tools/feature-probe/run.js`.

const path = require('path');
const { execFileSync } = require('child_process');

/** The workspace root, two directories above this file. */
const ROOT = path.resolve(__dirname, '..', '..');

/** Everything the probe writes. Gitignored; the transcripts are evidence, not source. */
const OUT = path.join(ROOT, '_agent_output', 'feature-probe');

/**
 * Returns the directory cargo builds into for this checkout.
 *
 * A git worktree made for a ticket has a `.cargo/config.toml` that names its own `target-dir`, so
 * `target/` under the worktree is empty there. Cargo knows the answer in both cases.
 */
function targetDirectory() {
  try {
    const metadata = execFileSync('cargo', ['metadata', '--format-version', '1', '--no-deps', '--manifest-path', path.join(ROOT, 'Cargo.toml')], { cwd: ROOT, encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'], maxBuffer: 64 * 1024 * 1024 });
    return JSON.parse(metadata).target_directory;
  } catch {
    return path.join(ROOT, 'target');
  }
}

module.exports = {
  ROOT,
  OUT,
  /** The shell under test. Build it with `cargo build --release --bin inillucent-shell`. */
  OURS: path.join(targetDirectory(), 'release', `inillucent-shell${process.platform === 'win32' ? '.exe' : ''}`),
  /** The pinned reference. Downloaded by `tools/sqlite-reference.sh`; see docs/dependency-policy.md. */
  REF: path.join(ROOT, '.sqlite-ref', '3.53.4', 'shell', `sqlite3${process.platform === 'win32' ? '.exe' : ''}`),
};

#!/usr/bin/env node
// The `inillucent` shim: resolve the platform binary and become it.
//
// `spawn` with `stdio: 'inherit'` rather than an exec, because Node has no
// execve and this has to work on Windows. The child's exit code and its signal
// are both passed back up: a wrapper that always exited 0 would break every
// script that branches on the exit code, and inillucent's exit codes carry
// meaning - 3 is "the engine has not built that".
import { spawn } from 'node:child_process';
import { resolveBinary } from '../resolve.mjs';

let binary;
try {
  binary = resolveBinary('inillucent');
} catch (why) {
  process.stderr.write(String(why.message ?? why) + '\n');
  process.exit(1);
}

const child = spawn(binary, process.argv.slice(2), { stdio: 'inherit' });
child.on('error', (why) => {
  process.stderr.write(`inillucent: could not run ${binary}: ${why.message}\n`);
  process.exit(1);
});
child.on('exit', (code, signal) => {
  if (signal) {
    // Re-raise it on ourselves so a shell sees the same thing it would have
    // seen from the real program, rather than a plain exit.
    process.kill(process.pid, signal);
    return;
  }
  process.exit(code ?? 0);
});

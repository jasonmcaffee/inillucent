// Finds the binary for the machine this is running on.
//
// The arrangement is esbuild's, and it is the right one: the binaries live in
// per-platform packages listed as `optionalDependencies`, npm installs only the
// one that matches, and this package's `bin` entries are tiny shims that
// resolve it and hand over. There is no postinstall script and nothing is
// downloaded at install time, which means `npm ci` works offline, in a locked
// CI, and behind a registry proxy - none of which is true of a package that
// fetches a binary from GitHub when it is installed.
//
// The cost is that a platform without a published package cannot install, and
// the failure has to say so in a sentence somebody can act on rather than as an
// unresolved import. That is what `resolveBinary` is for.

import { createRequire } from 'node:module';
import { accessSync, constants } from 'node:fs';

const require = createRequire(import.meta.url);

/** The platform packages, by the `process.platform`-`process.arch` pair each serves. */
const PACKAGES = {
  'win32-x64': '@inillucent/cli-win32-x64',
  'darwin-arm64': '@inillucent/cli-darwin-arm64',
  'darwin-x64': '@inillucent/cli-darwin-x64',
  'linux-x64': '@inillucent/cli-linux-x64',
};

/** The four programs the release ships, and what each one is for. */
export const PROGRAMS = {
  inillucent: 'the command line: query, exec, describe, import, export, search',
  'inillucent-shell': 'the interactive sqlite3-shaped shell',
  'inillucent-mcp': 'the MCP server, for an agent',
  'inillucent-migrate': 'builds an inillucent database from a SQLite file',
};

/**
 * Returns the platform package name for this machine, or null if there is none.
 */
export function platformPackage() {
  return PACKAGES[`${process.platform}-${process.arch}`] ?? null;
}

/**
 * Returns the absolute path of one of the four programs on this machine.
 *
 * @param program - which program, as it is named in PROGRAMS
 */
export function resolveBinary(program) {
  if (!(program in PROGRAMS)) {
    throw new Error(`inillucent has no program called ${program}`);
  }
  const name = platformPackage();
  if (!name) {
    throw new Error(
      `inillucent has no prebuilt binary for ${process.platform}-${process.arch}.\n` +
        `  Build it from source instead:  cargo install inillucent-cli\n` +
        `  Or open an issue: https://github.com/jasonmcaffee/inillucent/issues`,
    );
  }
  const suffix = process.platform === 'win32' ? '.exe' : '';
  let path;
  try {
    // The platform package's own entry point tells us where its bin directory
    // is, rather than this package guessing at a node_modules layout - which
    // pnpm, yarn's pnp and a hoisted npm tree all arrange differently.
    path = require.resolve(`${name}/bin/${program}${suffix}`);
  } catch {
    throw new Error(
      `inillucent's binary for ${process.platform}-${process.arch} is not installed.\n` +
        `  The package ${name} should have been installed as an optional dependency.\n` +
        `  If your installer was run with --no-optional, install it directly:\n` +
        `    npm install ${name}\n`,
    );
  }
  try {
    accessSync(path, constants.X_OK);
  } catch {
    // An npm tarball does not always preserve the executable bit, and the
    // failure it produces otherwise is EACCES from execve with no explanation.
    throw new Error(
      `${path} is not executable.\n  Fix it with:  chmod +x ${path}\n` +
        `  Then please report it: https://github.com/jasonmcaffee/inillucent/issues`,
    );
  }
  return path;
}

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
import { accessSync, constants, existsSync } from 'node:fs';
import { dirname, join } from 'node:path';

const require = createRequire(import.meta.url);

// **The scope here is the scope they are published under, and the two came apart (task-1995).**
// The packages were renamed from `@inillucent/*` to `@blackrainbowlabs/*` in build.mjs and in this
// package's optionalDependencies, and this table was missed. npm then installed
// `@blackrainbowlabs/cli-win32-x64` correctly and the shim looked for `@inillucent/cli-win32-x64`,
// so every install on every platform ended at "inillucent's binary for win32-x64 is not installed"
// - naming a package that does not exist. It reached the registry, where a version cannot be
// replaced. resolve.test.mjs compares this table against package.json and against build.mjs, and it
// no longer hard-codes a scope, so a rename that touches one of the three fails here instead.
/** The platform packages, by the `process.platform`-`process.arch` pair each serves. */
const PACKAGES = {
  'win32-x64': '@blackrainbowlabs/cli-win32-x64',
  'darwin-arm64': '@blackrainbowlabs/cli-darwin-arm64',
  'darwin-x64': '@blackrainbowlabs/cli-darwin-x64',
  'linux-x64': '@blackrainbowlabs/cli-linux-x64',
  'linux-arm64': '@blackrainbowlabs/cli-linux-arm64',
};

/** The four programs the release ships, and what each one is for. */
export const PROGRAMS = {
  inillucent: 'the command line: query, exec, describe, import, export, search',
  'inillucent-shell': 'the interactive sqlite3-shaped shell',
  'inillucent-mcp': 'the MCP server, for an agent',
  'inillucent-migrate': 'builds an inillucent database from a legacy retrieval index',
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
  // **`INILLUCENT_BIN` wins, the way it already does for the Go and PHP
  // wrappers (task-1969, 4.5).** It names the `inillucent` binary; the others
  // are looked for beside it, which is where a build and an install both put
  // them. Without this the wrappers stage of `tools/validate` could point the
  // other two languages at a freshly built binary and had no way to point this
  // one, so the only npm test that could run was the one that reads the
  // platform table as text.
  //
  // It throws rather than falling through when the named binary is not there:
  // a caller who says where the binary is and is wrong wants to know, not to
  // have the resolver quietly go looking somewhere else.
  const named = process.env.INILLUCENT_BIN;
  if (named) {
    const suffix = process.platform === 'win32' ? '.exe' : '';
    const beside = program === 'inillucent'
      ? named
      : join(dirname(named), `${program}${suffix}`);
    if (!existsSync(beside)) {
      throw new Error(
        `INILLUCENT_BIN is set and ${beside} is not there.
` +
          `  Unset INILLUCENT_BIN to look for an installed copy instead.`,
      );
    }
    return beside;
  }
  const name = platformPackage();
  if (!name) {
    throw new Error(
      `inillucent has no prebuilt binary for ${process.platform}-${process.arch}.\n` +
        `  Build it from source instead:  cargo install inillucent-cli\n` +
        `  Or open an issue: https://github.com/Black-Rainbow-Labs/Inillucent/issues`,
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
        `  Then please report it: https://github.com/Black-Rainbow-Labs/Inillucent/issues`,
    );
  }
  return path;
}

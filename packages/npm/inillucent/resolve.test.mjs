// The platform table, checked against what the release builds and what npm
// would install.
//
// Invariant: the three lists agree. `resolve.mjs` maps a platform to a package
// name, `package.json` lists those packages as optional dependencies, and
// `build.mjs` stages one directory per package from a release archive. A
// platform present in one and absent from another installs the shim with no
// binary behind it, and the failure a person sees is an unresolved import
// rather than "there is no build for your machine".
//
// The defect this was written for (task-1932, H12): the release has built
// `aarch64-unknown-linux-gnu` since packaging began and npm was the one wrapper
// that never offered it, so `npm i inillucent` on a Graviton or an Ampere
// machine installed the shim and nothing else.
//
//   node --test packages/npm/inillucent/resolve.test.mjs

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

import { PROGRAMS } from './resolve.mjs';

const here = dirname(fileURLToPath(import.meta.url));

/** Reads this package's manifest. */
function manifest() {
  return JSON.parse(readFileSync(join(here, 'package.json'), 'utf8'));
}

/** Reads the platform table out of `resolve.mjs`, which does not export it. */
function resolverPackages() {
  const source = readFileSync(join(here, 'resolve.mjs'), 'utf8');
  const table = source.slice(source.indexOf('const PACKAGES'));
  const body = table.slice(table.indexOf('{'), table.indexOf('};') + 1);
  // **Any scope, not `@inillucent` (task-1995).** Hard-coding the scope meant that when the
  // packages were renamed to `@blackrainbowlabs/*`, the matcher below found nothing in build.mjs,
  // the comparison ran over an empty list, and the test passed while the published wrapper could
  // not find its own binary on any platform.
  return [...body.matchAll(/'([\w-]+)':\s*'(@[\w-]+\/[\w-]+)'/g)].map(
    ([, platform, name]) => ({ platform, name })
  );
}

/** Reads the platform table out of the build script. */
function builtPackages() {
  const source = readFileSync(join(here, '..', 'build.mjs'), 'utf8');
  return [...source.matchAll(/npm:\s*'(@[\w-]+\/[\w-]+)',\s*target:\s*'([\w-]+)'/g)].map(
    ([, name, target]) => ({ name, target })
  );
}

test('every platform the resolver knows is an optional dependency', () => {
  const optional = Object.keys(manifest().optionalDependencies ?? {});
  const resolved = resolverPackages().map((one) => one.name);
  assert.ok(resolved.length >= 5, `the resolver knows ${resolved.length} platforms`);
  for (const name of resolved) {
    assert.ok(
      optional.includes(name),
      `${name} is in resolve.mjs and not in package.json's optionalDependencies, so npm ` +
        `never installs it`
    );
  }
  for (const name of optional) {
    assert.ok(
      resolved.includes(name),
      `${name} is an optional dependency and not in resolve.mjs, so it is downloaded and ` +
        `never used`
    );
  }
});

test('every package the build stages is one the resolver asks for', () => {
  const built = builtPackages();
  const resolved = resolverPackages().map((one) => one.name);
  assert.ok(built.length >= 5, `the build stages ${built.length} packages`);
  for (const { name } of built) {
    assert.ok(
      resolved.includes(name),
      `build.mjs stages ${name} and resolve.mjs never asks for it`
    );
  }
  for (const name of resolved) {
    assert.ok(
      built.some((one) => one.name === name),
      `resolve.mjs asks for ${name} and build.mjs never stages it, so the install finds ` +
        `nothing`
    );
  }
});

test('linux arm64 is published', () => {
  const resolved = resolverPackages();
  const arm = resolved.find((one) => one.platform === 'linux-arm64');
  assert.ok(arm, 'linux-arm64 has no package, and the release builds that target');
  const built = builtPackages().find((one) => one.name === arm.name);
  assert.ok(built, `${arm.name} is not staged by build.mjs`);
  assert.equal(
    built.target,
    'aarch64-unknown-linux-gnu',
    'linux-arm64 is staged from the wrong release target'
  );
});

test('every program the resolver offers is one the release ships', () => {
  const names = Object.keys(PROGRAMS);
  assert.deepEqual(
    names.sort(),
    ['inillucent', 'inillucent-mcp', 'inillucent-migrate', 'inillucent-shell'],
    'the four programs are what the release ships'
  );
  const bin = manifest().bin ?? {};
  for (const name of names) {
    assert.ok(bin[name], `${name} is offered by resolve.mjs and has no bin entry`);
  }
});

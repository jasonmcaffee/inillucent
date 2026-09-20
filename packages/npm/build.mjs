#!/usr/bin/env node
// Stages the npm packages from a built dist/, and optionally publishes them.
//
//   node packages/npm/build.mjs                     # stage into packages/npm/staged
//   node packages/npm/build.mjs --pack              # and `npm pack` each one
//   node packages/npm/build.mjs --publish           # and publish, platform packages first
//   node packages/npm/build.mjs --publish --dry-run # say what it would publish
//   node packages/npm/build.mjs --publish --otp 123456   # 2FA on writes
//   node packages/npm/build.mjs --publish --version 0.1.3 # a release the tree has moved past
//
// **--otp is how a publish reaches an account that has two-factor on.** npm
// answers a publish without one with `403 ... Two-factor authentication or
// granular access token with bypass 2fa enabled is required to publish
// packages`, and that is a property of the credential rather than of anything
// here: a token proves who you are, and two-factor is a second thing the
// account asks for on a write. The six digits from the authenticator app
// satisfy it. npm accepts one code for the whole of its 30-second window, so
// every package is published in a single pass with no pause between them.
//
// It is the durable route as well as the immediate one. npm restricted
// two-factor-bypass tokens for account changes on 2026-07-31 and turns off
// direct publishing with them in January 2027, so a token minted to skip the
// second factor is a credential with an expiry date on it.
//
// The order matters and is not a detail: `inillucent` lists the platform
// packages as optionalDependencies at an exact version, so publishing it first
// makes a package that npm cannot resolve for the minutes until the others land.
// So the platform packages go first, and the wrapper last.
//
// Only the platforms whose archives are in dist/ are staged. Building for a
// platform you do not have is not possible here and pretending otherwise would
// publish an empty package - so an absent one is reported and skipped, and the
// wrapper still installs everywhere the others do, because npm treats a missing
// optionalDependency as a platform it does not serve rather than as an error.

import { existsSync, mkdirSync, rmSync, copyFileSync, writeFileSync, readFileSync, chmodSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

/** A trailing newline, so the staged package.json ends the way every other JSON file here does. */
const NEWLINE = String.fromCharCode(10);

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '..', '..');
const dist = join(root, 'dist');
const staged = join(here, 'staged');

/** Each npm platform package, and the release target it carries. */
const PLATFORMS = [
  { npm: '@blackrainbowlabs/cli-win32-x64', target: 'x86_64-pc-windows-msvc', os: 'win32', cpu: 'x64', exe: '.exe', library: 'inillucent_driver_capi.dll' },
  // **Both macOS packages come from the one universal archive**, which is what the release builds
  // now (`task-1995`): `lipo`'s job is done by `rcodesign macho-universal-create`, and the result
  // carries an arm64 and an x86-64 slice in every program. There is no
  // `inillucent-<version>-aarch64-apple-darwin` archive to find, so before this both macOS packages
  // were reported absent and skipped - and the wrapper then listed two optional dependencies that
  // do not exist, which npm skips without a word, leaving a Mac with the shim and no binary.
  //
  // `os` and `cpu` still differ, so npm installs exactly one of the two on any given Mac. They hold
  // the same bytes; what each declares is which machine it is for.
  { npm: '@blackrainbowlabs/cli-darwin-arm64', target: 'aarch64-apple-darwin', source: 'universal-apple-darwin', os: 'darwin', cpu: 'arm64', exe: '', library: 'libinillucent_driver_capi.dylib' },
  { npm: '@blackrainbowlabs/cli-darwin-x64', target: 'x86_64-apple-darwin', source: 'universal-apple-darwin', os: 'darwin', cpu: 'x64', exe: '', library: 'libinillucent_driver_capi.dylib' },
  { npm: '@blackrainbowlabs/cli-linux-x64', target: 'x86_64-unknown-linux-gnu', os: 'linux', cpu: 'x64', exe: '', library: 'libinillucent_driver_capi.so' },
  // The release has built this target since packaging began and npm was
  // the one wrapper that did not offer it, so `npm i inillucent` on a
  // Graviton or an Ampere machine installed the shim and no binary
  // (task-1932, H12).
  { npm: '@blackrainbowlabs/cli-linux-arm64', target: 'aarch64-unknown-linux-gnu', os: 'linux', cpu: 'arm64', exe: '', library: 'libinillucent_driver_capi.so' },
];

const PROGRAMS = ['inillucent', 'inillucent-shell', 'inillucent-mcp', 'inillucent-migrate'];

const argv = process.argv.slice(2);
const options = new Set(argv);

/** Reads `--name value` off the command line, or null when it is absent. */
function optionValue(name) {
  const at = argv.indexOf(name);
  return at >= 0 && argv[at + 1] ? argv[at + 1] : null;
}
// **`--version` stages a release the working tree is not on.** The tree moves to the next version
// as soon as a release is cut, so by the time anything is being published the wrapper's package.json
// already names the version after it. Reading only that number meant npm could be given 0.1.4 while
// the site, PyPI, Homebrew and the GitHub release all served 0.1.3 - the mismatch the release is
// built to prevent, in the one channel that had not been published yet.
const version = optionValue('--version') ??
  JSON.parse(readFileSync(join(here, 'inillucent', 'package.json'), 'utf8')).version;

/**
 * Writes one platform package's manifest.
 *
 * `os` and `cpu` are what make npm install exactly one of these four on any
 * given machine: a package whose `os` does not match is skipped, which is why
 * the wrapper can list all four as optional dependencies and still install
 * cleanly everywhere.
 *
 * @param platform - the platform entry
 * @param into - the directory to write into
 */
function writeManifest(platform, into) {
  const manifest = {
    name: platform.npm,
    version,
    description: `The inillucent binaries for ${platform.os}-${platform.cpu}. Installed automatically by the 'inillucent' package; there is no reason to depend on this directly.`,
    homepage: 'https://github.com/Black-Rainbow-Labs/Inillucent#readme',
    repository: { type: 'git', url: 'git+https://github.com/Black-Rainbow-Labs/Inillucent.git' },
    license: 'MIT',
    author: 'Black Rainbow Labs',
    os: [platform.os],
    cpu: [platform.cpu],
    files: ['bin', 'lib', 'include', 'README.md', 'LICENSE'],
    preferUnplugged: true,
  };
  writeFileSync(join(into, 'package.json'), JSON.stringify(manifest, null, 2) + '\n');
}

/**
 * Returns the unpacked release directory for one platform, unpacking the
 * archive first when only the archive is there.
 *
 * packaging/release.ps1 leaves both the directory and the .zip behind, so on
 * the machine that built a release the directory is already present. An archive
 * that arrived from another machine - the Linux tarball built in WSL, a macOS
 * one built on the MacBook - is only the archive, and skipping it meant the
 * release could not be staged anywhere except the machine that built it.
 *
 * @param platform - the platform entry
 */
function unpacked(platform) {
  // `source` is the archive this package's binaries come from, which is the target itself unless a
  // platform says otherwise - the two macOS packages share one universal archive.
  const from = platform.source ?? platform.target;
  const directory = join(dist, `inillucent-${version}-${from}`);
  if (existsSync(directory)) return directory;

  for (const extension of ['.tar.gz', '.zip']) {
    const name = `inillucent-${version}-${from}${extension}`;
    if (!existsSync(join(dist, name))) continue;
    // tar reads both, on Windows 10 1803 and later as well as on macOS and
    // Linux, so there is one command rather than one per platform.
    //
    // It is given the archive's plain name and run with dist/ as its working
    // directory rather than an absolute path, because GNU tar reads a path
    // starting `C:` as a host to connect to and answers `Cannot connect to C:
    // resolve failed`. A name with no colon in it is unambiguous to every tar.
    execFileSync('tar', ['--extract', '--file', name], { cwd: dist, stdio: 'inherit' });
    if (existsSync(directory)) return directory;
  }
  return null;
}

/**
 * Stages one platform package from its release archive directory.
 *
 * @param platform - the platform entry
 */
function stage(platform) {
  const source = unpacked(platform);
  if (source === null) {
    console.log(`  ${platform.npm}: no release archive for ${platform.target}, skipped`);
    return null;
  }
  const into = join(staged, platform.npm.replace('@', '').replace('/', '-'));
  rmSync(into, { recursive: true, force: true });
  mkdirSync(join(into, 'bin'), { recursive: true });
  mkdirSync(join(into, 'lib'), { recursive: true });
  mkdirSync(join(into, 'include'), { recursive: true });
  for (const program of PROGRAMS) {
    const file = join(into, 'bin', program + platform.exe);
    copyFileSync(join(source, 'bin', program + platform.exe), file);
    // npm preserves the executable bit for files listed under `bin`, but these
    // are not npm bins - the wrapper's shims are - so the bit has to be set
    // here or the shim's execve fails with EACCES on a Unix machine.
    chmodSync(file, 0o755);
  }
  copyFileSync(join(source, 'lib', platform.library), join(into, 'lib', platform.library));
  copyFileSync(join(source, 'include', 'inillucent_driver.h'), join(into, 'include', 'inillucent_driver.h'));
  copyFileSync(join(root, 'LICENSE'), join(into, 'LICENSE'));
  writeFileSync(
    join(into, 'README.md'),
    `# ${platform.npm}\n\nThe inillucent binaries for ${platform.os}-${platform.cpu}.\n\n` +
      `Install [\`inillucent\`](https://www.npmjs.com/package/inillucent) instead; ` +
      `npm picks this package up automatically on a matching machine.\n`,
  );
  writeManifest(platform, into);
  console.log(`  ${platform.npm}: staged from ${platform.target}`);
  return into;
}

/**
 * Runs npm in a directory and lets its output through.
 *
 * @param directory - where to run
 * @param args - the npm arguments
 */
function npm(directory, args, secret = []) {
  // The printed line leaves `secret` out, so a one-time code does not reach the
  // log a release leaves behind. It is still an argument to npm, and so still
  // readable in a process listing for the second it runs - npm's CLI takes it no
  // other way, and the code expires in 30 seconds regardless.
  console.log(`  npm ${args.join(' ')}  (in ${directory})`);
  execFileSync('npm', [...args, ...secret], { cwd: directory, stdio: 'inherit', shell: process.platform === 'win32' });
}

console.log(`staging inillucent ${version} npm packages`);
rmSync(staged, { recursive: true, force: true });
mkdirSync(staged, { recursive: true });

const built = [];
for (const platform of PLATFORMS) {
  const into = stage(platform);
  if (into) {
    built.push(into);
  }
}

// The wrapper is copied rather than published from its source directory, so the
// staged tree is the whole of what would be published and can be inspected.
const wrapper = join(staged, 'inillucent');
rmSync(wrapper, { recursive: true, force: true });
mkdirSync(join(wrapper, 'bin'), { recursive: true });
for (const file of ['package.json', 'index.mjs', 'resolve.mjs', 'README.md']) {
  const from = join(here, 'inillucent', file);
  if (existsSync(from)) {
    copyFileSync(from, join(wrapper, file));
  }
}
for (const program of PROGRAMS) {
  copyFileSync(join(here, 'inillucent', 'bin', `${program}.mjs`), join(wrapper, 'bin', `${program}.mjs`));
}

// **The wrapper's own version and its pins are rewritten to what is being published.** Copying its
// package.json verbatim was right only while `version` came from that same file. With `--version`
// it is not, and the effect was published: `inillucent@0.1.4` went to npm pinning
// `@blackrainbowlabs/cli-*@0.1.4` while the five platform packages beside it were 0.1.3. npm skips
// an optionalDependency it cannot resolve **without a word**, so `npm install inillucent` installed
// the shim and no binary at all, and the first thing the user saw was the shim failing to find one.
const manifest = JSON.parse(readFileSync(join(wrapper, 'package.json'), 'utf8'));
manifest.version = version;
for (const platform of PLATFORMS) {
  if (manifest.optionalDependencies?.[platform.npm]) {
    manifest.optionalDependencies[platform.npm] = version;
  }
}
writeFileSync(join(wrapper, 'package.json'), `${JSON.stringify(manifest, null, 2)}${NEWLINE}`);

// A pin the release did not build is a pin npm will skip in silence, so it is a hard failure here
// rather than a broken install on somebody else's machine.
const unbuilt = PLATFORMS.filter((p) => !built.some((b) => b.endsWith(p.npm.replace('@', '').replace('/', '-'))))
  .filter((p) => manifest.optionalDependencies?.[p.npm]);
if (unbuilt.length > 0 && options.has('--publish')) {
  throw new Error(
    `the wrapper pins ${unbuilt.map((p) => p.npm).join(', ')} at ${version}, and this run did not ` +
      `stage them. Publishing would put a wrapper on npm whose binaries do not exist, which npm ` +
      `installs without complaining. Build their archives into dist/ first.`,
  );
}
console.log(`  inillucent: staged at ${version}`);

if (options.has('--pack')) {
  console.log('\npacking:');
  for (const directory of [...built, wrapper]) {
    npm(directory, ['pack', '--pack-destination', staged]);
  }
}

if (options.has('--publish')) {
  const dry = options.has('--dry-run') ? ['--dry-run'] : [];
  const otp = optionValue('--otp');
  if (otp && !/^[0-9]{6}$/.test(otp)) {
    throw new Error(`--otp is ${JSON.stringify(otp)}, which is not the six digits npm expects.`);
  }
  const second = otp ? [`--otp=${otp}`] : [];
  console.log(`\npublishing${dry.length ? ' (dry run)' : ''}${otp ? ' with a one-time code' : ''}:`);
  // Platform packages first. The wrapper pins them at an exact version, so a
  // wrapper published first is a package nobody can install until the rest
  // land - and npm's registry is eventually consistent, so "a few seconds" is
  // not a number anybody can rely on.
  for (const directory of built) {
    npm(directory, ['publish', '--access', 'public', ...dry], second);
  }
  npm(wrapper, ['publish', '--access', 'public', ...dry], second);
}

console.log(`\nstaged in ${staged}`);
if (built.length < PLATFORMS.length) {
  console.log(
    `\n${PLATFORMS.length - built.length} platform package(s) were skipped because their release\n` +
      `archives are not in dist/. Build them on their own machines with packaging/release.sh\n` +
      `and run this again before publishing a release that claims to support them.`,
  );
}

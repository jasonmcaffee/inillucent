/**
 * Asks, with no credential of any kind, whether the links this project hands a stranger resolve.
 *
 * Every package published from here names a GitHub repository. npm reads `homepage`, `repository`
 * and `bugs` out of `package.json` and puts them on the package page; PyPI does the same with
 * `[project.urls]`; the PHP installer prints one in its error message; `install.sh` prints one as
 * the "build it yourself" fallback; the Homebrew formula's `head` spec clones one; and the Go module
 * path *is* one, which is the only case where a 404 stops the package working rather than just
 * misleading somebody.
 *
 * All of them point at `Black-Rainbow-Labs/Inillucent`, which is private. So every one of them
 * answers 404 to everybody except the account that owns it, and none of it is visible from a machine
 * that is signed in - which is every machine this has ever been checked on. `go install` was
 * recorded as verified for exactly that reason: it resolved here, where git holds a credential.
 *
 * This is the check that would have caught it. It reads the URLs out of the tracked files rather
 * than out of a list kept here, fetches each one with no `Authorization` header and no cookie, and
 * reports what an anonymous reader gets. It also asks `proxy.golang.org` for the Go module, because
 * that is what `go install` asks and it answers for its own reasons.
 *
 * Usage:
 *   node tools/check-public-urls.mjs
 *   node tools/check-public-urls.mjs --json
 *
 * It exits 1 when any URL does not resolve, and 2 when the network could not be reached at all -
 * a check that cannot run must not report a pass.
 *
 * It is deliberately not wired into `tools/validate.*` yet. It fails today, on purpose, because the
 * repository is private and that is the finding; wiring a permanently red check into the gate is how
 * a gate gets ignored. The day the mirror is public this goes into validate as a network-gated
 * check, and `packaging/PUBLISHING.md` says so beside the decision it belongs to.
 */
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, '..');

const asJson = process.argv.includes('--json');

/**
 * The tracked files that carry a link a stranger is handed.
 *
 * Tracked, from git, rather than a directory walk: `packages/npm/staged/` and
 * `packages/python/build/` are build outputs holding copies of the same URLs, and counting a URL
 * three times because a build ran turns one finding into three.
 */
function surfaceFiles() {
  const tracked = execFileSync('git', ['-C', ROOT, 'ls-files'], { encoding: 'utf8' })
    .split('\n')
    .map((line) => line.trim())
    .filter(Boolean);

  const wanted = /^(README\.md|composer\.json|packaging\/(install\.sh|install\.ps1|homebrew\/.*)|packages\/(npm|python|php|go)\/.*|docs\/.*\.md|agent-skills\/.*\.md)$/;
  const excluded = /(\/staged\/|\/build\/|\/dist\/|\.egg-info\/|node_modules\/)/;

  return tracked.filter((file) => wanted.test(file) && !excluded.test(file));
}

/**
 * Every distinct https://github.com/... URL in a file, with the trailing punctuation that prose,
 * JSON and shell quoting leave on the end taken off.
 *
 * @param file - the repository-relative path to read
 */
function githubUrlsIn(file) {
  const text = fs.readFileSync(path.join(ROOT, file), 'utf8');
  const found = new Map();
  const templates = new Set();
  for (const match of text.matchAll(/https:\/\/github\.com\/[^\s"'`)\\<>]+/g)) {
    // `.git`, `#install` and `/issues` are all real parts of a URL somebody follows, so only the
    // characters that are certainly punctuation come off.
    const url = match[0].replace(/[.,;:]+$/, '');
    // `https://github.com/%s/issues` is a printf template, and the owner and repository it is
    // filled with are a constant in the same file that this check reads separately. Probing the
    // template asks github.com for a literal percent sign and gets a 400, which is a failure this
    // check invented rather than one it found.
    if (/%[a-zA-Z]|\{/.test(url)) {
      templates.add(url);
      continue;
    }
    if (!found.has(url)) found.set(url, file);
  }
  return { found, templates };
}

/**
 * The Go module path this repository publishes, read out of packages/go/go.mod.
 *
 * The module path is the one link that is not decoration: `go install` resolves it through
 * proxy.golang.org, and a module the proxy cannot read is a module nobody can install.
 */
function goModulePath() {
  const goMod = path.join(ROOT, 'packages', 'go', 'go.mod');
  if (!fs.existsSync(goMod)) return null;
  const match = fs.readFileSync(goMod, 'utf8').match(/^module\s+(\S+)/m);
  return match ? match[1] : null;
}

/**
 * The proxy.golang.org URL for a module path.
 *
 * The proxy lower-cases a path by escaping every upper-case letter as `!` followed by the
 * lower-case one, so `Black-Rainbow-Labs` becomes `!black-!rainbow-!labs`. Getting that wrong
 * produces a 404 that looks exactly like the one this check is looking for, which is why it is done
 * here rather than written out by hand.
 *
 * @param modulePath - the module path from go.mod
 * @param suffix - the proxy endpoint, for instance `@latest`
 */
function goProxyUrl(modulePath, suffix) {
  const escaped = modulePath.replace(/[A-Z]/g, (c) => `!${c.toLowerCase()}`);
  return `https://proxy.golang.org/${escaped}/${suffix}`;
}

/**
 * Fetches a URL the way a signed-out reader would, and reports what came back.
 *
 * No Authorization header, no cookie, and `redirect: follow` because a repository that moved is
 * still reachable. A network failure is reported as such rather than as a 404, because the two mean
 * opposite things: one is a broken link and the other is a check that did not run.
 *
 * @param url - the URL to fetch
 */
async function probe(url) {
  try {
    const response = await fetch(url, {
      redirect: 'follow',
      headers: { 'user-agent': 'inillucent-check-public-urls' },
      signal: AbortSignal.timeout(20000),
    });
    return { url, status: response.status, ok: response.ok };
  } catch (error) {
    return { url, status: null, ok: false, error: String(error.message || error) };
  }
}

/**
 * Checks that the network is reachable at all, so an offline run says so instead of reporting every
 * link as broken.
 */
async function networkIsReachable() {
  const result = await probe('https://github.com/');
  return result.status !== null;
}

/**
 * Collects the URLs, probes each one once, and prints a row per URL with the files that name it.
 */
async function main() {
  const urls = new Map();
  const templates = new Set();
  for (const file of surfaceFiles()) {
    const { found, templates: fileTemplates } = githubUrlsIn(file);
    for (const [url, source] of found) {
      if (!urls.has(url)) urls.set(url, []);
      urls.get(url).push(source);
    }
    for (const template of fileTemplates) templates.add(template);
  }

  const modulePath = goModulePath();
  const goChecks = [];
  if (modulePath) {
    goChecks.push(goProxyUrl(modulePath, '@v/list'), goProxyUrl(modulePath, '@latest'));
  }

  if (!(await networkIsReachable())) {
    const message = 'github.com could not be reached, so nothing here was checked.';
    if (asJson) console.log(JSON.stringify({ reachable: false, message }, null, 2));
    else console.error(message);
    process.exit(2);
  }

  const results = [];
  for (const [url, sources] of [...urls.entries()].sort()) {
    const result = await probe(url);
    results.push({ ...result, sources, kind: 'link' });
  }
  for (const url of goChecks) {
    const result = await probe(url);
    results.push({ ...result, sources: ['packages/go/go.mod'], kind: 'go module' });
  }

  const broken = results.filter((r) => !r.ok);

  if (asJson) {
    console.log(JSON.stringify({ reachable: true, module: modulePath, templates: [...templates], results }, null, 2));
  } else {
    console.log('Every link below was fetched with no credential, as a signed-out reader would.');
    console.log('');
    for (const result of results) {
      const status = result.status === null ? `error: ${result.error}` : String(result.status);
      console.log(`  ${result.ok ? 'ok  ' : 'FAIL'}  ${status.padEnd(5)}  ${result.url}`);
      if (!result.ok) {
        for (const source of result.sources) console.log(`              named by ${source}`);
      }
    }
    if (templates.size > 0) {
      console.log(`  (${templates.size} printf template(s) skipped: ${[...templates].join(', ')})`);
    }
    console.log('');
    if (broken.length === 0) {
      console.log(`${results.length} links, all of them reachable without signing in.`);
    } else {
      console.log(`${broken.length} of ${results.length} links do not resolve for a signed-out reader.`);
      const goBroken = broken.some((r) => r.kind === 'go module');
      if (goBroken) {
        console.log('');
        console.log(`The Go module is among them, and that one is not cosmetic: ${modulePath}`);
        console.log('cannot be installed by anybody, because proxy.golang.org cannot read the repository.');
      }
    }
  }

  process.exit(broken.length === 0 ? 0 : 1);
}

await main();
